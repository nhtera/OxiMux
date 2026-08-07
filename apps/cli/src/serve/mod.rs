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
mod scheduler;
#[cfg(windows)]
pub mod service_windows;

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

/// The exit code for "another host already holds this data directory" —
/// distinct from the generic boot failure (1) because it is the one refusal a
/// supervisor must NOT blindly retry: the incumbent is healthy, and a restart
/// loop would hammer it forever. A systemd unit excludes it with
/// `RestartPreventExitStatus=6` (see `docs/server-install.md`). Outside the
/// client verbs' 0–5 taxonomy on purpose, so the two never collide.
pub const EXIT_HELD_BY_ANOTHER_HOST: u8 = 6;

/// The typed form of that refusal, so `run_with_shutdown` can map it to
/// [`EXIT_HELD_BY_ANOTHER_HOST`] by downcast rather than by matching message
/// text.
#[derive(Debug)]
struct HeldByAnotherHost {
    data_dir: std::path::PathBuf,
    holder_pid: Option<u32>,
}

impl std::fmt::Display for HeldByAnotherHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let who = self.holder_pid.map(|pid| format!(" (PID {pid})")).unwrap_or_default();
        write!(
            f,
            "another OxiMux host is already serving {}{who}. Stop it first, \
             or use a different --data-dir.",
            self.data_dir.display()
        )
    }
}

impl std::error::Error for HeldByAnotherHost {}

/// Boot and run until a shutdown signal, then drain. The process exit code is
/// the verb's result: 0 after a clean drain, 1 on a boot failure — except the
/// held-data-dir refusal, which exits [`EXIT_HELD_BY_ANOTHER_HOST`].
pub fn run(args: ServeArgs) -> u8 {
    run_with_shutdown(args, None)
}

/// Like [`run`], with the shutdown signal injectable: `None` waits on the
/// platform's console/OS notifications; `Some` waits on the channel instead —
/// the SCM service wrapper's control handler fires it on `SERVICE_CONTROL_STOP`
/// so a service stop drains exactly as a console signal does.
pub(crate) fn run_with_shutdown(
    args: ServeArgs,
    external_shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> u8 {
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
    match rt.block_on(serve(args, external_shutdown)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("serve: {err:#}");
            if err.downcast_ref::<HeldByAnotherHost>().is_some() {
                EXIT_HELD_BY_ANOTHER_HOST
            } else {
                1
            }
        }
    }
}

async fn serve(
    args: ServeArgs,
    external_shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // ---- data root, hardened before anything writes into it ----
    let data_dir = match args.data_dir {
        Some(dir) => dir,
        None => oximux_remote_local::default_runtime_dir()
            .context("this platform reports no local data directory; pass --data-dir")?,
    };
    // Asked before the directory is even created. This boot may be impossible
    // on unix — a data dir long enough to push `control-v1.sock` past
    // `sockaddr_un.sun_path` cannot be served at all — and the bind that used to
    // discover it sits behind the database, the identity key and a detached
    // relay spawn. That refused ~5 s in and left a migrated 192 KB database, a
    // token and a host key behind, none of which the answer depended on.
    oximux_remote_local::check_data_dir(&data_dir)?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;
    // A server is the deployment where other accounts on the box are most
    // likely to exist, so this matters more here than on the desktop.
    oximux_owner_only::prepare_owner_only_dir(&data_dir)
        .with_context(|| format!("restrict data dir {}", data_dir.display()))?;

    // ---- single holder, decided before anything else is written ----
    //
    // `docs/server-install.md` promises one host per data directory, and the
    // rest of this function assumes it: the bind is where each agent's confined
    // credential is minted from, and two hosts would run two session registries
    // over one database.
    //
    // Checked here rather than at the bind because *where* matters as much as
    // whether. `LocalControlListener::bind` writes the token file on its way to
    // the socket, so a second boot that got that far — and every Windows one
    // did, since a pipe name in use cannot be taken over — replaced the
    // credential the incumbent's live listener authenticates against. The first
    // host stayed up and answering while every client it had was denied. The
    // refusal has to land before the first destructive step, not at the last
    // one.
    //
    // And it has to be a lock rather than a probe of the socket: one process
    // rebinding over its own live listener is legitimate (the desktop's
    // Settings → Remote toggle does exactly that), and on unix that is
    // indistinguishable from a second host by looking at the socket alone.
    let _host_lock = match oximux_single_instance::try_acquire(
        &data_dir.join(oximux_remote_local::HOST_LOCK_FILENAME),
    ) {
        Ok(oximux_single_instance::AcquireOutcome::Acquired(guard)) => guard,
        Ok(oximux_single_instance::AcquireOutcome::AlreadyRunning { holder_pid }) => {
            return Err(anyhow::Error::new(HeldByAnotherHost {
                data_dir: data_dir.clone(),
                holder_pid,
            }));
        }
        // A lock that cannot be evaluated must not silently authorise a second
        // host — the whole point is that the incumbent survives contact with
        // one.
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "could not determine whether another host holds {}",
                data_dir.display()
            )));
        }
    };

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

    // ---- local socket, bound before anything can spawn an agent ----
    // Bound this early because the launcher mints each agent's confined
    // credential from it: an agent spawned before the socket existed would have
    // no credential to be confined by, and would fall back to the operator
    // path. Binding is also the single-host check, so failing here fails fast.
    // The accept loop starts later, once the dispatcher exists to serve it.
    // The context asserts no cause. It used to read "(is another OxiMux host
    // already serving here?)", which leads every reader toward a conflicting
    // process — wrong, and expensively so, for the path-too-long case, where
    // the fix is a shorter `--data-dir`. The underlying errors already say
    // which it is: "Address already in use" for a real conflict, and an
    // explicit path-length refusal for the other.
    let local_listener = Arc::new(
        LocalControlListener::bind(&data_dir, &oximux_remote_local::generate_token())
            .context("bind the local control socket")?,
    );

    // ---- registry + headless seams + dispatcher ----
    let registry = Arc::new(SessionRegistry::new());
    let pumps = pump::PumpSet::new();
    let draining = Arc::new(AtomicBool::new(false));
    // One index, three writers: the boot scan, the launcher, and every pump.
    let index = Arc::new(catalog::SessionIndex::default());
    let launcher = Arc::new(launcher::HeadlessLauncher::new(
        registry.clone(),
        settings.clone(),
        pumps.clone(),
        draining.clone(),
        index.clone(),
        local_listener.clone(),
    ));
    let catalog = Arc::new(catalog::HeadlessCatalog::scan(
        registry.clone(),
        settings.clone(),
        pumps.clone(),
        draining.clone(),
        index,
        local_listener.clone(),
    ));
    let provider = Arc::new(projects::StaticProjects::load(args.projects, &data_dir));

    // ---- worktrees ----
    // Creating one resolves a client-named root to a `projects` row, so the
    // configured roots are registered as rows before the service is wired.
    // `insert_or_touch` is idempotent, which is what makes this safe to run on
    // every boot and safe against the desktop having already added the same
    // path — a shared data dir means both hosts see one set of projects, and a
    // worktree either creates is the row the other lists.
    let project_repo = oximux_storage::ProjectRepo::new(db.clone());
    for root in provider.roots() {
        let path = root.to_string_lossy();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone().into_owned());
        // `main` matches what the desktop's own project picker records; the
        // real HEAD is read from git when a worktree is actually branched.
        if let Err(err) = project_repo.insert_or_touch(&name, &path, "main") {
            tracing::warn!(%err, path = %path, "could not register project root; worktrees there will be refused");
        }
    }
    let worktrees = Arc::new(oximux_worktree_ops::RepoWorktrees::new(
        project_repo,
        oximux_storage::WorkspaceRepo::new(db.clone()),
        data_dir.clone(),
    ));

    // ---- schedules: reads/writes always; the TICKER only if we own it ----
    // The advisory role lock decides which process fires this data dir's
    // schedules — on a box also running the desktop app, whoever booted first.
    // The loser keeps serving every schedule read and write; run-now answers
    // `Unsupported` there, naming the honest reason.
    let schedule_store =
        oximux_agents::schedule::ScheduleStore::new(db.conn());
    let (schedule_events, _) = tokio::sync::broadcast::channel(64);
    // Held (never read) for the whole serve lifetime; dropped — releasing the
    // role — only when serve exits.
    let mut _ticker_lock = None;
    let schedule_runner = {
        use oximux_agents::schedule::{TICK, TICKER_LOCK_FILENAME, Ticker};
        match oximux_single_instance::try_acquire(&data_dir.join(TICKER_LOCK_FILENAME)) {
            Ok(oximux_single_instance::AcquireOutcome::Acquired(guard)) => {
                // Held in serve()'s scope for the process lifetime.
                _ticker_lock = Some(guard);
                // Only the lock holder recovers: settling claims another
                // process is actively firing would fail live runs.
                match schedule_store.recover_interrupted(chrono::Local::now()) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(runs = n, "settled interrupted schedule runs"),
                    Err(err) => tracing::warn!(%err, "could not recover interrupted runs"),
                }
                // Orphan sweep: a schedule aimed at a session this host no
                // longer has is disabled and surfaced, not silently skipped.
                let exists = {
                    let settings = settings.clone();
                    move |sid: &str| {
                        settings.get(&blob::chat_settings_key(sid)).ok().flatten().is_some()
                    }
                };
                match schedule_store.sweep_orphaned_targets(chrono::Local::now(), exists) {
                    Ok(ids) if ids.is_empty() => {}
                    Ok(ids) => {
                        tracing::warn!(?ids, "disabled schedules whose target sessions are gone")
                    }
                    Err(err) => tracing::warn!(%err, "orphaned-schedule sweep failed"),
                }
                let firer =
                    scheduler::ServeFirer::new(launcher.clone(), registry.clone());
                let events = schedule_events.clone();
                let ticker = Arc::new(
                    Ticker::new(schedule_store.clone(), Arc::new(firer)).with_recorded_hook(
                        Arc::new(move |run| {
                            // No subscriber is normal (nobody attached).
                            let _ = events.send(oximux_remote_host::schedule_run_to_wire(run));
                        }),
                    ),
                );
                let loop_ticker = ticker.clone();
                tokio::spawn(async move {
                    loop {
                        loop_ticker.tick(chrono::Local::now()).await;
                        tokio::time::sleep(TICK).await;
                    }
                });
                Some(Arc::new(oximux_remote_host::TickerRunner(ticker)))
            }
            Ok(oximux_single_instance::AcquireOutcome::AlreadyRunning { holder_pid }) => {
                // One line, once — this is the expected state beside a running
                // desktop, not an error to nag about.
                let holder = holder_pid
                    .map(|p| format!("process {p}"))
                    .unwrap_or_else(|| "another OxiMux process".into());
                tracing::info!(
                    "schedule ticker: {holder} owns scheduling for this data dir; \
                     schedules will fire from there"
                );
                None
            }
            Err(err) => {
                tracing::warn!(%err, "schedule ticker lock failed; schedules will not fire here");
                None
            }
        }
    };

    // ---- team runs: settle the roles a restart orphaned ----
    // The restart-survival property, and the reason a run is host state: a role
    // whose session survived keeps running; one whose session is gone is closed
    // with a reason, so the run converges instead of waiting forever on an
    // agent that no longer exists.
    let teams = Arc::new(oximux_agents::team::TeamStore::new(db.conn()));
    {
        let settings = settings.clone();
        let exists = move |sid: &str| {
            settings.get(&blob::chat_settings_key(sid)).ok().flatten().is_some()
        };
        match teams.recover_open_roles(chrono::Local::now(), exists) {
            Ok(settled) if settled.is_empty() => {}
            Ok(settled) => {
                tracing::info!(roles = settled.len(), "settled team roles orphaned by a restart")
            }
            Err(err) => tracing::warn!(%err, "team-run recovery failed"),
        }
    }
    let coord = Arc::new(oximux_agents::coord::CoordStore::new(db.conn()));

    let mut dispatcher = Dispatcher::new(registry.clone(), auth.clone())
        .with_launcher(launcher)
        .with_catalog(catalog)
        .with_projects(provider)
        .with_pairing_endpoint(endpoint_id)
        .with_schedule_store(Arc::new(schedule_store))
        .with_schedule_events(schedule_events)
        .with_team_store(teams)
        .with_coord_store(coord)
        .with_worktrees(worktrees);
    if let Some(runner) = schedule_runner {
        dispatcher = dispatcher.with_schedule_runner(runner);
    }
    if let Some(relay) = relay {
        dispatcher =
            dispatcher.with_terminals(Arc::new(oximux_relay_terminals::RelayTerminals::new(relay)));
    }
    // No transcriber and no rewinder: both of those RPCs answer `Unsupported`
    // (or their documented refusal) rather than pretending.
    let dispatcher = Arc::new(dispatcher);

    // ---- local socket: start accepting on the listener bound above ----
    let local = serve_local_connections(dispatcher.clone(), local_listener);

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
    //
    // `dataDir`, not `localSocket`. The value has always been the directory —
    // the socket is `<dir>/control-v1.sock` — and the old name sent readers to
    // connect to a path that is not a socket. It is also exactly what a caller
    // does with it: hand it back as `--dir`. Renamed while the field still has
    // no consumers outside this repo; after a release it could only be added
    // beside the lie, never instead of it.
    let endpoint_hex: String = endpoint_id.iter().map(|b| format!("{b:02x}")).collect();
    println!(
        "{}",
        serde_json::json!({
            "type": "oximux_serve_ready",
            "schemaVersion": 1,
            "protocolVersion": oximux_remote_proto::proto::PROTOCOL_VERSION,
            "dataDir": data_dir.to_string_lossy(),
            "endpointId": endpoint_hex,
        })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    tracing::info!(data_dir = %data_dir.display(), endpoint = %endpoint_hex, "serve ready");

    // ---- run until asked to stop ----
    match external_shutdown {
        // The SCM control handler owns the signal; a closed channel (the
        // handler was torn down) reads as a stop rather than serving forever
        // with nothing able to stop it.
        Some(rx) => {
            let _ = rx.await;
        }
        None => wait_for_shutdown().await,
    }
    tracing::info!("shutdown requested; draining");

    // ---- drain: stop taking work, let in-flight turns finish, persist ----
    draining.store(true, Ordering::SeqCst);
    // Cuts the local listener AND aborts every in-flight local RPC task (the
    // JoinSet dies with the accept loop). Deliberate: a drain's contract is
    // about agent turns, not about answering one last `ls` — and a client
    // sees the same "host closed the connection" a crash would produce.
    drop(local);
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

fn serve_local_connections(
    dispatcher: Arc<Dispatcher>,
    listener: Arc<LocalControlListener>,
) -> LocalHandle {
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
    LocalHandle { task }
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
