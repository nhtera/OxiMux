use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
#[cfg(unix)]
use oximux_relay::host_lookup::{self, Identity, LookupWatch, Verdict};
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
    let mut checkpoint_dir: Option<PathBuf> = None;
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
            "--checkpoint-dir" => {
                checkpoint_dir =
                    Some(args.next().context("--checkpoint-dir expects a path")?.into());
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
    // Checkpointing defaults ON, rooted next to the socket — the
    // supervisor that spawns this daemon doesn't need to know the flag
    // exists. An explicit `--checkpoint-dir` overrides.
    let checkpoint_dir = checkpoint_dir.or_else(|| {
        socket
            .parent()
            .map(|runtime_dir| runtime_dir.join("checkpoints"))
    });
    Ok(ParsedArgs {
        cfg: ServerConfig {
            socket_path: socket,
            token_file,
            pid_path: pid_file,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            idle_tick_interval: None,
            checkpoint_dir,
            checkpoint_tick_interval: None,
        },
        log_dir,
    })
}

fn print_help() {
    println!(
        "oximux-relay — PTY-owning daemon for OxiMux\n\
         \n\
         USAGE:\n    oximux-relay --socket <path> --token <path> [--pid-file <path>] [--log-dir <path>] [--checkpoint-dir <path>]\n\
         \n\
         FLAGS:\n  --socket   <path>   unix-domain socket to bind\n  --token    <path>   token file (0600) for client auth\n  --pid-file <path>   write own PID for supervisor liveness probes\n  --log-dir  <path>   write daily-rotated JSON logs to this directory\n  --checkpoint-dir <path>  disk scrollback checkpoints root (default: <socket dir>/checkpoints)"
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
    let stderr_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

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
    let oslog_layer = Some(tracing_oslog::OsLogger::new("dev.nhtera.oximux", "relay"));
    // `Identity` rather than a concrete `fmt::Layer`: this sits partway up a
    // `Layered` stack, and naming `Layer<Registry>` pins the subscriber type to
    // the bottom of that stack instead of wherever it actually composes.
    #[cfg(not(target_os = "macos"))]
    let oslog_layer: Option<tracing_subscriber::layer::Identity> = None;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .with(oslog_layer)
        .init();
    guard
}

// How often to re-ask the OS who we are. Slow on purpose: the condition being
// watched for is permanent once it happens, so the only thing frequency buys is
// how long a broken daemon keeps handing broken PTYs out — and the cost of
// getting it wrong is every terminal pane the user has open.
#[cfg(unix)]
const HOST_LOOKUP_PROBE_INTERVAL: Duration = Duration::from_secs(30);
// Three in a row, so a probe that lands mid-`opendirectoryd`-restart cannot
// cost the user a session. With the interval above, a genuinely dead lookup
// path is acted on inside ~90s.
#[cfg(unix)]
const HOST_LOOKUP_FAILURES_BEFORE_FATAL: u32 = 3;
// A daemon broken since boot never exits — it would only be replaced by an
// equally broken one — so it repeats itself instead, hourly at the interval
// above. Logs rotate daily and this daemon can outlive many rotations; without
// the repeat, the live log is empty for whoever eventually goes looking.
#[cfg(unix)]
const HOST_LOOKUP_DEGRADED_REREPORT_EVERY: u32 = 120;
// A dead lookup path is only assumed to fail, not to fail *fast*: a stuck mach
// message would park the probe indefinitely, and a blocking task cannot be
// aborted. Elapsed time therefore counts as a failed probe.
#[cfg(unix)]
const HOST_LOOKUP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
// Bound on how long a wedged blocking task may hold the process open after the
// server stops. Dropping a runtime waits for blocking tasks to finish, and the
// one thing this daemon puts on that pool is a probe that may hang.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// Probe once at boot and record the answer, so `relay.log` carries it even when
// nothing ever goes wrong. A daemon that cannot ask the OS who anything is will
// hand that same incapacity to every PTY it spawns, and the symptom the user
// actually sees — a pane that cannot resolve a hostname, or a prompt missing
// its username — points nowhere near here.
//
// The returned health is not incidental to the logging: it seeds the watcher's
// state machine, and so decides whether this daemon is ever allowed to exit.
#[cfg(unix)]
fn probe_and_log_at_boot() -> bool {
    match host_lookup::probe() {
        Identity::Resolved(user) => {
            tracing::info!(user = %user, "host lookup ok");
            true
        }
        Identity::Unresolvable => {
            tracing::error!(
                "host lookup FAILED at boot: getpwuid() has no answer for our own uid. \
                 Terminals spawned by this daemon will not resolve hostnames, users or \
                 keychain items. The process that spawned this daemon is in the same \
                 state, so respawning the daemon alone will not clear it."
            );
            false
        }
    }
}

// Re-probe on a slow tick and exit if a daemon that *was* healthy stops being
// so. Exiting is the fix: the host's heartbeat respawns one from its own
// context, which — when the app is healthy and only the long-lived daemon has
// rotted — is exactly the context that works.
#[cfg(unix)]
async fn watch_host_lookup(boot_healthy: bool) {
    let mut watch = LookupWatch::new(
        HOST_LOOKUP_FAILURES_BEFORE_FATAL,
        HOST_LOOKUP_DEGRADED_REREPORT_EVERY,
    );
    // The boot probe already ran and was already logged. Folding it in seeds
    // the healthy/degraded state and stops the first tick, which fires
    // immediately, from reporting the same thing twice.
    let _ = watch.observe(boot_healthy);

    let mut ticker = tokio::time::interval(HOST_LOOKUP_PROBE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        // Off the runtime worker, and bounded. A join error says nothing about
        // the OS, so it counts as healthy — this task must never itself be the
        // reason a working daemon dies. A probe that never returns is a
        // different matter: it is indistinguishable from one that failed, and
        // is treated as one.
        let probe = tokio::task::spawn_blocking(host_lookup::is_healthy);
        let healthy = match tokio::time::timeout(HOST_LOOKUP_PROBE_TIMEOUT, probe).await {
            Ok(Ok(healthy)) => healthy,
            Ok(Err(_join_error)) => true,
            Err(_elapsed) => false,
        };

        let verdict = watch.observe(healthy);
        match &verdict {
            Verdict::Quiet => {}
            Verdict::Recovered => tracing::info!("host lookup recovered"),
            Verdict::Flaky { consecutive } => {
                tracing::warn!(consecutive, "host lookup probe failed; watching");
            }
            // Repeats on a cadence rather than firing once, so the reason stays
            // in the live log however many rotations this daemon outlives.
            Verdict::BornDegraded => {
                tracing::error!("host lookup still unavailable, and has been since boot");
            }
            Verdict::Died { consecutive } => {
                tracing::error!(
                    consecutive,
                    "host lookup died: getpwuid() stopped resolving our own uid, so every \
                     PTY this daemon owns has lost hostname, user and keychain resolution. \
                     Shutting down so the host respawns a daemon from its own context."
                );
            }
        }

        if verdict.is_fatal() {
            // SIGTERM, not `process::exit`: the server's own handler drains the
            // accept loop and takes a final checkpoint pass, so the next launch
            // cold-restores the freshest scrollback rather than whatever the
            // 5s tick last happened to write. Returning through `main` also
            // lets the log appender's guard flush on drop — an exit runs no
            // destructors, which would put the line explaining this shutdown
            // among the most likely to be lost.
            if let Err(err) = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM) {
                tracing::error!(?err, "raising SIGTERM failed; exiting without a flush");
                std::process::exit(1);
            }
            return;
        }
    }
}

// The session-marker scrub lives in `oximux-shell-env` (one list, three
// consumers: the desktop app, this daemon, and `oximux serve`). The daemon
// inherits the markers when the app that spawned it was itself launched from
// inside a Claude Code session, and passes them on to every terminal PTY —
// where a spawned `claude` then treats itself as a nested child session and
// disables transcript saving.
use oximux_shell_env::scrub_inherited_claude_session_markers;

// A sync `main` (not `#[tokio::main]`) so the environment scrub runs before
// the runtime's worker threads exist — env mutation is only sound while the
// process is single-threaded.
fn main() -> Result<()> {
    let dropped_markers = scrub_inherited_claude_session_markers();

    let parsed = parse_args()?;
    let _log_guard = init_tracing(parsed.log_dir.as_deref());
    for marker in dropped_markers {
        tracing::info!(marker, "dropped an inherited Claude Code session marker");
    }
    tracing::info!(
        socket = %parsed.cfg.socket_path.display(),
        log_dir = ?parsed.log_dir,
        "starting oximux-relay"
    );
    // Before the runtime exists, so the probe runs on a plain thread and its
    // line is in the log ahead of anything the server says.
    #[cfg(unix)]
    let boot_healthy = probe_and_log_at_boot();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let result = runtime.block_on(async move {
        #[cfg(unix)]
        tokio::spawn(watch_host_lookup(boot_healthy));
        run_server(parsed.cfg).await
    });
    // Bounded, because dropping a runtime waits for its blocking tasks: a probe
    // wedged in a dead lookup path would otherwise hold the process open in
    // exactly the state this watch exists to end, with the host's liveness
    // heartbeat seeing a live pid and never respawning.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    result
}
