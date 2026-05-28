//! `evy-chat` binary — terminal chat client for the Evy v4 daemon.
//!
//! Connects to the daemon's `POST /api/evy/chat` endpoint and gives the
//! operator a two-pane ratatui surface: scrollback + multi-line input.
//!
//! Run examples:
//!
//! ```text
//! evy-chat                                  # localhost:8787 default
//! evy-chat --base-url http://127.0.0.1:8797 # v4 dev daemon
//! evy-chat --log-file /tmp/evy-chat.log     # file logging
//! ```

use std::io::{self, Stderr};
use std::path::PathBuf;

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
use tracing::Level;

use evy_chat_tui::{
    api::{ApiClient, ApiError, ChatResponse},
    app::{App, Status},
    input::{handle_key, KeyOutcome},
    render,
};

/// Evy v4 — terminal chat client. Talks to a running daemon over
/// `POST /api/evy/chat`.
#[derive(Debug, Parser)]
#[command(name = "evy-chat", version, about, long_about = None)]
struct Args {
    /// Daemon base URL.
    #[arg(long, default_value = "http://127.0.0.1:8787")]
    base_url: String,

    /// Path to write tracing logs (default: stderr, hidden by the alt
    /// screen until exit).
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.log_file.as_deref())?;

    let client = ApiClient::new(&args.base_url)
        .with_context(|| format!("constructing chat client for {}", args.base_url))?;

    install_panic_hook();

    let mut terminal = setup_terminal().context("entering ratatui alternate screen")?;
    let result = run(&mut terminal, client, &args.base_url).await;
    let restore = restore_terminal(&mut terminal);
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

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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

/// Result of one POST /api/evy/chat call, routed back through `result_rx`.
enum ChatTaskResult {
    Ok(ChatResponse),
    Err(ApiError),
}

async fn run(terminal: &mut Term, client: ApiClient, base_url: &str) -> Result<()> {
    let mut app = App::new();
    app.push_system(format!(
        "connected to {base_url} — type a message and press Enter"
    ));
    app.push_system("type /help to see slash commands");

    let mut key_stream = EventStream::new();
    let (result_tx, mut result_rx) = mpsc::channel::<ChatTaskResult>(8);
    let mut in_flight = false;

    loop {
        terminal
            .draw(|f| render(f, &app, base_url))
            .context("terminal draw")?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_key = key_stream.next() => {
                match maybe_key {
                    Some(Ok(CtEvent::Key(key))) => {
                        match handle_key(&mut app, key) {
                            KeyOutcome::Handled => {}
                            KeyOutcome::Quit => break,
                            KeyOutcome::Submit { message } => {
                                if in_flight {
                                    // The Sending status got set by handle_key —
                                    // re-set it back so the operator sees the prior
                                    // turn is still in flight. Drop the new turn.
                                    app.push_error(
                                        "previous turn still in flight; dropped this one"
                                    );
                                    continue;
                                }
                                in_flight = true;
                                let session_id = app.session_id;
                                let client_clone = client.clone();
                                let tx = result_tx.clone();
                                tokio::spawn(async move {
                                    let res = client_clone.send(session_id, message).await;
                                    let _ = tx.send(match res {
                                        Ok(r) => ChatTaskResult::Ok(r),
                                        Err(e) => ChatTaskResult::Err(e),
                                    }).await;
                                });
                            }
                        }
                    }
                    Some(Ok(_other)) => {}
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "key stream error");
                    }
                    None => {
                        tracing::info!("key stream ended");
                        break;
                    }
                }
            }
            // HTTP result lands here once the spawned task finishes.
            maybe_result = result_rx.recv() => {
                if let Some(result) = maybe_result {
                    in_flight = false;
                    match result {
                        ChatTaskResult::Ok(resp) => {
                            app.session_id = Some(resp.session_id);
                            app.push_partner_with_skills(&resp.response, &resp.skills_loaded);
                            app.status = Status::Idle;
                        }
                        ChatTaskResult::Err(e) => {
                            app.push_error(format!("daemon error: {e}"));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
