//! The ticker that drives scheduled agent runs.
//!
//! [`oximux_agents::schedule`] holds the persistence and the time arithmetic but
//! has no opinion about when anything happens; this is the part that wakes up,
//! asks which schedules are due, and keeps the machine awake while any are armed.
//!
//! **Level-triggered, not edge-triggered.** `due()` asks `next_fire_at <= now`
//! rather than "did a boundary just pass", so a tick that arrives late — or does
//! not arrive at all, because the machine slept through it — still catches the
//! occurrence on the next pass. That is what makes [`TICK`] a latency knob rather
//! than a correctness one, and it is why nothing here tries to align to wall-clock
//! boundaries.
//!
//! **Not owned by a window.** The scheduler must keep ticking while the user
//! closes and reopens windows, so it is spawned at the app level and detached
//! rather than held as a view's task. It runs on the GPUI executor, which is what
//! lets the fire path open a tab directly instead of going through the mpsc
//! handoff that remote-control launches need (`AsyncApp` is not `Send`, so the
//! tokio-side RPC dispatcher cannot touch GPUI; this loop is already on it).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};
use oximux_agents::schedule::{RunOutcome, Schedule, ScheduleStore};
use oximux_remote_host::LaunchError;

use crate::agent_awake::{AgentAwake, AwakeHold};

/// How often to look for due schedules.
///
/// `Recurrence::EveryMinutes` has a five-minute floor, so no interval below that
/// can miss an occurrence — the only cost of a longer tick is how late a run
/// starts. Thirty seconds bounds that lateness for a wall-clock schedule (a
/// `DailyAt { 9, 0 }` fires by 09:00:30) against one indexed read of a table
/// holding a handful of rows.
const TICK: Duration = Duration::from_secs(30);

/// Drives scheduled runs and owns the keep-awake hold taken on their behalf.
pub struct Scheduler {
    store: ScheduleStore,
    /// Injected rather than reached for via [`crate::agent_awake::global`] so the
    /// reconcile logic can be tested against a mock backend instead of real power
    /// management.
    awake_source: Arc<AgentAwake>,
    /// The live hold, or `None` when nothing is armed. `Option` rather than a
    /// count because one assertion covers every schedule.
    awake: Mutex<Option<AwakeHold>>,
    /// Ids of schedules a tick is currently firing. Guards against the same
    /// occurrence being launched twice — nothing is recorded until `mark_fired`,
    /// so without this a second tick that overlapped a slow launch would see the
    /// schedule still due and open a *second* agent session for the same work.
    /// The `(schedule_id, fired_at)` key dedupes the run *record*, not the side
    /// effect. Cleared by an RAII guard so a panic mid-fire cannot wedge a
    /// schedule out of firing for the rest of the process.
    in_flight: Mutex<HashSet<String>>,
}

impl Scheduler {
    pub fn new(store: ScheduleStore, awake_source: Arc<AgentAwake>) -> Self {
        Self {
            store,
            awake_source,
            awake: Mutex::new(None),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Start the tick loop. Detached: it runs for the process's lifetime and has
    /// no owner to hang it off.
    ///
    /// Ticks before the first sleep so a schedule that came due while the app was
    /// closed fires at boot rather than one [`TICK`] later.
    pub fn install(store: ScheduleStore, cx: &mut gpui::App) {
        let scheduler = Arc::new(Self::new(store, crate::agent_awake::global().clone()));
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            loop {
                scheduler.tick(cx).await;
                cx.background_executor().timer(TICK).await;
            }
        })
        .detach();
    }

    /// One pass: reconcile the keep-awake hold, then fire whatever is due.
    ///
    /// The reads (which is armed, which is due, which already ran) happen off the
    /// GPUI thread — a database busy with a writer would otherwise stall a frame.
    /// The fires themselves run *on* the GPUI thread, because opening a tab and
    /// spawning the agent is view-layer work; each is awaited in turn rather than
    /// spawned, so a burst of due schedules opens tabs one at a time instead of
    /// all at once.
    async fn tick(self: &Arc<Self>, cx: &mut gpui::AsyncApp) {
        let now = Local::now();
        let this = self.clone();
        let to_fire = cx
            .background_executor()
            .spawn(async move {
                this.reconcile_awake();
                this.select_due(now)
            })
            .await;
        for schedule in to_fire {
            self.fire(schedule, now, cx).await;
        }
    }

    /// The schedules to actually fire this tick: due, not already fired for this
    /// occurrence, and not already being fired by a still-running earlier tick.
    /// Pure filtering lives in [`plan_fire`]; this is the impure shell that reads
    /// the store.
    fn select_due(&self, now: DateTime<Local>) -> Vec<Schedule> {
        let due = match self.store.due(now) {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!(%err, "scheduler: could not read due schedules");
                return Vec::new();
            }
        };
        // Snapshot rather than hold the lock across the `already_ran` reads below:
        // the set is tiny, and holding a mutex over SQLite I/O is a habit worth
        // not forming even where it is currently uncontended.
        let in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        plan_fire(due, &in_flight, |s| {
            self.store.already_ran(&s.id, s.next_fire_at).unwrap_or_else(|err| {
                // An unreadable history counts as "not yet run". Firing again is
                // recoverable — `mark_fired`'s idempotency key still collapses the
                // duplicate record — whereas skipping would drop the occurrence
                // outright.
                tracing::warn!(%err, id = %s.id, "scheduler: could not check run history");
                false
            })
        })
    }

    /// Fire one due schedule: open its session, then record the outcome and arm
    /// the next slot. Runs on the GPUI thread — opening a tab is view work.
    async fn fire(&self, schedule: Schedule, now: DateTime<Local>, cx: &mut gpui::AsyncApp) {
        // Mark in-flight for the whole fire; the guard clears it on any exit,
        // including a panic. A second attempt while this one is running is a
        // no-op — the sequential loop should never produce one, but if a future
        // change spawns fires concurrently this is what stops a double-launch.
        {
            let mut set = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            if !set.insert(schedule.id.clone()) {
                return;
            }
        }
        let _guard = InFlightGuard { set: &self.in_flight, id: schedule.id.clone() };

        let cwd = schedule.cwd.clone();
        let agent_id = schedule.agent_id.clone();
        let prompt = schedule.prompt.clone();
        // `AsyncApp::update` returns the closure's value directly in this gpui, so
        // the launch's own error reaches us unflattened — the same shape the
        // remote launch bridge relies on.
        let opened = cx.update(|cx| {
            crate::remote_control::launch_bridge::open_session(
                &cwd,
                agent_id.as_deref(),
                Some(prompt),
                cx,
            )
        });
        match opened {
            Ok(session_id) => self.record(&schedule, RunOutcome::Ok, Some(&session_id), None, now),
            // No window to host the tab. Treat as "not yet": leave the schedule
            // due so it fires when a window reappears, rather than recording a
            // miss for something recoverable. A deliberate product call — the
            // alternative is to record `Failed` and skip the occurrence entirely.
            Err(LaunchError::Unavailable) => {
                tracing::info!(id = %schedule.id, "scheduler: no window to fire into; will retry");
            }
            Err(err) => {
                self.record(&schedule, RunOutcome::Failed, None, Some(launch_error_detail(&err)), now)
            }
        }
    }

    /// Record a fire and arm the next slot. `fired_at` is the **scheduled**
    /// instant, not `now`: that is the idempotency key, and using `now` would mint
    /// a fresh key every attempt and defeat the restart guard entirely.
    fn record(
        &self,
        schedule: &Schedule,
        outcome: RunOutcome,
        session_id: Option<&str>,
        detail: Option<&str>,
        now: DateTime<Local>,
    ) {
        if let Err(err) =
            self.store.mark_fired(schedule, schedule.next_fire_at, outcome, session_id, detail, now)
        {
            tracing::warn!(%err, id = %schedule.id, "scheduler: could not record run");
        }
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

/// Which due schedules to fire: drop the ones a tick is already firing and the
/// ones this occurrence already ran. Pure so the double-launch guard can be
/// tested without GPUI or a database — the fire path itself needs both.
fn plan_fire(
    due: Vec<Schedule>,
    in_flight: &HashSet<String>,
    already_ran: impl Fn(&Schedule) -> bool,
) -> Vec<Schedule> {
    due.into_iter()
        .filter(|s| !in_flight.contains(&s.id))
        .filter(|s| !already_ran(s))
        .collect()
}

/// Clears a schedule's in-flight mark on drop, so an early return or a panic in
/// the fire path cannot leave an id stuck in-flight and silently disable that
/// schedule for the rest of the process.
struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
    }
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
        // Not recorded (the caller treats it as "not yet"), but the match must be
        // total.
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
        fn create(&self, _name: &str) -> Option<u32> {
            let mut created = self.created.lock().unwrap();
            *created += 1;
            Some(*created)
        }
        fn release(&self, _id: u32) {}
    }

    /// A scheduler over an empty in-memory database, plus the backend so a test
    /// can count how many assertions were ever created.
    fn scheduler() -> (Scheduler, Arc<MockBackend>, Arc<AgentAwake>) {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));
        let store = ScheduleStore::new(db.conn());
        (Scheduler::new(store, awake.clone()), backend, awake)
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
        let (sched, _backend, awake) = scheduler();
        sched.reconcile_awake();
        assert!(!awake.asserted(), "nothing armed, nothing to stay awake for");

        let created = sched
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        sched.reconcile_awake();
        assert!(awake.asserted(), "an armed schedule holds it");

        sched.store.delete(&created.id).expect("delete");
        sched.reconcile_awake();
        assert!(!awake.asserted(), "released once nothing is armed");
    }

    /// Reconciling repeatedly must not stack assertions. A second `create` would
    /// leak the first — and since the OS refcounts nothing and `pmset` shows one
    /// entry per assertion name, the leak would be invisible.
    #[test]
    fn reconciling_twice_does_not_create_a_second_assertion() {
        let (sched, backend, _awake) = scheduler();
        sched.store.create(a_schedule("morning"), Local::now()).expect("create");

        sched.reconcile_awake();
        sched.reconcile_awake();
        sched.reconcile_awake();

        assert_eq!(*backend.created.lock().unwrap(), 1, "one assertion, not three");
    }

    /// A disabled schedule is not a reason to keep a laptop awake.
    #[test]
    fn a_disabled_schedule_holds_nothing() {
        let (sched, _backend, awake) = scheduler();
        let created = sched
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        sched
            .store
            .set_enabled(&created.id, false, Local::now())
            .expect("disable");

        sched.reconcile_awake();
        assert!(!awake.asserted());
    }

    /// Re-enabling re-takes the hold, so a schedule switched back on is not left
    /// armed behind a machine that will sleep through it.
    #[test]
    fn re_enabling_retakes_the_hold() {
        let (sched, _backend, awake) = scheduler();
        let created = sched
            .store
            .create(a_schedule("morning"), Local::now())
            .expect("create");
        sched.store.set_enabled(&created.id, false, Local::now()).expect("disable");
        sched.reconcile_awake();
        assert!(!awake.asserted());

        sched.store.set_enabled(&created.id, true, Local::now()).expect("enable");
        sched.reconcile_awake();
        assert!(awake.asserted());
    }

    /// A schedule armed for the future is not yet due — `select_due` must not
    /// report it merely because it exists.
    #[test]
    fn select_due_reports_only_what_is_due() {
        let (sched, _backend, _awake) = scheduler();
        let now = Local::now();
        sched.store.create(a_schedule("morning"), now).expect("create");

        assert!(sched.select_due(now).is_empty(), "next fire is in the future");

        // Far enough ahead that the next daily slot has certainly passed.
        let due = sched.select_due(now + chrono::Duration::days(2));
        assert_eq!(due.len(), 1);
    }

    /// The double-launch guard. `plan_fire` drops the schedules a prior tick is
    /// still firing and the occurrences that already ran, and selects the rest.
    #[test]
    fn plan_fire_skips_in_flight_and_already_ran() {
        let s = |id: &str| Schedule {
            id: id.to_string(),
            name: id.to_string(),
            cwd: "/tmp".into(),
            prompt: "x".into(),
            agent_id: None,
            recurrence: Recurrence::DailyAt { hour: 9, minute: 0 },
            enabled: true,
            next_fire_at: Local::now(),
        };
        let due = vec![s("a"), s("b"), s("c")];
        let in_flight: HashSet<String> = ["b".to_string()].into_iter().collect();

        // `a` is firing-eligible, `b` is in flight, `c` already ran.
        let picked = plan_fire(due, &in_flight, |sch| sch.id == "c");
        let ids: Vec<&str> = picked.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a"], "only the schedule that is neither in-flight nor already-run");
    }

    /// Two ticks over the same still-in-flight schedule select it exactly once —
    /// the highest-value guard against opening two sessions for one occurrence.
    #[test]
    fn a_still_in_flight_schedule_is_not_selected_again() {
        let sch = Schedule {
            id: "a".into(),
            name: "a".into(),
            cwd: "/tmp".into(),
            prompt: "x".into(),
            agent_id: None,
            recurrence: Recurrence::DailyAt { hour: 9, minute: 0 },
            enabled: true,
            next_fire_at: Local::now(),
        };

        // Tick A: nothing in flight yet → selected.
        let tick_a = plan_fire(vec![sch.clone()], &HashSet::new(), |_| false);
        assert_eq!(tick_a.len(), 1, "the first tick fires it");

        // Tick B while A's fire is still running → skipped.
        let in_flight: HashSet<String> = ["a".to_string()].into_iter().collect();
        let tick_b = plan_fire(vec![sch], &in_flight, |_| false);
        assert!(tick_b.is_empty(), "the second tick does not fire it again");
    }

    /// A failed run must still arm the next slot, or a broken schedule would come
    /// up due on every single tick and retry in a hot loop.
    #[test]
    fn a_failed_run_arms_the_next_slot() {
        let (sched, _backend, _awake) = scheduler();
        let now = Local::now();
        let created = sched.store.create(a_schedule("morning"), now).expect("create");
        // Two days on so the daily slot has passed and it is due.
        let later = now + chrono::Duration::days(2);
        let due = sched.select_due(later);
        assert_eq!(due.len(), 1);

        sched.record(&due[0], RunOutcome::Failed, None, Some("boom"), later);

        assert!(sched.select_due(later).is_empty(), "re-armed forward, not still due");
        let runs = sched.store.runs(&created.id, 10).expect("runs");
        assert_eq!(runs[0].outcome, RunOutcome::Failed);
        assert_eq!(runs[0].detail.as_deref(), Some("boom"));
    }
}
