//! Persistence for scheduled runs, and the run history each fire appends to.
//!
//! Holds no opinion about *when* anything fires — that is [`super::recurrence`]
//! — and none about how a run is performed. It answers "which schedules are due"
//! and records what happened, so the tick path that does the firing stays small
//! and the parts with real logic stay testable against an in-memory database.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, params};

use super::recurrence::Recurrence;

/// How many heartbeats one session may arm.
///
/// Low on purpose. A heartbeat is usually created *by* the agent living in the
/// session, so the failure mode is not a user with too many reminders — it is
/// an agent that arms one on every wake-up and compounds. Five is more than any
/// deliberate use needs and small enough that the runaway case hits a wall
/// within minutes.
pub const MAX_HEARTBEATS_PER_SESSION: usize = 5;

/// What a schedule fires INTO.
///
/// The discriminant is settled now so the fire contract never reopens, even
/// though every writer today produces [`ScheduleTarget::NewSession`]: firing a
/// prompt into an existing session is a later capability, and landing the
/// branch point with the table column means adding it changes no signatures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScheduleTarget {
    /// Spawn a fresh session for each fire — the original and default shape.
    #[default]
    NewSession,
    /// Send the prompt into this existing session.
    ExistingSession(String),
}

/// A schedule as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    /// Working directory the run's session opens in. Scoped to a project the
    /// desktop already has, matching what `CreateSession` allows.
    pub cwd: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub recurrence: Recurrence,
    pub enabled: bool,
    /// Materialized rather than recomputed each tick, so a restart resumes the
    /// same schedule instead of shifting every wall-clock run to whenever the
    /// app reopened.
    pub next_fire_at: DateTime<Local>,
    /// Where a fire lands. Persisted as `target_session_id` (NULL = fresh).
    pub target: ScheduleTarget,
}

/// What to create. Separate from [`Schedule`] because the id and the first
/// next-fire are derived, not supplied — a caller that could set them could
/// create a schedule already overdue by a year.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub name: String,
    pub cwd: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub recurrence: Recurrence,
}

/// How one fire turned out.
///
/// A third stored state exists — `'running'`, the durable claim a fire writes
/// before it starts — but it never appears here: [`ScheduleStore::runs`]
/// filters it, and a claim that outlived its process is rewritten to `Failed`
/// by [`ScheduleStore::recover_interrupted`] at boot. Keeping it out of the
/// enum keeps it off the wire, where an old client would choke on an
/// unknown ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Ok,
    Failed,
}

/// One recorded fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRun {
    pub schedule_id: String,
    /// The **scheduled** instant, not when the tick noticed it. Together with
    /// `schedule_id` this is the idempotency key.
    pub fired_at: DateTime<Local>,
    pub outcome: RunOutcome,
    pub session_id: Option<String>,
    pub detail: Option<String>,
}

/// Schedules and their run history, backed by the app's SQLite database.
/// Cloning shares the connection — the ticker and its host's firer read the
/// same rows.
#[derive(Clone)]
pub struct ScheduleStore {
    conn: Arc<Mutex<Connection>>,
}

impl ScheduleStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create a schedule that spawns a fresh session per fire.
    pub fn create(&self, new: NewSchedule, now: DateTime<Local>) -> Result<Schedule> {
        self.create_targeted(new, ScheduleTarget::NewSession, now)
    }

    /// Create a **heartbeat**: a schedule that wakes an existing session rather
    /// than opening a new one.
    ///
    /// Capped per session, because the caller is typically the agent living in
    /// that session and a runaway loop of self-arming wake-ups is the obvious
    /// way for this to go wrong. The cap counts rows, not enabled rows: a paused
    /// heartbeat is still one the agent chose to keep, and letting pauses buy
    /// headroom would make the ceiling meaningless.
    pub fn create_heartbeat(
        &self,
        new: NewSchedule,
        session_id: &str,
        now: DateTime<Local>,
    ) -> Result<Schedule> {
        let existing = self.for_session(session_id)?.len();
        if existing >= MAX_HEARTBEATS_PER_SESSION {
            anyhow::bail!(
                "this session already has {existing} heartbeats \
                 (the limit is {MAX_HEARTBEATS_PER_SESSION}); delete one first"
            );
        }
        self.create_targeted(new, ScheduleTarget::ExistingSession(session_id.to_string()), now)
    }

    /// Create a schedule with an explicit target, computing its first fire from
    /// `now`.
    pub fn create_targeted(
        &self,
        new: NewSchedule,
        target: ScheduleTarget,
        now: DateTime<Local>,
    ) -> Result<Schedule> {
        let target_session_id = match &target {
            ScheduleTarget::NewSession => None,
            ScheduleTarget::ExistingSession(id) => Some(id.clone()),
        };
        let schedule = Schedule {
            // Random rather than sequential: ids appear in run history and cross
            // the wire, and a guessable counter invites a client to address a
            // schedule it was never told about.
            id: format!("sch-{}", random_id()),
            name: new.name,
            cwd: new.cwd,
            prompt: new.prompt,
            agent_id: new.agent_id,
            recurrence: new.recurrence,
            enabled: true,
            next_fire_at: new.recurrence.next_after(now),
            target,
        };
        let (kind, interval, hour, minute, weekday) = decompose(&schedule.recurrence);
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO schedules (id, name, cwd, prompt, agent_id, kind, \
                 interval_minutes, hour, minute, weekday, enabled, next_fire_at, created_at, \
                 target_session_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13)",
                params![
                    schedule.id,
                    schedule.name,
                    schedule.cwd,
                    schedule.prompt,
                    schedule.agent_id,
                    kind,
                    interval,
                    hour,
                    minute,
                    weekday,
                    schedule.next_fire_at.to_rfc3339(),
                    now.to_rfc3339(),
                    target_session_id,
                ],
            )
            .context("insert schedule")?;
        Ok(schedule)
    }

    /// The heartbeats armed for one session, next fire first.
    pub fn for_session(&self, session_id: &str) -> Result<Vec<Schedule>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| s.target == ScheduleTarget::ExistingSession(session_id.to_string()))
            .collect())
    }

    /// Schedules that spawn a fresh session per fire — what "the schedule list"
    /// has always meant.
    ///
    /// Heartbeats are deliberately excluded: they are an agent's own wake-ups,
    /// belonging to one conversation rather than to the host, and a client that
    /// listed them among ordinary schedules would offer edits (change the cwd,
    /// change the agent) that mean nothing for a target that is already open.
    pub fn list_spawning(&self) -> Result<Vec<Schedule>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| s.target == ScheduleTarget::NewSession)
            .collect())
    }

    /// Every schedule, heartbeats included, newest fire first.
    pub fn list(&self) -> Result<Vec<Schedule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, cwd, prompt, agent_id, kind, interval_minutes, hour, \
                 minute, weekday, enabled, next_fire_at, target_session_id \
                 FROM schedules ORDER BY next_fire_at",
            )
            .context("prepare list")?;
        let rows = stmt
            .query_map([], |row| Ok(row_to_schedule(row)))
            .context("query schedules")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read schedules")?;
        // A row whose stored shape does not resolve is skipped rather than
        // failing the whole listing: one unreadable schedule must not hide every
        // other one, which would look like total data loss.
        Ok(rows.into_iter().flatten().collect())
    }

    /// Enabled schedules whose next fire is at or before `now`.
    ///
    /// Returns the **overdue** ones, which after a laptop was closed can mean a
    /// fire time hours in the past. Callers fire once and re-arm from `now`
    /// rather than replaying every missed occurrence — see [`Self::mark_fired`].
    pub fn due(&self, now: DateTime<Local>) -> Result<Vec<Schedule>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| s.enabled && s.next_fire_at <= now)
            .collect())
    }

    /// Claim one occurrence before firing it, durably.
    ///
    /// Inserts the run row with outcome `'running'` — the crash marker
    /// [`Self::recover_interrupted`] rewrites at boot — and returns whether
    /// **this caller** made the claim. `INSERT OR IGNORE` on the
    /// `(schedule_id, fired_at)` key makes the claim atomic: two processes
    /// racing the same occurrence cannot both see `true`, so the loser skips
    /// rather than double-firing.
    pub fn begin_fire(&self, schedule_id: &str, fired_at: DateTime<Local>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO schedule_runs (schedule_id, fired_at, outcome) \
                 VALUES (?1, ?2, 'running')",
                params![schedule_id, fired_at.to_rfc3339()],
            )
            .context("claim run")?;
        Ok(inserted > 0)
    }

    /// Release a claim whose fire never happened (the host declined — no
    /// window to open into, or it is draining). Deleting the `'running'` row
    /// leaves the occurrence claimable again, so the next tick retries it
    /// rather than treating it as already run.
    pub fn release_fire(&self, schedule_id: &str, fired_at: DateTime<Local>) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM schedule_runs \
                 WHERE schedule_id = ?1 AND fired_at = ?2 AND outcome = 'running'",
                params![schedule_id, fired_at.to_rfc3339()],
            )
            .context("release claim")?;
        Ok(())
    }

    /// Settle a **scheduled** fire's claim and arm the next one.
    ///
    /// The next fire is computed from `now`, **not** from the scheduled instant
    /// that just elapsed. After the app was closed for a day, anchoring on the
    /// old time would make a daily schedule fire immediately and then again at
    /// its real time — and an interval schedule would burn through every missed
    /// slot in a burst. Catching up on missed runs is not what a user asking for
    /// "every morning" wants.
    ///
    /// Settling the run row and re-arming happen in **one transaction**: a crash
    /// between them would otherwise either lose the history or leave the
    /// schedule armed at a time already past, firing again on the next tick.
    pub fn finish_scheduled(
        &self,
        schedule: &Schedule,
        fired_at: DateTime<Local>,
        outcome: RunOutcome,
        session_id: Option<&str>,
        detail: Option<&str>,
        now: DateTime<Local>,
    ) -> Result<DateTime<Local>> {
        let next = schedule.recurrence.next_after(now);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin finish_scheduled")?;
        // Settles only this caller's own claim: a row another process already
        // settled (`outcome != 'running'`) stands, which is what makes a
        // restart near a fire boundary unable to overwrite a completed run.
        tx.execute(
            "UPDATE schedule_runs SET outcome = ?3, session_id = ?4, detail = ?5 \
             WHERE schedule_id = ?1 AND fired_at = ?2 AND outcome = 'running'",
            params![
                schedule.id,
                fired_at.to_rfc3339(),
                outcome_text(outcome),
                session_id,
                detail,
            ],
        )
        .context("settle run")?;
        tx.execute(
            "UPDATE schedules SET next_fire_at = ?1 WHERE id = ?2",
            params![next.to_rfc3339(), schedule.id],
        )
        .context("re-arm schedule")?;
        tx.commit().context("commit finish_scheduled")?;
        Ok(next)
    }

    /// Settle a **manual** fire's claim — a run-now, recorded in history but
    /// leaving `next_fire_at` untouched so cadence accounting never notices it.
    pub fn finish_manual(
        &self,
        schedule_id: &str,
        fired_at: DateTime<Local>,
        outcome: RunOutcome,
        session_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE schedule_runs SET outcome = ?3, session_id = ?4, detail = ?5 \
                 WHERE schedule_id = ?1 AND fired_at = ?2 AND outcome = 'running'",
                params![
                    schedule_id,
                    fired_at.to_rfc3339(),
                    outcome_text(outcome),
                    session_id,
                    detail,
                ],
            )
            .context("settle manual run")?;
        Ok(())
    }

    /// Boot-time pass: settle every claim whose process died mid-fire.
    ///
    /// Each surviving `'running'` row becomes a `Failed` run explaining the
    /// restart. A row whose `fired_at` still equals its schedule's armed slot
    /// was a **scheduled** fire that never re-armed — those schedules are armed
    /// forward from `now`, so the occurrence is not silently retried at boot
    /// (no backfill; the missed-run policy stays "skip forward"). A manual
    /// fire's row (its `fired_at` is a wall-clock instant, not the armed slot)
    /// leaves the cadence alone, exactly as its success would have.
    ///
    /// The scheduled-vs-manual distinction is that timestamp equality, not a
    /// stored discriminator. The one ambiguous case — a manual fire whose
    /// wall-clock instant landed *exactly* on the armed slot — is recovered as
    /// if scheduled, which is the safe reading either way: the schedule is
    /// armed forward rather than left claimed-but-due, and the alternative
    /// error (a real scheduled crash misread as manual) would wedge it. A
    /// `kind` column becomes worth its migration only if manual fires ever
    /// need distinct recovery semantics.
    ///
    /// **Only the process that owns the ticker lock may call this** — a second
    /// host recovering rows the lock holder is actively firing would fail runs
    /// that are merely in progress.
    pub fn recover_interrupted(&self, now: DateTime<Local>) -> Result<u32> {
        let schedules = self.list()?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin recover")?;
        let interrupted: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT schedule_id, fired_at FROM schedule_runs WHERE outcome = 'running'",
                )
                .context("prepare recover scan")?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .context("scan interrupted runs")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("read interrupted runs")?
        };
        for (schedule_id, fired_at) in &interrupted {
            tx.execute(
                "UPDATE schedule_runs SET outcome = 'failed', \
                 detail = 'the host restarted before this run completed' \
                 WHERE schedule_id = ?1 AND fired_at = ?2",
                params![schedule_id, fired_at],
            )
            .context("fail interrupted run")?;
            let was_scheduled_slot = schedules
                .iter()
                .find(|s| &s.id == schedule_id)
                .is_some_and(|s| s.next_fire_at.to_rfc3339() == *fired_at);
            if was_scheduled_slot {
                let Some(schedule) = schedules.iter().find(|s| &s.id == schedule_id) else {
                    continue;
                };
                tx.execute(
                    "UPDATE schedules SET next_fire_at = ?1 WHERE id = ?2",
                    params![schedule.recurrence.next_after(now).to_rfc3339(), schedule_id],
                )
                .context("re-arm recovered schedule")?;
            }
        }
        tx.commit().context("commit recover")?;
        Ok(interrupted.len() as u32)
    }

    /// Disable every schedule aimed at a session the host no longer has, and
    /// surface each as a failed run so `schedule logs` explains the silence.
    /// Returns the ids disabled. Nothing writes a session target yet, so this
    /// is armed for the day something does — the sweep is part of the boot
    /// contract, not a later retrofit.
    pub fn sweep_orphaned_targets(
        &self,
        now: DateTime<Local>,
        session_exists: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>> {
        let orphaned: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|s| {
                s.enabled
                    && matches!(&s.target, ScheduleTarget::ExistingSession(sid) if !session_exists(sid))
            })
            .map(|s| s.id)
            .collect();
        if orphaned.is_empty() {
            return Ok(orphaned);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin orphan sweep")?;
        for id in &orphaned {
            tx.execute("UPDATE schedules SET enabled = 0 WHERE id = ?1", params![id])
                .context("disable orphaned schedule")?;
            tx.execute(
                "INSERT OR IGNORE INTO schedule_runs (schedule_id, fired_at, outcome, detail) \
                 VALUES (?1, ?2, 'failed', \
                 'the target session no longer exists; schedule disabled')",
                params![id, now.to_rfc3339()],
            )
            .context("record orphan")?;
        }
        tx.commit().context("commit orphan sweep")?;
        Ok(orphaned)
    }

    /// Whether this exact occurrence was already claimed or ran — the guard a
    /// tick checks before firing, so a restart cannot double-run one slot. A
    /// live `'running'` claim counts: the occurrence is being fired.
    pub fn already_ran(&self, schedule_id: &str, fired_at: DateTime<Local>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM schedule_runs WHERE schedule_id = ?1 AND fired_at = ?2",
                params![schedule_id, fired_at.to_rfc3339()],
                |row| row.get(0),
            )
            .optional()
            .context("check run")?;
        Ok(found.is_some())
    }

    /// Recent **finished** runs for one schedule, newest first.
    ///
    /// In-flight `'running'` claims are filtered: they are a crash marker, not
    /// history, and shipping them would put an ordinal on the wire that
    /// pre-v17 clients cannot decode. A fire settles within seconds, so the
    /// omission is invisible in practice.
    pub fn runs(&self, schedule_id: &str, limit: u32) -> Result<Vec<ScheduleRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT schedule_id, fired_at, outcome, session_id, detail FROM schedule_runs \
                 WHERE schedule_id = ?1 AND outcome != 'running' \
                 ORDER BY fired_at DESC LIMIT ?2",
            )
            .context("prepare runs")?;
        let rows = stmt
            .query_map(params![schedule_id, limit], |row| {
                Ok(ScheduleRun {
                    schedule_id: row.get(0)?,
                    fired_at: parse_local(&row.get::<_, String>(1)?).unwrap_or_else(Local::now),
                    outcome: match row.get::<_, String>(2)?.as_str() {
                        "ok" => RunOutcome::Ok,
                        _ => RunOutcome::Failed,
                    },
                    session_id: row.get(3)?,
                    detail: row.get(4)?,
                })
            })
            .context("query runs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read runs")?;
        Ok(rows)
    }

    /// Enable or disable a schedule.
    ///
    /// Re-enabling recomputes the next fire from `now`. A schedule disabled for
    /// a week would otherwise come back already overdue and fire the instant it
    /// was switched on, which reads as a bug rather than as catching up.
    pub fn set_enabled(&self, id: &str, enabled: bool, now: DateTime<Local>) -> Result<()> {
        let next = if enabled {
            self.list()?
                .into_iter()
                .find(|s| s.id == id)
                .map(|s| s.recurrence.next_after(now))
        } else {
            None
        };
        let conn = self.conn.lock().unwrap();
        match next {
            Some(next) => conn.execute(
                "UPDATE schedules SET enabled = ?1, next_fire_at = ?2 WHERE id = ?3",
                params![enabled as i64, next.to_rfc3339(), id],
            ),
            None => conn.execute(
                "UPDATE schedules SET enabled = ?1 WHERE id = ?2",
                params![enabled as i64, id],
            ),
        }
        .context("set enabled")?;
        Ok(())
    }

    /// Delete a schedule and its history.
    ///
    /// History goes with it: it is only meaningful as this schedule's record,
    /// and orphan rows keyed by a schedule nothing can name would accumulate
    /// with no way to read or clear them.
    pub fn delete(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin delete")?;
        tx.execute("DELETE FROM schedule_runs WHERE schedule_id = ?1", params![id])
            .context("delete runs")?;
        tx.execute("DELETE FROM schedules WHERE id = ?1", params![id])
            .context("delete schedule")?;
        tx.commit().context("commit delete")?;
        Ok(())
    }

    /// Whether any schedule is enabled — the condition the keep-awake hold is
    /// gated on, since a disabled-only set has nothing to stay awake for.
    pub fn any_enabled(&self) -> Result<bool> {
        Ok(self.list()?.iter().any(|s| s.enabled))
    }

    /// One schedule by id. A linear scan over `list` — the table holds a
    /// handful of rows, and reusing the row decoding beats a second query.
    pub fn get(&self, id: &str) -> Result<Option<Schedule>> {
        Ok(self.list()?.into_iter().find(|s| s.id == id))
    }
}

fn outcome_text(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Ok => "ok",
        RunOutcome::Failed => "failed",
    }
}

fn decompose(r: &Recurrence) -> (&'static str, Option<i64>, Option<i64>, Option<i64>, Option<i64>) {
    match *r {
        Recurrence::EveryMinutes(m) => ("interval", Some(m as i64), None, None, None),
        Recurrence::DailyAt { hour, minute } => {
            ("daily", None, Some(hour as i64), Some(minute as i64), None)
        }
        Recurrence::WeeklyAt { weekday, hour, minute } => (
            "weekly",
            None,
            Some(hour as i64),
            Some(minute as i64),
            Some(weekday as i64),
        ),
    }
}

/// Rebuild a schedule from a row. `None` when the stored shape does not resolve
/// to a valid recurrence — a row written by a future version, or one edited by
/// hand. Skipped rather than defaulted: a schedule that silently became "every
/// 5 minutes" because its real rule was unreadable would be worse than absent.
fn row_to_schedule(row: &rusqlite::Row<'_>) -> Option<Schedule> {
    let kind: String = row.get(5).ok()?;
    let recurrence = match kind.as_str() {
        "interval" => Recurrence::every_minutes(row.get::<_, i64>(6).ok()? as u32).ok()?,
        "daily" => {
            Recurrence::daily_at(row.get::<_, i64>(7).ok()? as u8, row.get::<_, i64>(8).ok()? as u8)
                .ok()?
        }
        "weekly" => Recurrence::weekly_at(
            row.get::<_, i64>(9).ok()? as u8,
            row.get::<_, i64>(7).ok()? as u8,
            row.get::<_, i64>(8).ok()? as u8,
        )
        .ok()?,
        _ => return None,
    };
    Some(Schedule {
        id: row.get(0).ok()?,
        name: row.get(1).ok()?,
        cwd: row.get(2).ok()?,
        prompt: row.get(3).ok()?,
        agent_id: row.get(4).ok()?,
        recurrence,
        enabled: row.get::<_, i64>(10).ok()? != 0,
        next_fire_at: parse_local(&row.get::<_, String>(11).ok()?)?,
        target: match row.get::<_, Option<String>>(12).ok()? {
            Some(session_id) => ScheduleTarget::ExistingSession(session_id),
            None => ScheduleTarget::NewSession,
        },
    })
}

fn parse_local(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Local))
}

/// A random id, without pulling in a uuid dependency for one call site.
pub(crate) fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    // Mixed with the address of a fresh allocation so two schedules created in
    // the same nanosecond (possible on a coarse clock) still differ.
    let boxed = Box::new(0u8);
    let addr = Box::into_raw(boxed) as usize;
    // SAFETY: reclaiming the allocation just made above, never used elsewhere.
    unsafe { drop(Box::from_raw(addr as *mut u8)) };
    format!("{nanos:x}{addr:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn store() -> ScheduleStore {
        let conn = Connection::open_in_memory().expect("open memory db");
        for m in oximux_storage::MIGRATIONS {
            conn.execute_batch(m.sql).expect("apply migration");
        }
        ScheduleStore::new(Arc::new(Mutex::new(conn)))
    }

    fn at(s: &str) -> DateTime<Local> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").expect("parse");
        Local.from_local_datetime(&naive).single().expect("unambiguous")
    }

    fn new_schedule(recurrence: Recurrence) -> NewSchedule {
        NewSchedule {
            name: "Nightly tests".into(),
            cwd: "/work/proj".into(),
            prompt: "run the test suite".into(),
            agent_id: None,
            recurrence,
        }
    }

    #[test]
    fn a_created_schedule_round_trips() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");

        let listed = store.list().expect("list");
        assert_eq!(listed, vec![made.clone()]);
        assert_eq!(made.next_fire_at, at("2026-07-21 09:00:00"));
        assert!(made.enabled);
    }

    #[test]
    fn due_returns_only_enabled_schedules_that_have_come_round() {
        let store = store();
        let soon = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store
            .create(new_schedule(Recurrence::daily_at(18, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");

        assert!(store.due(at("2026-07-21 08:30:00")).unwrap().is_empty(), "nothing due yet");

        let due = store.due(at("2026-07-21 09:00:00")).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, soon.id);

        store.set_enabled(&soon.id, false, at("2026-07-21 08:30:00")).unwrap();
        assert!(store.due(at("2026-07-21 09:00:00")).unwrap().is_empty(), "disabled never fires");
    }

    /// The catch-up question. After the app was shut for a day, a daily schedule
    /// must arm for the *next* real occurrence — not fire once for every slot it
    /// missed, and not immediately re-fire because its old time is still past.
    #[test]
    fn a_missed_schedule_arms_forward_rather_than_replaying() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-20 08:00:00"))
            .expect("create");
        assert_eq!(made.next_fire_at, at("2026-07-20 09:00:00"));

        // The app comes back two days later, well past that instant.
        let now = at("2026-07-22 14:00:00");
        let due = store.due(now).unwrap();
        assert_eq!(due.len(), 1, "the missed schedule is due once");

        assert!(store.begin_fire(&due[0].id, due[0].next_fire_at).unwrap(), "claimed");
        let next = store
            .finish_scheduled(&due[0], due[0].next_fire_at, RunOutcome::Ok, Some("sess-1"), None, now)
            .expect("finish fired");
        assert_eq!(next, at("2026-07-23 09:00:00"), "armed forward from now, not from the old slot");
        assert!(store.due(now).unwrap().is_empty(), "no longer due");
    }

    /// The idempotency key. A restart (or a second process) near a fire
    /// boundary must not run one occurrence twice: the claim is first-writer-
    /// wins, and a settled row cannot be overwritten by a later settle.
    #[test]
    fn the_same_occurrence_cannot_be_claimed_twice() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let slot = made.next_fire_at;

        assert!(!store.already_ran(&made.id, slot).unwrap(), "not yet run");
        assert!(store.begin_fire(&made.id, slot).unwrap(), "first claim wins");
        assert!(store.already_ran(&made.id, slot).unwrap(), "a live claim counts as ran");
        assert!(!store.begin_fire(&made.id, slot).unwrap(), "second claim loses");

        store
            .finish_scheduled(&made, slot, RunOutcome::Ok, Some("sess-1"), None, at("2026-07-21 09:00:01"))
            .unwrap();
        // A stray settle after the row is finished must not overwrite it.
        store
            .finish_scheduled(&made, slot, RunOutcome::Ok, Some("sess-2"), None, at("2026-07-21 09:00:02"))
            .expect("late settle is tolerated");
        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs.len(), 1, "one row for one occurrence, saw {runs:?}");
        assert_eq!(runs[0].session_id.as_deref(), Some("sess-1"), "the first run stands");
    }

    /// Releasing a claim makes the occurrence claimable again — the "no window
    /// to fire into, retry next tick" path must not consume the slot.
    #[test]
    fn a_released_claim_can_be_reclaimed() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let slot = made.next_fire_at;

        assert!(store.begin_fire(&made.id, slot).unwrap());
        store.release_fire(&made.id, slot).unwrap();
        assert!(!store.already_ran(&made.id, slot).unwrap(), "released, so not ran");
        assert!(store.begin_fire(&made.id, slot).unwrap(), "claimable again");
    }

    /// An unsettled claim is a crash marker, not history — `runs` must hide it
    /// (it would also put an undecodable ordinal on the wire).
    #[test]
    fn an_inflight_claim_does_not_appear_in_run_history() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store.begin_fire(&made.id, made.next_fire_at).unwrap();

        assert!(store.runs(&made.id, 10).unwrap().is_empty(), "claims are not history");
    }

    /// Boot recovery: a claim whose process died becomes a failed run, and a
    /// schedule whose armed slot died mid-fire is re-armed forward — never
    /// retried at boot.
    #[test]
    fn recovery_fails_interrupted_runs_and_arms_forward() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let slot = made.next_fire_at;
        store.begin_fire(&made.id, slot).unwrap();
        // The process dies here; a later boot recovers.

        let boot = at("2026-07-21 09:05:00");
        assert_eq!(store.recover_interrupted(boot).unwrap(), 1);

        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs[0].outcome, RunOutcome::Failed);
        assert!(runs[0].detail.as_deref().unwrap().contains("restarted"));
        assert!(store.due(boot).unwrap().is_empty(), "armed forward, not still due");
        assert_eq!(
            store.get(&made.id).unwrap().unwrap().next_fire_at,
            at("2026-07-22 09:00:00"),
            "the next real occurrence, no backfill"
        );
    }

    /// A crashed **manual** fire is failed too, but leaves the cadence alone —
    /// exactly as its success would have.
    #[test]
    fn recovery_of_a_manual_claim_leaves_the_cadence_untouched() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let armed = made.next_fire_at;
        // A manual fire claims at its own wall-clock instant, not the slot.
        store.begin_fire(&made.id, at("2026-07-21 08:10:00")).unwrap();

        assert_eq!(store.recover_interrupted(at("2026-07-21 08:20:00")).unwrap(), 1);

        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs[0].outcome, RunOutcome::Failed);
        assert_eq!(
            store.get(&made.id).unwrap().unwrap().next_fire_at,
            armed,
            "the scheduled slot is still armed"
        );
    }

    /// A manual fire settles into history without moving `next_fire_at` — the
    /// run-now contract.
    #[test]
    fn a_manual_run_records_without_advancing_cadence() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let armed = made.next_fire_at;
        let fired = at("2026-07-21 08:15:00");

        assert!(store.begin_fire(&made.id, fired).unwrap());
        store.finish_manual(&made.id, fired, RunOutcome::Ok, Some("sess-9"), None).unwrap();

        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].session_id.as_deref(), Some("sess-9"));
        assert_eq!(store.get(&made.id).unwrap().unwrap().next_fire_at, armed);
    }

    /// The orphan sweep: a schedule aimed at a session the host no longer has
    /// is disabled and the reason lands in its run history.
    #[test]
    fn the_orphan_sweep_disables_and_surfaces() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        // No public writer targets a session yet; aim it by hand as phase-6
        // plumbing will.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE schedules SET target_session_id = 'gone' WHERE id = ?1",
                params![made.id],
            )
            .unwrap();

        let swept = store.sweep_orphaned_targets(at("2026-07-21 08:30:00"), |_| false).unwrap();
        assert_eq!(swept, vec![made.id.clone()]);
        assert!(!store.get(&made.id).unwrap().unwrap().enabled, "disabled");
        let runs = store.runs(&made.id, 10).unwrap();
        assert!(runs[0].detail.as_deref().unwrap().contains("no longer exists"));

        // A live target is left alone.
        let alive = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let swept = store.sweep_orphaned_targets(at("2026-07-21 08:31:00"), |_| true).unwrap();
        assert!(swept.is_empty());
        assert!(store.get(&alive.id).unwrap().unwrap().enabled);
    }

    /// The target discriminant survives storage — NULL is a fresh session, a
    /// session id is an existing one.
    #[test]
    fn the_target_round_trips() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        assert_eq!(store.get(&made.id).unwrap().unwrap().target, ScheduleTarget::NewSession);

        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE schedules SET target_session_id = 'sess-42' WHERE id = ?1",
                params![made.id],
            )
            .unwrap();
        assert_eq!(
            store.get(&made.id).unwrap().unwrap().target,
            ScheduleTarget::ExistingSession("sess-42".into())
        );
    }

    #[test]
    fn a_failed_run_is_recorded_with_its_reason() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store.begin_fire(&made.id, made.next_fire_at).unwrap();
        store
            .finish_scheduled(
                &made,
                made.next_fire_at,
                RunOutcome::Failed,
                None,
                Some("that working directory is not usable"),
                at("2026-07-21 09:00:01"),
            )
            .unwrap();

        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs[0].outcome, RunOutcome::Failed);
        assert_eq!(runs[0].session_id, None);
        assert!(runs[0].detail.as_deref().unwrap().contains("not usable"));
    }

    /// Re-enabling recomputes from now. A schedule switched back on after a week
    /// off must not fire the instant it is enabled.
    #[test]
    fn re_enabling_arms_from_now_rather_than_firing_immediately() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store.set_enabled(&made.id, false, at("2026-07-21 08:30:00")).unwrap();

        let back = at("2026-07-28 14:00:00");
        store.set_enabled(&made.id, true, back).unwrap();

        assert!(store.due(back).unwrap().is_empty(), "not instantly due on re-enable");
        let listed = store.list().unwrap();
        assert_eq!(listed[0].next_fire_at, at("2026-07-29 09:00:00"));
    }

    #[test]
    fn deleting_a_schedule_takes_its_history_with_it() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store.begin_fire(&made.id, made.next_fire_at).unwrap();
        store
            .finish_scheduled(
                &made,
                made.next_fire_at,
                RunOutcome::Ok,
                Some("s"),
                None,
                at("2026-07-21 09:00:01"),
            )
            .unwrap();

        store.delete(&made.id).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert!(store.runs(&made.id, 10).unwrap().is_empty(), "history went with it");
    }

    #[test]
    fn any_enabled_tracks_the_keep_awake_condition() {
        let store = store();
        assert!(!store.any_enabled().unwrap(), "nothing to stay awake for");

        let made = store
            .create(new_schedule(Recurrence::every_minutes(30).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        assert!(store.any_enabled().unwrap());

        store.set_enabled(&made.id, false, at("2026-07-21 08:30:00")).unwrap();
        assert!(!store.any_enabled().unwrap(), "a disabled-only set holds nothing");
    }

    #[test]
    fn every_recurrence_shape_survives_storage() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        for r in [
            Recurrence::every_minutes(45).unwrap(),
            Recurrence::daily_at(6, 30).unwrap(),
            Recurrence::weekly_at(4, 17, 15).unwrap(),
        ] {
            let made = store.create(new_schedule(r), now).expect("create");
            let back = store.list().unwrap().into_iter().find(|s| s.id == made.id).expect("found");
            assert_eq!(back.recurrence, r, "recurrence survived the round trip");
            store.delete(&made.id).unwrap();
        }
    }

    /// Ids must not collide even when two schedules are created back to back on
    /// a coarse clock — a collision would make one overwrite the other's row.
    #[test]
    fn ids_are_distinct() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        let ids: std::collections::HashSet<String> = (0..50)
            .map(|_| {
                store
                    .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), now)
                    .expect("create")
                    .id
            })
            .collect();
        assert_eq!(ids.len(), 50, "every id is distinct");
    }

    /// A heartbeat is stored with its target, and the fire path reads it back —
    /// the column is what makes the `ExistingSession` arm reachable at all.
    #[test]
    fn a_heartbeat_stores_and_reads_back_its_target() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        let made = store
            .create_heartbeat(
                new_schedule(Recurrence::every_minutes(15).unwrap()),
                "sess-1",
                now,
            )
            .expect("create");
        assert_eq!(made.target, ScheduleTarget::ExistingSession("sess-1".into()));

        let back = store.get(&made.id).unwrap().expect("found");
        assert_eq!(
            back.target,
            ScheduleTarget::ExistingSession("sess-1".into()),
            "the target survived the round trip"
        );
    }

    /// Heartbeats and spawning schedules share a table but not a list. A client
    /// asking for "schedules" must not be handed rows whose target it has no
    /// field to see — and `for_session` must not hand back another session's.
    #[test]
    fn the_two_listings_do_not_bleed_into_each_other() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        let spawning =
            store.create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), now).expect("create");
        let mine = store
            .create_heartbeat(new_schedule(Recurrence::every_minutes(15).unwrap()), "sess-1", now)
            .expect("create");
        store
            .create_heartbeat(new_schedule(Recurrence::every_minutes(20).unwrap()), "sess-2", now)
            .expect("create");

        let spawning_ids: Vec<String> =
            store.list_spawning().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(spawning_ids, vec![spawning.id], "heartbeats stay out of the schedule list");

        let mine_ids: Vec<String> =
            store.for_session("sess-1").unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(mine_ids, vec![mine.id], "one session sees only its own");
        assert!(store.for_session("sess-none").unwrap().is_empty());
    }

    /// The runaway guard: an agent that arms a heartbeat on every wake-up hits
    /// a wall instead of compounding.
    #[test]
    fn a_session_cannot_arm_more_than_the_cap() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        for i in 0..MAX_HEARTBEATS_PER_SESSION {
            store
                .create_heartbeat(
                    new_schedule(Recurrence::every_minutes(15).unwrap()),
                    "sess-1",
                    now,
                )
                .unwrap_or_else(|e| panic!("heartbeat {i} should fit: {e}"));
        }
        let err = store
            .create_heartbeat(new_schedule(Recurrence::every_minutes(15).unwrap()), "sess-1", now)
            .expect_err("over the cap");
        assert!(err.to_string().contains("limit"), "names the limit: {err}");
        // Another session is unaffected — the cap is per session, not global.
        store
            .create_heartbeat(new_schedule(Recurrence::every_minutes(15).unwrap()), "sess-2", now)
            .expect("a different session has its own headroom");
    }

    /// A paused heartbeat still occupies a slot: letting a pause buy headroom
    /// would make the ceiling meaningless.
    #[test]
    fn a_paused_heartbeat_still_counts_against_the_cap() {
        let store = store();
        let now = at("2026-07-21 08:00:00");
        let mut ids = Vec::new();
        for _ in 0..MAX_HEARTBEATS_PER_SESSION {
            ids.push(
                store
                    .create_heartbeat(
                        new_schedule(Recurrence::every_minutes(15).unwrap()),
                        "sess-1",
                        now,
                    )
                    .expect("create")
                    .id,
            );
        }
        store.set_enabled(&ids[0], false, now).expect("pause");
        assert!(
            store
                .create_heartbeat(
                    new_schedule(Recurrence::every_minutes(15).unwrap()),
                    "sess-1",
                    now
                )
                .is_err(),
            "pausing one does not free a slot"
        );
    }
}
