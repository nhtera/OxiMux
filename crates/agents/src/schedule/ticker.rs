//! The shared tick engine behind scheduled agent runs — one implementation for
//! every host, desktop or headless.
//!
//! **Level-triggered, not edge-triggered.** `due()` asks `next_fire_at <= now`
//! rather than "did a boundary just pass", so a tick that arrives late — or does
//! not arrive at all, because the machine slept through it — still catches the
//! occurrence on the next pass. That is what makes [`TICK`] a latency knob
//! rather than a correctness one, and it is why nothing here tries to align to
//! wall-clock boundaries.
//!
//! What differs per host is *how a fire happens* — the desktop opens a tab on
//! its UI thread, the headless host spawns straight into its registry — so that
//! is the [`ScheduleFirer`] seam, and everything else (due-selection, the
//! double-launch guards, the durable claim, run recording) lives here once.
//! The loop shell itself also stays host-side: the desktop's executor is GPUI
//! and the headless host's is tokio, and a four-line `loop { tick; sleep }`
//! per host is cheaper than abstracting over their timers. Hosts call
//! [`Ticker::tick`] on their own cadence.
//!
//! **One ticker per data directory.** Ownership is decided by an advisory lock
//! on [`TICKER_LOCK_FILENAME`] (via `oximux-single-instance`), acquired before
//! the loop starts; a process that loses the contest runs no ticker and says so
//! once. The lock is what makes cross-process double-fire impossible; the
//! durable claim in [`ScheduleStore::begin_fire`] is the belt to that brace,
//! and it is also what lets a manual run-now and the tick loop coexist.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};

use super::store::{RunOutcome, Schedule, ScheduleRun, ScheduleStore, ScheduleTarget};

/// How often a host should look for due schedules.
///
/// `Recurrence::EveryMinutes` has a five-minute floor, so no interval below that
/// can miss an occurrence — the only cost of a longer tick is how late a run
/// starts. Thirty seconds bounds that lateness for a wall-clock schedule (a
/// `DailyAt { 9, 0 }` fires by 09:00:30) against one indexed read of a table
/// holding a handful of rows.
pub const TICK: Duration = Duration::from_secs(30);

/// The role-lock file, under the data directory, that decides which process
/// owns this data dir's ticker. Beside the GUI's own `oximux-gui.lock`.
pub const TICKER_LOCK_FILENAME: &str = "schedule-ticker.lock";

/// How one fire attempt ended, as the firer saw it.
pub enum FireOutcome {
    /// The run started; `session_id` is the session it runs in, when known.
    Completed { session_id: Option<String> },
    /// The run could not start. `session_id` is present when a session opened
    /// but the run still failed (say, the prompt could not be delivered into
    /// it), so the history row can still name where to look.
    Failed { session_id: Option<String>, detail: String },
    /// The host cannot fire *right now* but expects to be able to later — the
    /// desktop with no window open, a host mid-drain. The occurrence stays due
    /// and the next tick retries it; nothing is recorded.
    NotNow,
}

/// How a host performs one fire. Implementations must be cheap to call and
/// safe to call sequentially; the ticker never fires concurrently.
#[async_trait::async_trait]
pub trait ScheduleFirer: Send + Sync {
    /// Start one run. `target` is the schedule's own target, passed separately
    /// so the branch a host must implement (fresh session vs existing session)
    /// is visible in the signature. A host that cannot serve
    /// [`ScheduleTarget::ExistingSession`] yet returns a
    /// [`FireOutcome::Failed`] saying so.
    async fn fire(&self, schedule: &Schedule, target: &ScheduleTarget) -> FireOutcome;

    /// Host hook run once per tick, before due-selection — the desktop
    /// reconciles its keep-awake hold here. Default: nothing.
    fn on_tick(&self) {}
}

/// Called with every run the ticker records (scheduled and manual, success and
/// failure) — the seam a host uses to push run results to subscribers.
pub type RecordedHook = Arc<dyn Fn(&ScheduleRun) + Send + Sync>;

/// Fire a schedule into a session that is already open — the heartbeat path.
///
/// Shared by every host because there is nothing host-specific about it: the
/// session is already live in the registry, so no tab opens and no process
/// spawns. That is the whole point of the [`ScheduleTarget::ExistingSession`]
/// arm — an agent asking to be woken inside its own conversation, with the
/// context it has already built, rather than in a fresh one that knows nothing.
///
/// A session that is not live is a **failure**, not a [`FireOutcome::NotNow`]:
/// `NotNow` means "I expect to be able to shortly", and a dormant session's
/// agent is not coming back on its own. Recording it arms the next slot, so a
/// heartbeat on a closed session reports once per cadence instead of spinning.
/// A target whose session is gone for good is disabled by
/// [`ScheduleStore::sweep_orphaned_targets`] at boot.
pub fn nudge_existing_session(
    registry: &crate::session_registry::SessionRegistry,
    schedule: &Schedule,
    session_id: &str,
) -> FireOutcome {
    let Some(handle) = registry.get(session_id) else {
        return FireOutcome::Failed {
            session_id: Some(session_id.to_string()),
            detail: "that session is not running, so there was nothing to wake".into(),
        };
    };
    match handle.send_prompt(&heartbeat_prompt(schedule), &[]) {
        Ok(()) => FireOutcome::Completed { session_id: Some(session_id.to_string()) },
        Err(err) => {
            tracing::warn!(%err, session_id, "heartbeat prompt could not be delivered");
            FireOutcome::Failed {
                session_id: Some(session_id.to_string()),
                detail: "the session is running but the prompt could not be delivered".into(),
            }
        }
    }
}

/// The prompt a heartbeat delivers: the schedule's own text under a preamble
/// naming where it came from.
///
/// The preamble is load-bearing, not decoration. Without it the agent reads a
/// wake-up as the user having typed something, and answers the *person* —
/// asking what they meant, or apologising for the interruption. Saying plainly
/// that this is a timer the agent itself armed is what makes it act instead.
fn heartbeat_prompt(schedule: &Schedule) -> String {
    format!(
        "[oximux heartbeat: {}] This is a scheduled wake-up, delivered by the host on \
         the cadence set for this session. Nobody typed it. Act on it directly and \
         reply only with what the instruction below asks for.\n\n{}",
        schedule.name, schedule.prompt
    )
}

/// Why a manual run-now was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum RunNowError {
    /// No schedule has that id.
    NotFound,
    /// The schedule is already firing (tick or another run-now).
    Busy,
    /// The host declined ([`FireOutcome::NotNow`]) — nothing was recorded.
    NotNow,
    /// The store could not record the run. The text is for the host's log, not
    /// the wire.
    Store(String),
}

/// Drives scheduled runs against one store through one firer.
pub struct Ticker {
    store: ScheduleStore,
    firer: Arc<dyn ScheduleFirer>,
    /// Ids a fire is currently running for. Guards against the same occurrence
    /// being launched twice by overlapping ticks or a run-now racing the loop
    /// — nothing is settled until the fire finishes, and the durable claim
    /// alone would let a *released* slot be retried while its first attempt is
    /// still in flight. Cleared by an RAII guard so a panic mid-fire cannot
    /// wedge a schedule out of firing for the rest of the process.
    in_flight: Mutex<HashSet<String>>,
    on_recorded: Option<RecordedHook>,
}

impl Ticker {
    pub fn new(store: ScheduleStore, firer: Arc<dyn ScheduleFirer>) -> Self {
        Self { store, firer, in_flight: Mutex::new(HashSet::new()), on_recorded: None }
    }

    /// Install the run-recorded hook.
    pub fn with_recorded_hook(mut self, hook: RecordedHook) -> Self {
        self.on_recorded = Some(hook);
        self
    }

    /// One pass: the host hook, then fire whatever is due — each awaited in
    /// turn rather than spawned, so a burst of due schedules starts one at a
    /// time instead of all at once.
    pub async fn tick(&self, now: DateTime<Local>) {
        self.firer.on_tick();
        for schedule in self.select_due(now) {
            self.fire_one(schedule, now).await;
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
                // recoverable — the claim's idempotency key still collapses the
                // duplicate — whereas skipping would drop the occurrence
                // outright.
                tracing::warn!(%err, id = %s.id, "scheduler: could not check run history");
                false
            })
        })
    }

    /// Fire one due schedule: claim the occurrence, run it, settle the claim
    /// and arm the next slot.
    async fn fire_one(&self, schedule: Schedule, now: DateTime<Local>) {
        {
            let mut set = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            if !set.insert(schedule.id.clone()) {
                return;
            }
        }
        let _guard = InFlightGuard { set: &self.in_flight, id: schedule.id.clone() };

        // `fired_at` is the **scheduled** instant, not `now`: that is the
        // idempotency key, and using `now` would mint a fresh key every attempt
        // and defeat the restart guard entirely.
        let fired_at = schedule.next_fire_at;
        match self.store.begin_fire(&schedule.id, fired_at) {
            Ok(true) => {}
            // Another process (or a prior attempt) holds the claim.
            Ok(false) => return,
            Err(err) => {
                tracing::warn!(%err, id = %schedule.id, "scheduler: could not claim the run");
                return;
            }
        }

        match self.firer.fire(&schedule, &schedule.target).await {
            FireOutcome::Completed { session_id } => {
                self.settle_scheduled(&schedule, fired_at, RunOutcome::Ok, session_id, None, now);
            }
            FireOutcome::Failed { session_id, detail } => {
                self.settle_scheduled(
                    &schedule,
                    fired_at,
                    RunOutcome::Failed,
                    session_id,
                    Some(detail),
                    now,
                );
            }
            FireOutcome::NotNow => {
                // Leave the schedule due so it fires when the host can host it,
                // rather than recording a miss for something recoverable.
                tracing::info!(id = %schedule.id, "scheduler: host cannot fire right now; will retry");
                if let Err(err) = self.store.release_fire(&schedule.id, fired_at) {
                    tracing::warn!(%err, id = %schedule.id, "scheduler: could not release the claim");
                }
            }
        }
    }

    /// Fire one schedule immediately, recording the run without advancing its
    /// cadence — the manual run-now verb. Works on disabled schedules too: an
    /// explicit "run it now" outranks the pause, which only silences the clock.
    pub async fn run_now(
        &self,
        schedule_id: &str,
        now: DateTime<Local>,
    ) -> Result<ScheduleRun, RunNowError> {
        let schedule = self
            .store
            .get(schedule_id)
            .map_err(|e| RunNowError::Store(e.to_string()))?
            .ok_or(RunNowError::NotFound)?;

        {
            let mut set = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            if !set.insert(schedule.id.clone()) {
                return Err(RunNowError::Busy);
            }
        }
        let _guard = InFlightGuard { set: &self.in_flight, id: schedule.id.clone() };

        // Claimed at the wall-clock instant, not the armed slot: a manual run
        // is its own occurrence, and must neither consume the scheduled one
        // nor collide with it.
        match self.store.begin_fire(&schedule.id, now) {
            Ok(true) => {}
            Ok(false) => return Err(RunNowError::Busy),
            Err(e) => return Err(RunNowError::Store(e.to_string())),
        }

        let (outcome, session_id, detail) =
            match self.firer.fire(&schedule, &schedule.target).await {
                FireOutcome::Completed { session_id } => (RunOutcome::Ok, session_id, None),
                FireOutcome::Failed { session_id, detail } => {
                    (RunOutcome::Failed, session_id, Some(detail))
                }
                FireOutcome::NotNow => {
                    if let Err(err) = self.store.release_fire(&schedule.id, now) {
                        tracing::warn!(%err, id = %schedule.id, "scheduler: could not release the claim");
                    }
                    return Err(RunNowError::NotNow);
                }
            };

        if let Err(e) = self.store.finish_manual(
            &schedule.id,
            now,
            outcome,
            session_id.as_deref(),
            detail.as_deref(),
        ) {
            return Err(RunNowError::Store(e.to_string()));
        }
        let run = ScheduleRun {
            schedule_id: schedule.id.clone(),
            fired_at: now,
            outcome,
            session_id,
            detail,
        };
        if let Some(hook) = &self.on_recorded {
            hook(&run);
        }
        Ok(run)
    }

    /// Settle a scheduled fire's claim, arm the next slot, and notify.
    fn settle_scheduled(
        &self,
        schedule: &Schedule,
        fired_at: DateTime<Local>,
        outcome: RunOutcome,
        session_id: Option<String>,
        detail: Option<String>,
        now: DateTime<Local>,
    ) {
        if let Err(err) = self.store.finish_scheduled(
            schedule,
            fired_at,
            outcome,
            session_id.as_deref(),
            detail.as_deref(),
            now,
        ) {
            tracing::warn!(%err, id = %schedule.id, "scheduler: could not record run");
            return;
        }
        if let Some(hook) = &self.on_recorded {
            hook(&ScheduleRun {
                schedule_id: schedule.id.clone(),
                fired_at,
                outcome,
                session_id,
                detail,
            });
        }
    }
}

/// Which due schedules to fire: drop the ones a tick is already firing and the
/// ones this occurrence already ran. Pure so the double-launch guard can be
/// tested without a firer or a database — the fire path itself needs both.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{NewSchedule, Recurrence};
    use chrono::TimeZone;

    fn store() -> ScheduleStore {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        ScheduleStore::new(db.conn())
    }

    fn at(s: &str) -> DateTime<Local> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").expect("parse");
        Local.from_local_datetime(&naive).single().expect("unambiguous")
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

    /// A firer whose answers are scripted, counting calls.
    struct ScriptedFirer {
        outcome: Mutex<Vec<FireOutcome>>,
        fired: Mutex<Vec<String>>,
    }

    impl ScriptedFirer {
        fn always(outcome: fn() -> FireOutcome, times: usize) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new((0..times).map(|_| outcome()).collect()),
                fired: Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> usize {
            self.fired.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ScheduleFirer for ScriptedFirer {
        async fn fire(&self, schedule: &Schedule, _target: &ScheduleTarget) -> FireOutcome {
            self.fired.lock().unwrap().push(schedule.id.clone());
            self.outcome.lock().unwrap().pop().expect("script exhausted")
        }
    }

    fn ok_outcome() -> FireOutcome {
        FireOutcome::Completed { session_id: Some("sess-1".into()) }
    }

    /// A due schedule fires once, its run is recorded, and it is re-armed —
    /// the second tick has nothing to do.
    #[tokio::test]
    async fn a_due_schedule_fires_once_and_rearms() {
        let store = store();
        let made = store.create(a_schedule("morning"), at("2026-07-21 08:00:00")).unwrap();
        let firer = ScriptedFirer::always(ok_outcome, 1);
        let ticker = Ticker::new(store, firer.clone());

        let now = at("2026-07-21 09:00:10");
        ticker.tick(now).await;
        assert_eq!(firer.calls(), 1, "fired once");
        ticker.tick(now).await;
        assert_eq!(firer.calls(), 1, "the second tick does not re-fire the slot");

        let runs = ticker.store.runs(&made.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, RunOutcome::Ok);
        assert_eq!(runs[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(runs[0].fired_at, made.next_fire_at, "keyed on the scheduled instant");
    }

    /// A host that cannot fire right now leaves the occurrence due: nothing is
    /// recorded, and the next tick retries the same slot.
    #[tokio::test]
    async fn a_declined_fire_is_retried_next_tick() {
        let store = store();
        let made = store.create(a_schedule("morning"), at("2026-07-21 08:00:00")).unwrap();
        let firer = Arc::new(ScriptedFirer {
            // Popped back-to-front: first tick declines, second completes.
            outcome: Mutex::new(vec![ok_outcome(), FireOutcome::NotNow]),
            fired: Mutex::new(Vec::new()),
        });
        let ticker = Ticker::new(store, firer.clone());

        let now = at("2026-07-21 09:00:10");
        ticker.tick(now).await;
        assert!(ticker.store.runs(&made.id, 10).unwrap().is_empty(), "a decline records nothing");

        ticker.tick(now).await;
        assert_eq!(firer.calls(), 2, "retried");
        assert_eq!(ticker.store.runs(&made.id, 10).unwrap().len(), 1, "the retry recorded");
    }

    /// A failed fire still arms the next slot, or a broken schedule would come
    /// up due on every tick and retry in a hot loop.
    #[tokio::test]
    async fn a_failed_fire_arms_the_next_slot() {
        let store = store();
        let made = store.create(a_schedule("morning"), at("2026-07-21 08:00:00")).unwrap();
        let firer = ScriptedFirer::always(
            || FireOutcome::Failed { session_id: None, detail: "boom".into() },
            1,
        );
        let ticker = Ticker::new(store, firer.clone());

        let now = at("2026-07-21 09:00:10");
        ticker.tick(now).await;
        ticker.tick(now).await;
        assert_eq!(firer.calls(), 1, "not retried in a hot loop");

        let runs = ticker.store.runs(&made.id, 10).unwrap();
        assert_eq!(runs[0].outcome, RunOutcome::Failed);
        assert_eq!(runs[0].detail.as_deref(), Some("boom"));
    }

    /// run-now records a manual run and leaves the cadence untouched.
    #[tokio::test]
    async fn run_now_records_without_advancing_cadence() {
        let store = store();
        let made = store.create(a_schedule("morning"), at("2026-07-21 08:00:00")).unwrap();
        let armed = made.next_fire_at;
        let firer = ScriptedFirer::always(ok_outcome, 1);
        let ticker = Ticker::new(store, firer);

        let run = ticker.run_now(&made.id, at("2026-07-21 08:30:00")).await.expect("ran");
        assert_eq!(run.outcome, RunOutcome::Ok);
        assert_eq!(run.session_id.as_deref(), Some("sess-1"));

        let after = ticker.store.get(&made.id).unwrap().unwrap();
        assert_eq!(after.next_fire_at, armed, "cadence untouched");
        assert_eq!(ticker.store.runs(&made.id, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_now_of_an_unknown_schedule_is_not_found() {
        let ticker = Ticker::new(store(), ScriptedFirer::always(ok_outcome, 0));
        let err = ticker.run_now("sch-nope", at("2026-07-21 08:30:00")).await.unwrap_err();
        assert_eq!(err, RunNowError::NotFound);
    }

    /// The recorded hook sees every settled run — the push seam.
    #[tokio::test]
    async fn the_recorded_hook_sees_scheduled_and_manual_runs() {
        let store = store();
        let made = store.create(a_schedule("morning"), at("2026-07-21 08:00:00")).unwrap();
        let seen: Arc<Mutex<Vec<ScheduleRun>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let ticker = Ticker::new(store, ScriptedFirer::always(ok_outcome, 2))
            .with_recorded_hook(Arc::new(move |run| sink.lock().unwrap().push(run.clone())));

        ticker.tick(at("2026-07-21 09:00:10")).await;
        ticker.run_now(&made.id, at("2026-07-21 10:00:00")).await.expect("ran");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one scheduled, one manual");
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
            target: ScheduleTarget::NewSession,
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
            target: ScheduleTarget::NewSession,
        };

        // Tick A: nothing in flight yet → selected.
        let tick_a = plan_fire(vec![sch.clone()], &HashSet::new(), |_| false);
        assert_eq!(tick_a.len(), 1, "the first tick fires it");

        // Tick B while A's fire is still running → skipped.
        let in_flight: HashSet<String> = ["a".to_string()].into_iter().collect();
        let tick_b = plan_fire(vec![sch], &in_flight, |_| false);
        assert!(tick_b.is_empty(), "the second tick does not fire it again");
    }
}
