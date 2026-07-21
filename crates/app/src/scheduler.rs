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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};
use oximux_agents::schedule::{Schedule, ScheduleStore};

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
}

impl Scheduler {
    pub fn new(store: ScheduleStore, awake_source: Arc<AgentAwake>) -> Self {
        Self {
            store,
            awake_source,
            awake: Mutex::new(None),
        }
    }

    /// Start the tick loop. Detached: it runs for the process's lifetime and has
    /// no owner to hang it off.
    pub fn install(store: ScheduleStore, cx: &mut gpui::App) {
        let scheduler = Arc::new(Self::new(store, crate::agent_awake::global().clone()));
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            loop {
                let this = scheduler.clone();
                // SQLite reads, so off the GPUI thread — a database busy with a
                // writer would otherwise stall a frame.
                let due = cx
                    .background_executor()
                    .spawn(async move { this.poll(Local::now()) })
                    .await;
                if !due.is_empty() {
                    // Firing lands here. Until then, say so rather than dropping
                    // it silently — a schedule that came due and did nothing is
                    // exactly the symptom someone would be debugging.
                    tracing::info!(
                        count = due.len(),
                        ids = ?due.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                        "scheduler: schedules due (firing not yet implemented)"
                    );
                }
                cx.background_executor().timer(TICK).await;
            }
        })
        .detach();
    }

    /// One pass: keep the awake hold in sync with what is armed, and report what
    /// is due. Runs off the GPUI thread — both halves touch SQLite.
    ///
    /// Polls before the first sleep so a schedule that came due while the app was
    /// closed fires at boot rather than one [`TICK`] later.
    fn poll(&self, now: DateTime<Local>) -> Vec<Schedule> {
        self.reconcile_awake();
        match self.store.due(now) {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!(%err, "scheduler: could not read due schedules");
                Vec::new()
            }
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

    /// A schedule armed for the future is not yet due — `poll` must not report it
    /// merely because it exists.
    #[test]
    fn poll_reports_only_what_is_due() {
        let (sched, _backend, _awake) = scheduler();
        let now = Local::now();
        sched.store.create(a_schedule("morning"), now).expect("create");

        assert!(sched.poll(now).is_empty(), "next fire is in the future");

        // Far enough ahead that the next daily slot has certainly passed.
        let due = sched.poll(now + chrono::Duration::days(2));
        assert_eq!(due.len(), 1);
    }
}
