//! `evy-tui` — binary entry point.
//!
//! Thin shell over the library: parse CLI args, install the panic hook
//! that restores the terminal, set up tracing → stderr, spin up the
//! tokio runtime, and run the multiplexing loop.
//!
//! The library at `evy_tui::*` is where the actual logic lives —
//! [`evy_tui::App`] state machine, [`evy_tui::ApiClient`] HTTP/SSE
//! wrapper, [`evy_tui::render`] painter, and the
//! [`evy_tui::handle_key`] dispatcher.

use std::io::{self, Stderr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Level;

use evy_tui::{
    api::{ApiClient, DaemonEvent},
    app::{App, ConnectionState},
    input::{handle_key, KeyOutcome},
    render,
};

const RECONNECT_BACKOFF: Duration = Duration::from_secs(3);
const TICK_INTERVAL: Duration = Duration::from_millis(1000);

/// Evy v4 — ratatui operator console. Read-only TUI for a running
/// daemon's HTTP+SSE surface. Run with `--base-url` to point at a
/// specific daemon; defaults to the standard loopback address.
#[derive(Debug, Parser)]
#[command(name = "evy-tui", version, about, long_about = None)]
struct Args {
    /// Base URL the daemon's HTTP server is bound to. The TUI talks
    /// to `${BASE}/api/evy/...` and the SSE endpoint at
    /// `${BASE}/api/evy/events`.
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    base_url: String,

    /// Where to write tracing logs. Default: stderr (visible only on
    /// exit; the alt-screen hides it during normal operation, but
    /// it's intentional — operators want to see warnings after
    /// pressing `q`).
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.log_file.as_deref())?;

    let client = ApiClient::new(&args.base_url)
        .with_context(|| format!("constructing api client for {}", args.base_url))?;

    // Install panic hook BEFORE entering raw mode so a panic during
    // setup still cleans up the terminal.
    install_panic_hook();

    let mut terminal = setup_terminal().context("entering ratatui alternate screen")?;
    let result = run(&mut terminal, client, &args.base_url).await;
    let restore = restore_terminal(&mut terminal);

    // Surface either failure; prefer the run error since it's the
    // operator-meaningful one.
    if let Err(e) = result {
        let _ = restore;
        return Err(e);
    }
    restore.context("restoring terminal")?;
    Ok(())
}

fn init_tracing(log_file: Option<&std::path::Path>) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(Level::INFO.to_string()));
    if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_ansi(false)
            .try_init()
            .map_err(|e| anyhow::anyhow!("init tracing: {e}"))?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .with_ansi(false)
            .try_init()
            .map_err(|e| anyhow::anyhow!("init tracing: {e}"))?;
    }
    Ok(())
}

/// Wrap the default panic hook so a panic restores the terminal
/// before printing. Without this, a panic inside the run loop leaves
/// the operator stuck in raw mode + alternate screen — they can't
/// even read their shell prompt.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort cleanup; ignore errors because we're already
        // dying.
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen, DisableMouseCapture);
        default(info);
    }));
}

type Term = Terminal<CrosstermBackend<Stderr>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enable_raw_mode")?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen, EnableMouseCapture).context("EnterAlternateScreen")?;
    let backend = CrosstermBackend::new(stderr);
    let terminal = Terminal::new(backend).context("Terminal::new")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().context("disable_raw_mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("LeaveAlternateScreen")?;
    terminal.show_cursor().context("show_cursor")?;
    Ok(())
}

async fn run(terminal: &mut Term, client: ApiClient, base_url: &str) -> Result<()> {
    let mut app = App::new();

    // SSE consumer task — runs in the background, reconnects on
    // failure with a fixed backoff. Daemon events land on `sse_rx`.
    let (sse_tx, mut sse_rx) = mpsc::channel::<DaemonEvent>(256);
    let (conn_tx, mut conn_rx) = mpsc::channel::<ConnectionState>(8);
    let shutdown = CancellationToken::new();
    let sse_task = spawn_sse_supervisor(client.clone(), sse_tx, conn_tx, shutdown.clone());

    // Initial snapshot fetch so the first paint isn't all empty.
    refresh_snapshots(&client, &mut app).await;

    let mut key_stream = EventStream::new();
    let mut tick = tokio::time::interval(TICK_INTERVAL);

    loop {
        terminal
            .draw(|f| render(f, &app, base_url))
            .context("terminal draw")?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            // Terminal key events
            maybe_key = key_stream.next() => {
                match maybe_key {
                    Some(Ok(CtEvent::Key(key))) => {
                        match handle_key(&mut app, key) {
                            KeyOutcome::Handled => {}
                            KeyOutcome::Refresh => {
                                refresh_snapshots(&client, &mut app).await;
                            }
                            KeyOutcome::Quit => break,
                        }
                    }
                    Some(Ok(_other)) => { /* resize, mouse, etc.: ignored this slice */ }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "key event stream error");
                    }
                    None => {
                        tracing::info!("key event stream ended");
                        break;
                    }
                }
            }
            // SSE daemon events
            ev = sse_rx.recv() => {
                if let Some(ev) = ev {
                    app.push_event(ev);
                } else {
                    // Channel closed — supervisor exited. Reflect.
                    app.set_connection(ConnectionState::Disconnected {
                        reason: "stream closed".to_owned(),
                    });
                }
            }
            // SSE connection-state updates
            state = conn_rx.recv() => {
                if let Some(state) = state {
                    app.set_connection(state);
                }
            }
            // 1Hz tick (relative-time + reconnect hints)
            _ = tick.tick() => {}
        }
    }

    // Tear down the supervisor cleanly.
    shutdown.cancel();
    let _ = sse_task.await;
    Ok(())
}

/// Spawn the SSE supervisor as a Tokio task.
///
/// The supervisor reconnects on failure with a fixed backoff,
/// reporting transitions on `conn_tx` and forwarding events on
/// `sse_tx`. Exits clean when `shutdown` fires or both senders are
/// dropped (run loop ended).
fn spawn_sse_supervisor(
    client: ApiClient,
    sse_tx: mpsc::Sender<DaemonEvent>,
    conn_tx: mpsc::Sender<ConnectionState>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let _ = conn_tx.send(ConnectionState::Connecting).await;

            // Per-connection live-signal channel. `stream_events` sends
            // a unit on `live_tx` after the SSE handshake passes; this
            // helper task forwards that into a `Live` connection-state
            // emit. Bounded to 1 because we expect exactly one event.
            let (live_tx, mut live_rx) = mpsc::channel::<()>(1);
            let conn_tx_for_live = conn_tx.clone();
            let live_forwarder = tokio::spawn(async move {
                if live_rx.recv().await.is_some() {
                    let _ = conn_tx_for_live.send(ConnectionState::Live).await;
                }
            });

            client
                .stream_events(sse_tx.clone(), live_tx, shutdown.clone())
                .await;

            // The connection just dropped — abort the forwarder so it
            // doesn't keep a stale handle alive across the backoff.
            live_forwarder.abort();
            let _ = live_forwarder.await;

            if shutdown.is_cancelled() {
                return;
            }
            let _ = conn_tx
                .send(ConnectionState::Disconnected {
                    reason: format!("retrying in {}s", RECONNECT_BACKOFF.as_secs()),
                })
                .await;
            // Backoff, but exit immediately if shutdown is signaled
            // during the wait.
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(RECONNECT_BACKOFF) => {}
            }
        }
    })
}

async fn refresh_snapshots(client: &ApiClient, app: &mut App) {
    match client.fetch_workers().await {
        Ok(ws) => app.set_workers(ws),
        Err(e) => tracing::warn!(error = %e, "fetch workers failed"),
    }
    match client.fetch_jobs().await {
        Ok(js) => app.set_jobs(js),
        Err(e) => tracing::warn!(error = %e, "fetch jobs failed"),
    }
    match client.fetch_policy().await {
        Ok(p) => app.set_policy(p),
        Err(e) => tracing::warn!(error = %e, "fetch policy failed"),
    }
}
