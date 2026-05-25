//! Tiny CLI that mirrors `providers/claude/_write_snapshot.ts` in Rust.
//!
//! Used for the byte-compatibility check across the v3 TS and the Rust ports.
//! Invocation:
//!
//! ```sh
//! SUBCTL_STATE_DIR=... SUBCTL_INSTALL_ROOT=... \
//!     cargo run --example write_snapshot -- \
//!     --team=<id> --project-root=<dir> --mode=gated
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use evy_policy::{write_snapshot_with_local_mode, Mode};

struct Args {
    team: String,
    project_root: PathBuf,
    mode: Mode,
}

fn die(msg: &str) -> ! {
    eprintln!("write_snapshot: {msg}");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut team: Option<String> = None;
    let mut project_root: Option<PathBuf> = None;
    let mut mode: Option<Mode> = None;
    for raw in std::env::args().skip(1) {
        let Some((key, val)) = raw.split_once('=') else {
            die(&format!("unexpected argument: {raw}"));
        };
        match key {
            "--team" => team = Some(val.to_owned()),
            "--project-root" => project_root = Some(PathBuf::from(val)),
            "--mode" => {
                mode = Some(match val {
                    "trusted" => Mode::Trusted,
                    "gated" => Mode::Gated,
                    "sealed" => Mode::Sealed,
                    other => die(&format!("invalid --mode value: {other}")),
                });
            }
            other => die(&format!("unknown flag: {other}")),
        }
    }
    Args {
        team: team.unwrap_or_else(|| die("--team=<id> is required")),
        project_root: project_root.unwrap_or_else(|| die("--project-root=<dir> is required")),
        mode: mode.unwrap_or_else(|| die("--mode=<trusted|gated|sealed> is required")),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = parse_args();
    match write_snapshot_with_local_mode(&args.team, &args.project_root, args.mode).await {
        Ok(meta) => {
            let json = serde_json::to_string(&meta).expect("metadata is JSON-able");
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("write_snapshot: {e}");
            ExitCode::from(2)
        }
    }
}
