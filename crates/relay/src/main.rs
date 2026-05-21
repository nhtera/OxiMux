use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use oximux_relay::{DEFAULT_IDLE_TIMEOUT, ServerConfig, run_server};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

struct ParsedArgs {
    cfg: ServerConfig,
    log_dir: Option<PathBuf>,
}

// Minimal arg parsing — no clap dep. The relay is invoked by the app
// (or launchd) with a fixed flag set, so a hand-written parser keeps
// boot-time deps small and the dependency graph predictable.
fn parse_args() -> Result<ParsedArgs> {
    let mut socket: Option<PathBuf> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut pid_file: Option<PathBuf> = None;
    let mut log_dir: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = Some(args.next().context("--socket expects a path")?.into()),
            "--token" => {
                token_file = Some(args.next().context("--token expects a path")?.into());
            }
            "--pid-file" => {
                pid_file = Some(args.next().context("--pid-file expects a path")?.into());
            }
            "--log-dir" => {
                log_dir = Some(args.next().context("--log-dir expects a path")?.into());
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
    Ok(ParsedArgs {
        cfg: ServerConfig {
            socket_path: socket,
            token_file,
            pid_path: pid_file,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            idle_tick_interval: None,
        },
        log_dir,
    })
}

fn print_help() {
    println!(
        "oximux-relay — PTY-owning daemon for OxiMux\n\
         \n\
         USAGE:\n    oximux-relay --socket <path> --token <path> [--pid-file <path>] [--log-dir <path>]\n\
         \n\
         FLAGS:\n  --socket   <path>   unix-domain socket to bind\n  --token    <path>   token file (0600) for client auth\n  --pid-file <path>   write own PID for supervisor liveness probes\n  --log-dir  <path>   write daily-rotated JSON logs to this directory"
    );
}

// Compose the env filter. Precedence: explicit `OXIMUX_RELAY_LOG` wins;
// otherwise `OXIMUX_RELAY_TRACE=1` opens the per-byte trace path; bare
// default is `info` so daily logs stay readable under load (the plan
// budgets 86 GiB/day worst case if everything traces at byte level).
fn compose_env_filter() -> EnvFilter {
    if let Ok(f) = EnvFilter::try_from_env("OXIMUX_RELAY_LOG") {
        return f;
    }
    let trace_on = std::env::var("OXIMUX_RELAY_TRACE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false);
    if trace_on {
        EnvFilter::new("trace")
    } else {
        EnvFilter::new("info")
    }
}

// Delete `relay.log.YYYY-MM-DD` files older than the retention window.
// Best-effort: any IO error during sweep just gets logged once and the
// daemon proceeds. Files that don't match the daily-rotation suffix
// pattern are left alone.
fn purge_old_logs(dir: &Path, retain: Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("relay.log.") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age > retain {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// Build the subscriber stack. Returns the worker guard for the
// non-blocking file appender — drop on process exit flushes pending
// records.
fn init_tracing(log_dir: Option<&Path>) -> Option<WorkerGuard> {
    let env_filter = compose_env_filter();
    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);

    let (file_layer, guard) = if let Some(dir) = log_dir {
        let _ = std::fs::create_dir_all(dir);
        purge_old_logs(dir, Duration::from_secs(60 * 60 * 24 * 7));
        let appender = tracing_appender::rolling::daily(dir, "relay.log");
        let (writer, worker_guard) = tracing_appender::non_blocking(appender);
        let layer = fmt::layer().json().with_target(true).with_writer(writer);
        (Some(layer), Some(worker_guard))
    } else {
        (None, None)
    };

    #[cfg(target_os = "macos")]
    let oslog_layer = Some(tracing_oslog::OsLogger::new(
        "dev.nhtera.oximux",
        "relay",
    ));
    #[cfg(not(target_os = "macos"))]
    let oslog_layer: Option<fmt::Layer<tracing_subscriber::Registry>> = None;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .with(oslog_layer)
        .init();
    guard
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let parsed = parse_args()?;
    let _log_guard = init_tracing(parsed.log_dir.as_deref());
    tracing::info!(
        socket = %parsed.cfg.socket_path.display(),
        log_dir = ?parsed.log_dir,
        "starting oximux-relay"
    );
    run_server(parsed.cfg).await
}
