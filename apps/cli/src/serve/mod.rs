//! `oximux serve` — the same binary as a headless host.
//!
//! The stack is the desktop's minus every view: the remote-host dispatcher
//! over the shared session registry, the relay daemon for surviving
//! terminals, SQLite for sessions/devices/transcripts, the owner-only local
//! socket for the CLI on the same box, and the iroh endpoint for paired
//! devices. Because it shares the desktop's data directory, identity, and
//! device store by default, a phone paired with the desktop reaches serve
//! without re-pairing, and a session created under either host is visible
//! from the other.
//!
//! Output contract: stdout carries exactly one line — the versioned readiness
//! JSON — and never anything else (no secret can end up in a journal that
//! captures stdout). Logs go to stderr.

mod blob;
mod catalog;
mod launcher;
mod projects;
mod pump;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{AuthStore, Dispatcher, HostIdentity, LocalScope, StorageDeviceStore};
use oximux_remote_local::{LocalClaim, LocalControlListener};

/// The identity scope the desktop uses — shared on purpose, so serve binds
/// the same endpoint id the desktop's pairings already dial.
const HOST_IDENTITY_SCOPE: &str = "remote-control-host";

/// How long a drain waits for in-flight turns before marking the stragglers
/// interrupted. The systemd unit's `TimeoutStopSec` must exceed this.
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

pub struct ServeArgs {
    pub data_dir: Option<PathBuf>,
    pub projects: Vec<PathBuf>,
}

/// Boot and run until a shutdown signal, then drain. The process exit code is
/// the verb's result: 0 after a clean drain, 1 on a boot failure.
pub fn run(args: ServeArgs) -> u8 {
    // Before any thread exists: an inherited Claude session marker would
    // switch transcript saving off in every agent this host ever spawns.
    for marker in oximux_shell_env::scrub_inherited_claude_session_markers() {
        eprintln!("serve: dropped inherited Claude Code session marker {marker}");
    }
    // Stdout is the readiness contract; everything human goes to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("serve: could not start the async runtime: {err}");
            return 1;
        }
    };
    match rt.block_on(serve(args)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("serve: {err:#}");
            1
        }
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // ---- data root, hardened before anything writes into it ----
    let data_dir = match args.data_dir {
        Some(dir) => dir,
        None => oximux_remote_local::default_runtime_dir()
            .context("this platform reports no local data directory; pass --data-dir")?,
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;
    // A server is the deployment where other accounts on the box are most
    // likely to exist, so this matters more here than on the desktop.
    oximux_owner_only::prepare_owner_only_dir(&data_dir)
        .with_context(|| format!("restrict data dir {}", data_dir.display()))?;

    // ---- storage (the desktop's own database and migration ladder) ----
    let db_path = data_dir.join("oximux.db");
    let db = oximux_storage::open(&db_path)
        .with_context(|| format!("open database {}", db_path.display()))?;
    // The directory descriptor alone does not protect files inside it on
    // Windows; restrict the database and its WAL sidecars explicitly.
    for name in ["oximux.db", "oximux.db-wal", "oximux.db-shm"] {
        let path = data_dir.join(name);
        if path.exists()
            && let Err(err) = oximux_owner_only::restrict_file(&path)
        {
            tracing::warn!(%err, file = %path.display(), "could not restrict database file");
        }
    }
    let settings = oximux_storage::SettingsRepo::new(db.clone());
    let device_repo = oximux_storage::RemoteDeviceRepo::new(db.clone());

    // ---- relay daemon (terminals that survive restarts) ----
    // Best-effort: a host without a relay serves everything except terminals.
    let relay = {
        let supervisor = oximux_relay_supervisor::RelaySupervisor::new(
            data_dir.clone(),
            data_dir.join("logs"),
        );
        match supervisor.ensure_running().await {
            Ok(client) => Some(Arc::new(client)),
            Err(err) => {
                tracing::warn!(%err, "relay unavailable; terminals will not be served");
                None
            }
        }
    };

    // ---- identity + auth (shared with the desktop) ----
    let identity = HostIdentity::load_or_generate(&data_dir, HOST_IDENTITY_SCOPE)
        .context("load host identity")?;
    let endpoint_secret = identity.transport_secret_bytes();
    let endpoint_id = oximux_remote_iroh::endpoint_id_of(&endpoint_secret);
    let auth = Arc::new(AuthStore::with_store(Arc::new(StorageDeviceStore::new(device_repo))));

    // ---- registry + headless seams + dispatcher ----
    let registry = Arc::new(SessionRegistry::new());
    let pumps = pump::PumpSet::new();
    let draining = Arc::new(AtomicBool::new(false));
    let launcher = Arc::new(launcher::HeadlessLauncher::new(
        registry.clone(),
        settings.clone(),
        pumps.clone(),
        draining.clone(),
    ));
    let catalog = Arc::new(catalog::HeadlessCatalog::scan(
        registry.clone(),
        settings.clone(),
        pumps.clone(),
        draining.clone(),
    ));
    let provider = Arc::new(projects::StaticProjects::load(args.projects, &data_dir));
    let mut dispatcher = Dispatcher::new(registry.clone(), auth.clone())
        .with_launcher(launcher)
        .with_catalog(catalog)
        .with_projects(provider)
        .with_pairing_endpoint(endpoint_id);
    if let Some(relay) = relay {
        dispatcher =
            dispatcher.with_terminals(Arc::new(oximux_relay_terminals::RelayTerminals::new(relay)));
    }
    // No transcriber, no rewinder, no worktree service: each of those RPCs
    // answers `Unsupported` (or its documented refusal) rather than
    // pretending. Schedules arrive with the phase-5 single-owner lock.
    let dispatcher = Arc::new(dispatcher);

    // ---- local socket (the CLI on this box) ----
    let local = start_local_listener(dispatcher.clone(), data_dir.clone())
        .context("bind the local control socket (is another OxiMux host already serving here?)")?;

    // ---- iroh endpoint (paired devices) ----
    // Bound in the background: `start_host` waits until the endpoint is
    // relay-reachable, which can take a while (or never resolve, air-gapped),
    // and neither the local socket nor the readiness contract depends on it.
    // The endpoint id is derived from the persisted secret, so pairing
    // tickets name the right endpoint even while the bind is in flight. The
    // boot secret seeds no pairing slot, so it redeems nothing; `pair-new`
    // opens real windows at runtime.
    let host: Arc<tokio::sync::Mutex<Option<oximux_remote_iroh::HostHandle>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    {
        let dispatcher = dispatcher.clone();
        let host = host.clone();
        tokio::spawn(async move {
            match oximux_remote_iroh::start_host(
                dispatcher,
                oximux_remote_host::mint_pairing_secret(),
                Some(endpoint_secret),
            )
            .await
            {
                Ok(handle) => {
                    tracing::info!("remote endpoint online");
                    *host.lock().await = Some(handle);
                }
                Err(err) => {
                    tracing::warn!(%err, "remote endpoint failed to bind; serving locally only");
                }
            }
        });
    }

    // ---- readiness: the one stdout line ----
    let endpoint_hex: String = endpoint_id.iter().map(|b| format!("{b:02x}")).collect();
    println!(
        "{}",
        serde_json::json!({
            "type": "oximux_serve_ready",
            "schemaVersion": 1,
            "protocolVersion": oximux_remote_proto::proto::PROTOCOL_VERSION,
            "localSocket": data_dir.to_string_lossy(),
            "endpointId": endpoint_hex,
        })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    tracing::info!(data_dir = %data_dir.display(), endpoint = %endpoint_hex, "serve ready");

    // ---- run until asked to stop ----
    wait_for_shutdown().await;
    tracing::info!("shutdown requested; draining");

    // ---- drain: stop taking work, let in-flight turns finish, persist ----
    draining.store(true, Ordering::SeqCst);
    drop(local); // cuts the local listener and every CLI connection
    let bound_host = host.lock().await.take();
    if let Some(mut handle) = bound_host {
        handle.shutdown(); // stops accepting remote connections
        handle.join().await;
    }
    let deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
    while pumps.active_turns() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let stragglers = pumps.active_turns();
    if stragglers > 0 {
        tracing::warn!(stragglers, "drain deadline reached; marking in-flight turns interrupted");
    }
    // Tell every pump to finalize (interrupted turns are marked, transcripts
    // written), then give the writes a moment to land.
    pumps.finalize_all();
    let flush_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while pumps.live_pumps() > 0 && tokio::time::Instant::now() < flush_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tracing::info!("drained; exiting");
    Ok(())
}

/// The local accept loop — the same shape the desktop's listener runs.
/// Dropping the handle aborts the loop, closes the socket, and cuts every
/// in-flight CLI connection.
struct LocalHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LocalHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start_local_listener(
    dispatcher: Arc<Dispatcher>,
    runtime_dir: PathBuf,
) -> anyhow::Result<LocalHandle> {
    let token = oximux_remote_local::generate_token();
    let listener = Arc::new(LocalControlListener::bind(&runtime_dir, &token)?);
    let task = tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept_pending() => match accepted {
                    Ok(pending) => {
                        let dispatcher = dispatcher.clone();
                        // Authenticate inside the per-connection task: a
                        // caller that connects and says nothing must not hold
                        // the accept path.
                        conns.spawn(async move {
                            let Ok((transport, claim)) = pending.authenticate().await else {
                                return;
                            };
                            let scope = match claim {
                                LocalClaim::Operator => LocalScope::Full,
                                LocalClaim::Session(id) => LocalScope::Session(id.into()),
                            };
                            dispatcher.serve_local(transport.as_ref(), scope).await;
                        });
                    }
                    Err(err) => tracing::debug!(%err, "local control accept failed"),
                },
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    });
    Ok(LocalHandle { task })
}

/// Resolve when the platform asks this process to stop.
#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// On Windows the console close and OS shutdown notices are the SIGTERM
/// analogue; a Scheduled-Task stop delivers ctrl-close.
#[cfg(windows)]
async fn wait_for_shutdown() {
    use tokio::signal::windows;
    let mut ctrl_c = windows::ctrl_c().expect("install ctrl-c handler");
    let mut ctrl_close = windows::ctrl_close().expect("install ctrl-close handler");
    let mut ctrl_shutdown = windows::ctrl_shutdown().expect("install ctrl-shutdown handler");
    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_close.recv() => {}
        _ = ctrl_shutdown.recv() => {}
    }
}
