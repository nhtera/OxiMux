//! The desktop's half of scheduled agent runs: its firer, its keep-awake
//! reconcile, and the boot shell that decides whether this process ticks.
//!
//! The engine — due-selection, the double-launch guards, the durable claim,
//! run recording — is [`oximux_agents::schedule::Ticker`], shared with the
//! headless host. What is desktop-specific rides here:
//!
//! - **Firing opens a tab.** The fire crosses to the GPUI thread over the same
//!   launch bridge remote-control launches use ([`BridgeLauncher`]), with the
//!   schedule's prompt as the session's first message. A desktop with no
//!   window answers [`FireOutcome::NotNow`], leaving the occurrence due until
//!   a window exists — a missed run for a recoverable condition would read as
//!   a broken feature.
//! - **Keep-awake.** While any schedule is enabled the desktop holds a sleep
//!   assertion, reconciled once per tick via the firer's `on_tick` hook.
//! - **The ticker lock.** One process per data directory ticks. The desktop
//!   contends for `schedule-ticker.lock` at install; losing it (a headless
//!   host already serving this data dir) logs one clear line and starts no
//!   loop — schedules stay readable and editable, they just fire elsewhere.
//!
//! The loop itself runs on the GPUI **background** executor: the tick's store
//! reads must not stall a frame, and the fire path no longer needs the UI
//! thread directly — the bridge hops for it.

use std::sync::{Arc, Mutex};

use chrono::Local;
use oximux_agents::schedule::{
    FireOutcome, Schedule, ScheduleFirer, ScheduleStore, ScheduleTarget, TICK,
    TICKER_LOCK_FILENAME, Ticker,
};
use oximux_remote_host::LaunchError;
use oximux_remote_proto::messages::ScheduleRunWire;

use crate::agent_awake::{AgentAwake, AwakeHold};
use crate::remote_control::launch_bridge::BridgeLauncher;

/// The desktop's [`ScheduleFirer`]: sessions open as tabs via the launch
/// bridge, and the keep-awake hold is reconciled on every tick.
pub struct DesktopFirer {
    launcher: BridgeLauncher,
    store: ScheduleStore,
    /// Live sessions, for the heartbeat arm: waking an already-open session is
    /// a prompt into its registry handle, not a tab to open.
    registry: Arc<oximux_agents::session_registry::SessionRegistry>,
    /// Injected rather than reached for via [`crate::agent_awake::global`] so the
    /// reconcile logic can be tested against a mock backend instead of real power
    /// management.
    awake_source: Arc<AgentAwake>,
    /// The live hold, or `None` when nothing is armed. `Option` rather than a
    /// count because one assertion covers every schedule.
    awake: Mutex<Option<AwakeHold>>,
}

impl DesktopFirer {
    pub fn new(
        launcher: BridgeLauncher,
        store: ScheduleStore,
        awake_source: Arc<AgentAwake>,
        registry: Arc<oximux_agents::session_registry::SessionRegistry>,
    ) -> Self {
        Self { launcher, store, registry, awake_source, awake: Mutex::new(None) }
    }

    /// Take the keep-awake hold when any schedule is enabled, release it when none
    /// is.
    ///
    /// Reconciling here rather than notifying from the store means the hold can lag
    /// a change by up to one [`TICK`]. That is harmless — a machine does not idle
    /// sleep within thirty seconds of the interaction that enabled the schedule —
    /// and it keeps the store free of a subscription it would otherwise exist only
    /// to serve.
    fn reconcile_awake(&self) {
        let want = match self.store.any_enabled() {
            Ok(want) => want,
            // Leave the hold exactly as it is. Guessing in either direction is
            // worse than doing nothing for one tick: releasing on a transient read
            // failure could let the machine sleep through a run, and acquiring
            // could pin a laptop awake over a database that is simply unreadable.
            Err(err) => {
                tracing::warn!(%err, "scheduler: could not read schedule state");
                return;
            }
        };
        let mut awake = self
            .awake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match (want, awake.is_some()) {
            (true, false) => *awake = Some(self.awake_source.acquire_scheduling()),
            (false, true) => drop(awake.take()),
            // Already in the right state. Reassigning in the `(true, true)` arm
            // would drop the old hold only *after* taking a second one, which
            // reads as harmless but leaks the assertion the first one held — and
            // `pmset` shows a single entry either way, so it would not look wrong.
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl ScheduleFirer for DesktopFirer {
    async fn fire(&self, schedule: &Schedule, target: &ScheduleTarget) -> FireOutcome {
        match target {
            ScheduleTarget::NewSession => {}
            // A heartbeat: nudge the live session instead of opening a tab. No
            // bridge hop — the registry entry is reachable from any thread, and
            // the chat view renders the prompt through its remote-prompt sink
            // exactly as it does for one sent from the phone.
            ScheduleTarget::ExistingSession(session_id) => {
                return oximux_agents::schedule::nudge_existing_session(
                    &self.registry,
                    schedule,
                    session_id,
                );
            }
        }
        match self
            .launcher
            .create_with_prompt(
                &schedule.cwd,
                schedule.agent_id.as_deref(),
                Some(schedule.prompt.clone()),
            )
            .await
        {
            Ok(session_id) => FireOutcome::Completed { session_id: Some(session_id) },
            // No window to host the tab (or the bridge's drain loop is gone).
            // Recoverable — retry when one exists.
            Err(LaunchError::Unavailable) => FireOutcome::NotNow,
            Err(err) => FireOutcome::Failed {
                session_id: None,
                detail: launch_error_detail(&err).to_string(),
            },
        }
    }

    fn on_tick(&self) {
        self.reconcile_awake();
    }
}

/// Contend for the ticker lock and, on winning, recover interrupted runs and
/// start the tick loop. Returns the ticker (for the run-now RPC seam), or
/// `None` when this process does not own this data dir's scheduling.
///
/// **No lock, no ticker — ever.** The tempting degraded mode ("tick unguarded
/// when the lock errors") is wrong twice over: with another host actually
/// holding the lock, an unguarded recovery would rewrite that host's in-flight
/// runs to failed and advance their cadence out from under it; and an
/// unguarded ticker *without* recovery would leave a crashed boot's `'running'`
/// claims unsettled forever, wedging those schedules (always due, always
/// "already ran"). So a lock error declines loudly, exactly like losing the
/// contest — the same posture `oximux serve` takes.
///
/// Ticks before the first sleep so a schedule that came due while the app was
/// closed fires at boot rather than one [`TICK`] later.
pub fn install(
    store: ScheduleStore,
    launcher: BridgeLauncher,
    registry: Arc<oximux_agents::session_registry::SessionRegistry>,
    data_dir: Option<std::path::PathBuf>,
    run_events: tokio::sync::broadcast::Sender<ScheduleRunWire>,
    session_exists: impl Fn(&str) -> bool,
    cx: &mut gpui::App,
) -> Option<Arc<Ticker>> {
    // In practice always present — storage opened from this same directory
    // earlier in boot — but the signature is honest about the platform API.
    let Some(dir) = &data_dir else {
        tracing::warn!("schedule ticker: no data dir; schedules will not fire from this process");
        return None;
    };
    let lock = match oximux_single_instance::try_acquire(&dir.join(TICKER_LOCK_FILENAME)) {
        Ok(oximux_single_instance::AcquireOutcome::Acquired(guard)) => guard,
        Ok(oximux_single_instance::AcquireOutcome::AlreadyRunning { holder_pid }) => {
            // One line, once — a repeating warning would train users to
            // ignore it, and this is the *expected* state on a machine
            // running both hosts.
            let holder = holder_pid
                .map(|p| format!("process {p}"))
                .unwrap_or_else(|| "another OxiMux process".into());
            tracing::info!(
                "schedule ticker: {holder} owns scheduling for this data dir; \
                 schedules will fire from there"
            );
            return None;
        }
        Err(err) => {
            tracing::warn!(
                %err,
                "schedule ticker: lock failed; schedules will not fire from this process"
            );
            return None;
        }
    };

    // Only the lock holder may recover: failing a run another process is
    // actively firing would be worse than leaving it. Reached only with the
    // lock in hand — see the doc comment.
    match store.recover_interrupted(Local::now()) {
        Ok(0) => {}
        Ok(n) => tracing::info!(runs = n, "schedule ticker: settled interrupted runs from last boot"),
        Err(err) => tracing::warn!(%err, "schedule ticker: could not recover interrupted runs"),
    }
    // Same lock-holder rule for the orphan sweep: a schedule aimed at a
    // session this host no longer has is disabled and surfaced in its run
    // history, never silently skipped every tick.
    match store.sweep_orphaned_targets(Local::now(), session_exists) {
        Ok(ids) if ids.is_empty() => {}
        Ok(ids) => {
            tracing::warn!(?ids, "schedule ticker: disabled schedules whose target sessions are gone")
        }
        Err(err) => tracing::warn!(%err, "schedule ticker: orphaned-schedule sweep failed"),
    }

    let firer = DesktopFirer::new(
        launcher,
        store.clone(),
        crate::agent_awake::global().clone(),
        registry,
    );
    let ticker = Arc::new(Ticker::new(store, Arc::new(firer)).with_recorded_hook(Arc::new(
        move |run| {
            // No subscriber is normal (remote off, no CLI attached).
            let _ = run_events.send(oximux_remote_host::schedule_run_to_wire(run));
        },
    )));

    let loop_ticker = ticker.clone();
    let executor = cx.background_executor().clone();
    let timer = executor.clone();
    executor
        .spawn(async move {
            // The lock guard lives inside the loop task: released only when the
            // process exits and the task dies with it.
            let _lock = lock;
            loop {
                loop_ticker.tick(Local::now()).await;
                timer.timer(TICK).await;
            }
        })
        .detach();
    Some(ticker)
}

/// A launch failure as run history should read it. Coarser than it could be:
/// [`open_session`](crate::remote_control::launch_bridge) folds "no default agent
/// configured" and "the named agent is not chat-capable" into one
/// [`LaunchError::Failed`] so the remote protocol never leaks host detail, and
/// the scheduler reads the same error. Distinguishing them would mean widening a
/// wire type for a local caller — not worth it.
fn launch_error_detail(err: &LaunchError) -> &'static str {
    match err {
        LaunchError::BadWorkingDirectory => "that working directory is not usable",
        LaunchError::Failed => {
            "no usable agent — none is set as the default, or the named one is not chat-capable"
        }
        // Not recorded (the fire path treats it as "not yet"), but the match
        // must be total.
        LaunchError::Unavailable => "the desktop could not start a session right now",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::schedule::{NewSchedule, Recurrence};

    use crate::agent_awake::SleepAssertionBackend;

    #[derive(Default)]
    struct MockBackend {
        created: Mutex<u32>,
    }

    impl SleepAssertionBackend for MockBackend {
        fn create(&self, _name: &str) -> Option<u64> {
            let mut created = self.created.lock().unwrap();
            *created += 1;
            Some(u64::from(*created))
        }
        fn release(&self, _id: u64) {}
    }

    /// A firer over an empty in-memory database, plus the backend so a test
    /// can count how many assertions were ever created. The bridge's receiver
    /// is dropped — these tests never fire.
    fn firer() -> (DesktopFirer, Arc<MockBackend>, Arc<AgentAwake>) {
        firer_with_registry(Arc::new(oximux_agents::session_registry::SessionRegistry::new()))
    }

    /// The same firer over a caller-supplied registry, so a heartbeat test can
    /// put a live session in it first.
    fn firer_with_registry(
        registry: Arc<oximux_agents::session_registry::SessionRegistry>,
    ) -> (DesktopFirer, Arc<MockBackend>, Arc<AgentAwake>) {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));
        let store = ScheduleStore::new(db.conn());
        let (launcher, _rx) = crate::remote_control::launch_bridge::launch_bridge();
        (DesktopFirer::new(launcher, store, awake.clone(), registry), backend, awake)
    }

    fn a_schedule(name: &str) -> NewSchedule {
        NewSchedule {
            name: name.to_string(),
            cwd: "/tmp".to_string(),
            prompt: "do the thing".to_string(),
            agent_id: None,
            recurrence: Recurrence::DailyAt { hour: 9, minute: 0 },
        }
    }

    /// An enabled schedule is a reason to stay awake; removing it is not.
    #[test]
    fn the_hold_follows_whether_anything_is_armed() {
        let (firer, _backend, awake) = firer();
        firer.reconcile_awake();
        assert!(!awake.asserted(), "nothing armed, nothing to stay awake for");

        let created = firer
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        firer.reconcile_awake();
        assert!(awake.asserted(), "an armed schedule holds it");

        firer.store.delete(&created.id).expect("delete");
        firer.reconcile_awake();
        assert!(!awake.asserted(), "released once nothing is armed");
    }

    /// Reconciling repeatedly must not stack assertions. A second `create` would
    /// leak the first — and since the OS refcounts nothing and `pmset` shows one
    /// entry per assertion name, the leak would be invisible.
    #[test]
    fn reconciling_twice_does_not_create_a_second_assertion() {
        let (firer, backend, _awake) = firer();
        firer.store.create(a_schedule("morning"), Local::now()).expect("create");

        firer.reconcile_awake();
        firer.reconcile_awake();
        firer.reconcile_awake();

        assert_eq!(*backend.created.lock().unwrap(), 1, "one assertion, not three");
    }

    /// A disabled schedule is not a reason to keep a laptop awake.
    #[test]
    fn a_disabled_schedule_holds_nothing() {
        let (firer, _backend, awake) = firer();
        let created = firer
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        firer
            .store
            .set_enabled(&created.id, false, Local::now())
            .expect("disable");

        firer.reconcile_awake();
        assert!(!awake.asserted());
    }

    /// Re-enabling re-takes the hold, so a schedule switched back on is not left
    /// armed behind a machine that will sleep through it.
    #[test]
    fn re_enabling_retakes_the_hold() {
        let (firer, _backend, awake) = firer();
        let created = firer
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        firer.store.set_enabled(&created.id, false, Local::now()).expect("disable");
        firer.reconcile_awake();
        assert!(!awake.asserted());

        firer.store.set_enabled(&created.id, true, Local::now()).expect("enable");
        firer.reconcile_awake();
        assert!(awake.asserted());
    }

    /// A heartbeat wakes the live session it names — no tab opens, so the
    /// launch bridge (whose receiver is dropped here, making any launch fail)
    /// is never touched. That the fire succeeds at all is the proof.
    #[tokio::test]
    async fn a_heartbeat_wakes_the_live_session_instead_of_spawning() {
        let registry = Arc::new(oximux_agents::session_registry::SessionRegistry::new());
        registry.register(
            "sess-1".into(),
            Arc::new(oximux_agents::thread::StubConnection::default()),
        );
        let (firer, _backend, _awake) = firer_with_registry(registry);
        let made = firer.store.create(a_schedule("morning"), Local::now()).expect("create");

        match firer.fire(&made, &ScheduleTarget::ExistingSession("sess-1".into())).await {
            FireOutcome::Completed { session_id } => {
                assert_eq!(session_id.as_deref(), Some("sess-1"))
            }
            other => panic!("must nudge the session, got {:?}", outcome_name(&other)),
        }
    }

    /// A heartbeat whose session is not live records a failure rather than
    /// declining: `NotNow` would leave it due forever, and a dormant session's
    /// agent is not coming back on its own.
    #[tokio::test]
    async fn a_heartbeat_on_a_dead_session_fails_rather_than_deferring() {
        let (firer, _backend, _awake) = firer();
        let made = firer.store.create(a_schedule("morning"), Local::now()).expect("create");

        match firer.fire(&made, &ScheduleTarget::ExistingSession("sess-gone".into())).await {
            FireOutcome::Failed { detail, session_id } => {
                assert_eq!(session_id.as_deref(), Some("sess-gone"), "name where to look");
                assert!(detail.contains("not running"), "says why: {detail}");
            }
            other => panic!("must record a failure, got {:?}", outcome_name(&other)),
        }
    }

    /// Test-only: `FireOutcome` carries no `Debug`, and a panic message that
    /// cannot name what it got is a panic you have to re-run to understand.
    fn outcome_name(outcome: &FireOutcome) -> &'static str {
        match outcome {
            FireOutcome::Completed { .. } => "Completed",
            FireOutcome::Failed { .. } => "Failed",
            FireOutcome::NotNow => "NotNow",
        }
    }

    /// With no drain loop behind the bridge, a fire is a clean `NotNow` — the
    /// occurrence stays due rather than recording a failure.
    #[tokio::test]
    async fn a_bridge_with_no_drain_loop_declines_rather_than_fails() {
        let (firer, _backend, _awake) = firer();
        let made = firer.store.create(a_schedule("morning"), Local::now()).expect("create");

        match firer.fire(&made, &ScheduleTarget::NewSession).await {
            FireOutcome::NotNow => {}
            _ => panic!("a dead bridge must decline, not fail the run"),
        }
    }
}
