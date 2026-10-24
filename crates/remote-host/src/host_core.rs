//! The host's process-wide orchestration state.
//!
//! State both dispatchers (remote and local) must share lives here, built once
//! per process and injected into each, never constructed per dispatcher: the
//! spawn lineage, the host config, and the runtime the host's background work
//! runs on. Later phases add the child watcher, the outbox drainer, attention
//! and push to it.
//!
//! **Always on.** On the desktop the core runs whether or not a remote or local
//! listener is enabled; the listeners are only front doors. Its background
//! work runs on an *injected* runtime handle, so a core built from the GPUI
//! main thread never depends on `Handle::try_current`.
//!
//! **Ownership of the data dir.** Only the process holding the data
//! directory's host lock writes lineage. [`HostCore::start`] is called each
//! time the host wins the lock: it reconciles what the previous owner left
//! (stale reservations deleted, working rows parked) and turns writes on.
//! [`HostCore::release`] turns them off again when the host gives the lock up
//! (the desktop does when local access is switched off, so `oximux serve` may
//! take over). Without the lock the core is a guest: it parks nothing, revives
//! nothing, and reports that spawning needs the host role.

use std::sync::{Arc, Mutex};

use oximux_agents::lineage::{LineageState, LineageStore, WorkingSessions};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_single_instance::HolderRecord;
use tokio::sync::watch;

use crate::host_config::HostConfig;

/// Who held the data directory before this host won its lock.
///
/// Winning the lock proves the previous holder is gone (the OS released its
/// lock). The record says who it was and, where both boot times are known,
/// whether the machine rebooted in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriorOwner {
    /// No record: a fresh data directory (or one never held by a host that
    /// wrote a record).
    None,
    /// The previous holder is gone. `same_boot: false` means the machine
    /// rebooted since it ran.
    Gone { record: HolderRecord, same_boot: bool },
    /// A record exists but its boot time cannot be compared: gone, but no
    /// claim is made about why.
    Unknown { record: HolderRecord },
}

impl PriorOwner {
    /// Classify the record a won lock handed back.
    pub fn from_previous(previous: Option<&HolderRecord>) -> Self {
        match previous {
            None => PriorOwner::None,
            Some(record) => match record.same_boot() {
                Some(same_boot) => PriorOwner::Gone { record: record.clone(), same_boot },
                None => PriorOwner::Unknown { record: record.clone() },
            },
        }
    }
}

/// Whether a session may spawn, and why not. Spawn tools are advertised only
/// when `allowed` is true; transient count caps are not part of it (they
/// refuse at call time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowance {
    pub allowed: bool,
    /// Why not, in words an agent can act on; `None` when allowed.
    pub reason: Option<String>,
    pub depth: u32,
    pub max_depth: u32,
}

/// What [`HostCore::new`] is built from.
pub struct HostCoreDeps {
    /// The runtime the core's background tasks run on, for the process lifetime.
    pub rt: tokio::runtime::Handle,
    pub lineage: LineageStore,
    pub registry: Arc<SessionRegistry>,
    pub config: HostConfig,
}

pub struct HostCore {
    rt: tokio::runtime::Handle,
    lineage: LineageStore,
    registry: Arc<SessionRegistry>,
    config: watch::Sender<HostConfig>,
    /// `Some` while this process owns the data dir: who owned it before.
    prior: Mutex<Option<PriorOwner>>,
}

impl HostCore {
    /// Build the core and start its always-on work: the status subscriber that
    /// parks a spawned child when its session goes idle and revives it when it
    /// works again.
    pub fn new(deps: HostCoreDeps) -> Arc<Self> {
        let (config, _) = watch::channel(deps.config.sanitized());
        let core = Arc::new(Self {
            rt: deps.rt,
            lineage: deps.lineage,
            registry: deps.registry,
            config,
            prior: Mutex::new(None),
        });
        core.spawn_status_subscriber();
        core
    }

    /// This process has (re)won the data directory's host lock. Reconcile what
    /// the previous owner left (its reservations will never launch, nothing it
    /// ran is still working), remember who it was, and start writing. Runs on
    /// every acquisition: a lock given up and won back may have been held by
    /// another host in between. A call while already the owner is a no-op.
    pub fn start(&self, prior: PriorOwner) {
        let mut slot = self.prior.lock().unwrap();
        if slot.is_some() {
            return;
        }
        if let Err(err) = self.lineage.boot_reconcile(chrono::Local::now()) {
            tracing::warn!(%err, "lineage boot reconcile failed");
        }
        tracing::info!(?prior, "host started");
        *slot = Some(prior);
        drop(slot);
        // Our own working chats may have been parked just now; correct them.
        self.sync_lineage_states();
    }

    /// The host gave the data directory up: stop writing until it wins the
    /// lock again.
    pub fn release(&self) {
        if self.prior.lock().unwrap().take().is_some() {
            tracing::info!("host released the data directory; lineage writes off");
        }
    }

    /// Whether this process owns the data directory (and so writes lineage).
    pub fn is_owner(&self) -> bool {
        self.prior.lock().unwrap().is_some()
    }

    /// Who held the data directory before this host, while it is the owner.
    pub fn prior_owner(&self) -> Option<PriorOwner> {
        self.prior.lock().unwrap().clone()
    }

    pub fn lineage(&self) -> &LineageStore {
        &self.lineage
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.rt
    }

    /// The config as enforced right now.
    pub fn config(&self) -> HostConfig {
        *self.config.borrow()
    }

    /// Apply new settings. Sanitized on the way in, so a stored depth above the
    /// ceiling never takes effect.
    pub fn set_config(&self, config: HostConfig) {
        self.config.send_replace(config.sanitized());
    }

    /// Answers "is this session working?" for reservation-time reconcile, from
    /// what the session's driver publishes. An unregistered session is idle.
    pub fn working(&self) -> RegistryWorking<'_> {
        RegistryWorking(&self.registry)
    }

    /// Whether `session` may spawn at all: delegation is on, it is a chat
    /// session this host drives, it is shallower than the depth cap, and its
    /// posture is `Default` (reviewers and verifiers never spawn).
    pub fn spawn_allowance(&self, session: &str) -> Allowance {
        let config = self.config();
        let max_depth = config.caps.depth;
        let row = self.lineage.row(session).ok().flatten();
        let depth = row.as_ref().map_or(0, |r| r.depth);
        let refuse = |reason: &str| Allowance {
            allowed: false,
            reason: Some(reason.to_string()),
            depth,
            max_depth,
        };
        if !config.delegation_enabled {
            return refuse("Agent delegation is turned off in Settings. Complete this task yourself.");
        }
        if !self.is_owner() {
            return refuse(
                "Spawning needs this app to host the data directory (turn on local access, \
                 or another host owns it). Complete this task yourself.",
            );
        }
        if self.registry.get(session).is_none() {
            return refuse("Spawning requires a chat agent. Complete this task yourself.");
        }
        if row.as_ref().is_some_and(|r| r.posture != SpawnPosture::Default) {
            return refuse("Review agents do not spawn; report findings instead.");
        }
        if depth >= max_depth {
            return refuse("Spawning is not available at this depth. Complete this task yourself.");
        }
        Allowance { allowed: true, reason: None, depth, max_depth }
    }

    /// Park or revive every tracked child against what its session publishes.
    /// A session that is not registered (tab closed, process gone) is not
    /// working, so an `active` row parks — parking is not ending: the row comes
    /// back on the next turn once it registers again. Only the data dir's owner
    /// writes; a guest does nothing.
    pub fn sync_lineage_states(&self) {
        if !self.is_owner() {
            return;
        }
        let tracked = match self.lineage.tracked() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "lineage sync: read failed");
                return;
            }
        };
        let now = chrono::Local::now();
        for (session, state) in tracked {
            let working = self.working().is_working(&session);
            let result = match (state, working) {
                (LineageState::Parked, true) => self.lineage.activate(&session, now),
                (LineageState::Active, false) => self.lineage.park(&session, now),
                _ => continue,
            };
            if let Err(err) = result {
                tracing::warn!(%err, session, "lineage sync: write failed");
            }
        }
    }

    fn spawn_status_subscriber(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut changes = self.registry.subscribe_changes();
        self.rt.spawn(async move {
            loop {
                let Some(core) = weak.upgrade() else { return };
                core.sync_lineage_states();
                drop(core);
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
    }
}

/// [`WorkingSessions`] over the registry: working iff registered and its
/// driver says so (or a permission prompt is open).
pub struct RegistryWorking<'a>(&'a SessionRegistry);

impl WorkingSessions for RegistryWorking<'_> {
    fn is_working(&self, session_id: &str) -> bool {
        self.0.get(session_id).is_some_and(|h| {
            let s = h.status_snapshot();
            s.working || s.awaiting_permission
        })
    }
}

#[cfg(test)]
#[path = "host_core_tests.rs"]
mod tests;
