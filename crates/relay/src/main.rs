use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use oximux_relay::{ServerConfig, run_server};
use tracing_subscriber::EnvFilter;

// Minimal arg parsing — no clap dep. The relay is invoked by the app
// (or launchd) with a fixed flag set, so a hand-written parser keeps
// boot-time deps small and the dependency graph predictable.
fn parse_args() -> Result<ServerConfig> {
    let mut socket: Option<PathBuf> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = Some(args.next().context("--socket expects a path")?.into()),
            "--token" => {
                token_file = Some(args.next().context("--token expects a path")?.into());
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let socket = socket.context("--socket <path> is required")?;
    let token_file = token_file.context("--token <path> is required")?;
    Ok(ServerConfig {
        socket_path: socket,
        token_file,
    })
}

fn print_help() {
    println!(
        "oximux-relay — PTY-owning daemon for OxiMux\n\
         \n\
         USAGE:\n    oximux-relay --socket <path> --token <path>\n\
         \n\
         FLAGS:\n  --socket <path>   unix-domain socket to bind\n  --token  <path>   token file (0600) for client auth"
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("OXIMUX_RELAY_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let cfg = parse_args()?;
    tracing::info!(socket = %cfg.socket_path.display(), "starting oximux-relay");
    run_server(cfg).await
}
