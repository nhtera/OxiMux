//! The relay daemon's lifecycle once the app is running: the crash heartbeat,
//! respawn, and (later) the user-facing restart and kill-all.
//!
//! Boot (`main.rs`) brings the first daemon up and hands its client to
//! [`RelayLifecycle::install`]. From then on this module owns every daemon
//! replacement, so there is exactly one place that decides whether a death has
//! already been handled:
//!
//! - **Single flight.** Every replacement runs under `respawn_lock` and first
//!   checks that the session it is replacing is still the current one. A second
//!   caller for the same death finds the epoch moved on and returns
//!   [`RespawnOutcome::AlreadyRespawned`] without touching anything.
//! - **Owned deaths.** A manual restart records the session it is about to stop
//!   in `expected_death`, so the heartbeat that sees that pid vanish stays out of
//!   it — no crash copy, no throttle count, no second respawn.
//! - **A restart re-checks the epoch after taking the lock.** Aborting the
//!   heartbeat stops the watch, not a crash respawn it already spawned; that one
//!   may be parked on `respawn_lock` and win it. So a restart snapshots
//!   `current_session` before locking and, once it holds the lock, treats a
//!   moved epoch as "already replaced" instead of stopping the fresh daemon.
//! - **No `SHARED_BACKEND` lock on a runtime worker.** The main thread can hold
//!   that mutex inside a `block_on` whose reply needs this runtime; a parked
//!   worker would deadlock it. Stop, probe and kill-all therefore use the
//!   client held here, and the backend swap runs on the blocking pool.
//!
//! The UI reads the outcome from [`state::RelayDaemonState`], which one
//! foreground loop keeps current from the events this module emits.

mod heartbeat;
mod kill_all;
mod probe;
mod respawn;
mod restart;
pub mod state;
pub mod ui;

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use oximux_relay_client::RelayClient;
use oximux_relay_supervisor::RelaySupervisor;

pub use kill_all::{KillAllError, KillAllFuture, KillAllOutcome};
pub use respawn::RespawnOutcome;
pub use restart::{RestartError, RestartFuture, RestartOutcome};

/// Why a daemon is being replaced. Decides the copy the user sees and whether
/// the crash throttle counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnReason {
    /// The heartbeat saw the daemon die on its own.
    Crash,
    /// The user asked for it.
    Manual,
}

/// Why a replacement did not produce a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespawnFailure {
    /// The crash throttle refused: the daemon died too often, too fast.
    KeepsStopping,
    /// Windows: another process owns the daemon's pipe name.
    EndpointHeld,
    /// A daemon of another build owns the socket.
    VersionMismatch,
    /// Every attempt failed; the text is for the log and the toast.
    Other(Arc<str>),
}

/// What the foreground learns about the daemon. Emitted once per outcome, from
/// one place each, and drained by [`state::install`].
#[derive(Debug, Clone)]
pub enum RelayLifecycleEvent {
    Respawned {
        dead_session: String,
        new_session: String,
        reason: RespawnReason,
    },
    RespawnFailed {
        reason: RespawnReason,
        failure: RespawnFailure,
    },
    /// A restart could not stop the daemon; nothing was restarted.
    RestartFailed { reason: Arc<str> },
    /// Whether the daemon answered a probe.
    Probed { responsive: bool },
    /// Boot stopped the previous protocol's daemon, so every terminal was
    /// restarted once. `foreign_serve` means an `oximux serve` was using it and
    /// must be restarted too.
    PreviousDaemonRetired { foreign_serve: bool },
}

/// The pid of an `oximux serve` (or another desktop) holding the local control
/// role, read without taking the lock. `None` when this process holds it, or
/// nobody alive does.
///
/// The record outlives its writer, and a pid alone is not identity: after a
/// crash the number can be recycled by an unrelated live process, which
/// `pid_alive` would report as a foreign serve (and boot would restart daemons
/// on that false premise). So a record that carries a start time must match
/// the live process's start time and this boot. A one-line record from an
/// older build has no start time; it keeps the old liveness check rather than
/// hiding a real older serve that is still running.
pub fn foreign_serve_holder(data_dir: &std::path::Path) -> Option<u32> {
    let lock = data_dir.join(oximux_remote_local::HOST_LOCK_FILENAME);
    let record = oximux_single_instance::read_holder_record(&lock)?;
    let live = if record.started_at_unix_s.is_some() {
        record.is_live()
    } else {
        oximux_relay_supervisor::pid_alive(record.pid)
    };
    (record.pid != std::process::id() && live).then_some(record.pid)
}

/// A crash respawn is admitted only while fewer than [`CRASH_LIMIT`] happened
/// inside [`CRASH_WINDOW`]. A daemon that keeps dying is left down and reported
/// rather than restarted in a loop.
const CRASH_LIMIT: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);

/// Sliding window of recent crash respawns.
#[derive(Debug, Default)]
struct CrashThrottle {
    recent: VecDeque<Instant>,
}

impl CrashThrottle {
    /// Record a crash respawn at `now` if the window has room for it.
    fn admit(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) >= CRASH_WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= CRASH_LIMIT {
            return false;
        }
        self.recent.push_back(now);
        true
    }

    /// A deliberate restart starts the count over.
    fn reset(&mut self) {
        self.recent.clear();
    }
}

pub struct RelayLifecycle {
    supervisor: RelaySupervisor,
    repo: oximux_storage::PaneRelayIdRepo,
    handle: tokio::runtime::Handle,
    /// The current daemon's client. Everything that talks to the daemon from
    /// the relay runtime goes through this, never through `SHARED_BACKEND`.
    client: RwLock<Arc<RelayClient>>,
    /// The current daemon's session id — the epoch every replacement checks.
    current_session: RwLock<String>,
    respawn_lock: tokio::sync::Mutex<()>,
    heartbeat: Mutex<Option<tokio::task::AbortHandle>>,
    /// What the heartbeat was last armed on: (pid, session).
    heartbeat_target: Mutex<Option<(u32, String)>>,
    /// A session whose death a manual restart has claimed.
    expected_death: Mutex<Option<String>>,
    crash_throttle: Mutex<CrashThrottle>,
    restart_in_flight: Mutex<Option<RestartFuture>>,
    kill_all_in_flight: Mutex<Option<KillAllFuture>>,
    probe_in_flight: AtomicBool,
    last_unreachable: Mutex<Option<Instant>>,
    /// The daemon boot kept although it is from another app version.
    stale_at_boot: Mutex<Option<state::StaleInfo>>,
    #[cfg(test)]
    restart_runs: std::sync::atomic::AtomicUsize,
    events_tx: UnboundedSender<RelayLifecycleEvent>,
    events_rx: Mutex<Option<UnboundedReceiver<RelayLifecycleEvent>>>,
}

static LIFECYCLE: OnceLock<Arc<RelayLifecycle>> = OnceLock::new();

/// The installed lifecycle, if the relay came up at boot.
pub fn lifecycle() -> Option<Arc<RelayLifecycle>> {
    LIFECYCLE.get().cloned()
}

impl RelayLifecycle {
    fn new(
        supervisor: RelaySupervisor,
        repo: oximux_storage::PaneRelayIdRepo,
        handle: tokio::runtime::Handle,
        client: Arc<RelayClient>,
    ) -> Arc<Self> {
        let session = client.server_session_id().to_owned();
        let (events_tx, events_rx) = unbounded();
        Arc::new(Self {
            supervisor,
            repo,
            handle,
            client: RwLock::new(client),
            current_session: RwLock::new(session),
            respawn_lock: tokio::sync::Mutex::new(()),
            heartbeat: Mutex::new(None),
            heartbeat_target: Mutex::new(None),
            expected_death: Mutex::new(None),
            crash_throttle: Mutex::new(CrashThrottle::default()),
            restart_in_flight: Mutex::new(None),
            kill_all_in_flight: Mutex::new(None),
            probe_in_flight: AtomicBool::new(false),
            last_unreachable: Mutex::new(None),
            stale_at_boot: Mutex::new(None),
            #[cfg(test)]
            restart_runs: std::sync::atomic::AtomicUsize::new(0),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
        })
    }

    /// Publish the lifecycle for the daemon boot just connected to and start
    /// watching it. Called once, from the relay boot.
    pub fn install(
        supervisor: RelaySupervisor,
        repo: oximux_storage::PaneRelayIdRepo,
        handle: tokio::runtime::Handle,
        client: Arc<RelayClient>,
    ) -> Arc<Self> {
        let lifecycle = Self::new(supervisor, repo, handle, client);
        match lifecycle.supervisor.read_pid() {
            Some(pid) => lifecycle.arm_heartbeat(pid, lifecycle.current_session()),
            None => tracing::warn!("relay PID file missing; crash heartbeat disabled"),
        }
        if LIFECYCLE.set(Arc::clone(&lifecycle)).is_err() {
            tracing::warn!("relay lifecycle already installed; ignoring");
        }
        lifecycle
    }

    /// The current daemon's client.
    pub fn client(&self) -> Arc<RelayClient> {
        Arc::clone(&self.client.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// The current daemon's session id.
    pub fn current_session(&self) -> String {
        self.current_session.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The current daemon's pid, from its pid file.
    pub fn pid(&self) -> Option<u32> {
        self.supervisor.read_pid()
    }

    pub fn supervisor(&self) -> &RelaySupervisor {
        &self.supervisor
    }

    /// Whether an `oximux serve` (or another desktop) is using this daemon;
    /// it cannot follow a restart and must be restarted itself.
    pub fn foreign_serve(&self) -> bool {
        foreign_serve_holder(self.supervisor.runtime_dir()).is_some()
    }

    /// Boot stopped the previous protocol's daemon: tell the UI once.
    pub fn note_previous_daemon_retired(&self, foreign_serve: bool) {
        self.emit(RelayLifecycleEvent::PreviousDaemonRetired { foreign_serve });
    }

    /// Boot kept a daemon from another app version (it had sessions): the
    /// Settings status offers the restart that updates it.
    pub fn note_stale_daemon(&self, daemon_version: String, app_version: String) {
        *self.stale_at_boot.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(state::StaleInfo { daemon_version, app_version });
    }

    /// What [`Self::note_stale_daemon`] recorded; read once, to seed the state.
    fn take_stale_at_boot(&self) -> Option<state::StaleInfo> {
        self.stale_at_boot.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// Hand the event stream to the one foreground loop that drains it.
    fn take_events(&self) -> Option<UnboundedReceiver<RelayLifecycleEvent>> {
        self.events_rx.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn emit(&self, event: RelayLifecycleEvent) {
        // Closed only when the foreground loop is gone, i.e. the app is exiting.
        let _ = self.events_tx.unbounded_send(event);
    }
}

#[cfg(test)]
mod tests;
