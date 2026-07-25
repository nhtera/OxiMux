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
pub struct ScheduleStore {
    conn: Arc<Mutex<Connection>>,
}

impl ScheduleStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create a schedule, computing its first fire from `now`.
    pub fn create(&self, new: NewSchedule, now: DateTime<Local>) -> Result<Schedule> {
        let schedule = Schedule {
            // Random rather than sequential: ids appear in run history and cross
            // the wire, and a guessable counter invites a client to address a
            // schedule it was never told about.
            id: format!("sch-{}", uuid_like()),
            name: new.name,
            cwd: new.cwd,
            prompt: new.prompt,
            agent_id: new.agent_id,
            recurrence: new.recurrence,
            enabled: true,
            next_fire_at: new.recurrence.next_after(now),
        };
        let (kind, interval, hour, minute, weekday) = decompose(&schedule.recurrence);
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO schedules (id, name, cwd, prompt, agent_id, kind, \
                 interval_minutes, hour, minute, weekday, enabled, next_fire_at, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12)",
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
                ],
            )
            .context("insert schedule")?;
        Ok(schedule)
    }

    /// Every schedule, newest fire first.
    pub fn list(&self) -> Result<Vec<Schedule>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, cwd, prompt, agent_id, kind, interval_minutes, hour, \
                 minute, weekday, enabled, next_fire_at FROM schedules ORDER BY next_fire_at",
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

    /// Record a fire and arm the next one.
    ///
    /// The next fire is computed from `now`, **not** from the scheduled instant
    /// that just elapsed. After the app was closed for a day, anchoring on the
    /// old time would make a daily schedule fire immediately and then again at
    /// its real time — and an interval schedule would burn through every missed
    /// slot in a burst. Catching up on missed runs is not what a user asking for
    /// "every morning" wants.
    ///
    /// Writing the run row and re-arming happen in **one transaction**: a crash
    /// between them would otherwise either lose the history or leave the
    /// schedule armed at a time already past, firing again on the next tick.
    pub fn mark_fired(
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
        let tx = conn.transaction().context("begin mark_fired")?;
        // `OR IGNORE`: the (schedule_id, fired_at) key makes a repeated attempt
        // at the same occurrence a no-op rather than an error, which is what
        // makes a restart near a fire boundary safe.
        tx.execute(
            "INSERT OR IGNORE INTO schedule_runs (schedule_id, fired_at, outcome, session_id, detail) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                schedule.id,
                fired_at.to_rfc3339(),
                match outcome {
                    RunOutcome::Ok => "ok",
                    RunOutcome::Failed => "failed",
                },
                session_id,
                detail,
            ],
        )
        .context("insert run")?;
        tx.execute(
            "UPDATE schedules SET next_fire_at = ?1 WHERE id = ?2",
            params![next.to_rfc3339(), schedule.id],
        )
        .context("re-arm schedule")?;
        tx.commit().context("commit mark_fired")?;
        Ok(next)
    }

    /// Whether this exact occurrence already ran — the guard a tick checks
    /// before firing, so a restart cannot double-run one slot.
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

    /// Recent runs for one schedule, newest first.
    pub fn runs(&self, schedule_id: &str, limit: u32) -> Result<Vec<ScheduleRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT schedule_id, fired_at, outcome, session_id, detail FROM schedule_runs \
                 WHERE schedule_id = ?1 ORDER BY fired_at DESC LIMIT ?2",
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
    })
}

fn parse_local(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Local))
}

/// A random id, without pulling in a uuid dependency for one call site.
fn uuid_like() -> String {
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

        let next = store
            .mark_fired(&due[0], due[0].next_fire_at, RunOutcome::Ok, Some("sess-1"), None, now)
            .expect("mark fired");
        assert_eq!(next, at("2026-07-23 09:00:00"), "armed forward from now, not from the old slot");
        assert!(store.due(now).unwrap().is_empty(), "no longer due");
    }

    /// The idempotency key. A restart near a fire boundary must not run one
    /// occurrence twice.
    #[test]
    fn the_same_occurrence_cannot_be_recorded_twice() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        let slot = made.next_fire_at;

        assert!(!store.already_ran(&made.id, slot).unwrap(), "not yet run");
        store
            .mark_fired(&made, slot, RunOutcome::Ok, Some("sess-1"), None, at("2026-07-21 09:00:01"))
            .unwrap();
        assert!(store.already_ran(&made.id, slot).unwrap(), "recorded");

        // A second attempt at the SAME slot is a no-op, not an error and not a
        // duplicate row.
        store
            .mark_fired(&made, slot, RunOutcome::Ok, Some("sess-2"), None, at("2026-07-21 09:00:02"))
            .expect("second attempt is tolerated");
        let runs = store.runs(&made.id, 10).unwrap();
        assert_eq!(runs.len(), 1, "one row for one occurrence, saw {runs:?}");
        assert_eq!(runs[0].session_id.as_deref(), Some("sess-1"), "the first run stands");
    }

    #[test]
    fn a_failed_run_is_recorded_with_its_reason() {
        let store = store();
        let made = store
            .create(new_schedule(Recurrence::daily_at(9, 0).unwrap()), at("2026-07-21 08:00:00"))
            .expect("create");
        store
            .mark_fired(
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
        store
            .mark_fired(
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
}
