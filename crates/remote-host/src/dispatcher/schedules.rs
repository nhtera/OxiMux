//! The schedule RPC handlers — create, list, delete, toggle, and read run
//! history for the desktop's scheduled agent runs.
//!
//! Unlike the session RPCs these name no session, so they scope on device *tier*
//! rather than `is_allowed_for`: reads want full scope
//! ([`AuthStore::may_read_schedules`](crate::auth::AuthStore::may_read_schedules)),
//! writes want full-and-not-read-only
//! ([`may_manage_schedules`](crate::auth::AuthStore::may_manage_schedules)) —
//! because a schedule is a deferred session spawn and a narrowed device could
//! otherwise plant one that runs outside its own confinement.
//!
//! The store itself is gpui-free and process-spawn-free (SQLite rows only), so
//! these handlers call it directly, without the view-layer seam `launcher` and
//! `rewinder` need. Wire ↔ store conversion lives here, keeping `remote-proto`
//! free of any `oximux-agents` dependency.

use chrono::{DateTime, Local, TimeZone};

use oximux_agents::schedule::{
    NewSchedule, Recurrence, RunOutcome, Schedule, ScheduleRun, describe,
};
use oximux_remote_proto::messages::{
    RecurrenceWire, RunOutcomeWire, ScheduleRunWire, ScheduleWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::AppPubkey;

impl Dispatcher {
    /// Every schedule the desktop holds. A full-scope read; empty is normal.
    pub(super) fn list_schedules(&self, pubkey: &AppPubkey) -> Response {
        if !self.auth.may_read_schedules(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            // Same answer a device lacking scope gets: whether this desktop keeps
            // schedules is not something an unauthorized client should probe.
            return Response::Error(RpcError::Unauthorized);
        };
        match store.list() {
            Ok(rows) => Response::Schedules(rows.iter().map(to_wire).collect()),
            Err(e) => {
                tracing::warn!(error = %e, "listing schedules failed");
                Response::Error(RpcError::Internal("could not read schedules".into()))
            }
        }
    }

    /// Create a schedule from a recurrence the phone chose.
    ///
    /// The recurrence is rebuilt through the desktop's own constructors, so an
    /// interval under the floor or an impossible time is refused rather than
    /// stored — the phone's pickers cannot express those, but a hand-crafted
    /// frame could, and the store must not be the last line of defence.
    pub(super) fn create_schedule(
        &self,
        pubkey: &AppPubkey,
        name: String,
        cwd: String,
        prompt: String,
        agent_id: Option<String>,
        recurrence: RecurrenceWire,
    ) -> Response {
        if !self.auth.may_manage_schedules(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        let recurrence = match from_wire(recurrence) {
            Ok(r) => r,
            // The constructor's message ("interval must be at least 5 minutes")
            // is safe to forward: it names no host path, only the rule broken.
            Err(e) => return Response::Error(RpcError::BadRequest(e.to_string())),
        };
        let new = NewSchedule { name, cwd, prompt, agent_id, recurrence };
        match store.create(new, self.now_local()) {
            Ok(schedule) => Response::ScheduleCreated(to_wire(&schedule)),
            Err(e) => {
                // A store error here is a disk/SQL failure, whose text can name
                // the database path — logged, not forwarded.
                tracing::warn!(error = %e, "creating schedule failed");
                Response::Error(RpcError::Internal("could not create the schedule".into()))
            }
        }
    }

    /// Delete a schedule. Idempotent — deleting one already gone is success.
    pub(super) fn delete_schedule(&self, pubkey: &AppPubkey, id: &str) -> Response {
        if !self.auth.may_manage_schedules(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        match store.delete(id) {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, "deleting schedule failed");
                Response::Error(RpcError::Internal("could not delete the schedule".into()))
            }
        }
    }

    /// Enable or disable a schedule without deleting it.
    pub(super) fn set_schedule_enabled(
        &self,
        pubkey: &AppPubkey,
        id: &str,
        enabled: bool,
    ) -> Response {
        if !self.auth.may_manage_schedules(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        // Re-enabling recomputes the next fire from now, so a schedule paused for a
        // week does not wake up owing a week of missed runs — the store owns that
        // arithmetic, which is why `now` is passed rather than assumed.
        match store.set_enabled(id, enabled, self.now_local()) {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, "toggling schedule failed");
                Response::Error(RpcError::Internal("could not update the schedule".into()))
            }
        }
    }

    /// A schedule's recent run history, most recent first. A full-scope read.
    pub(super) fn schedule_runs(
        &self,
        pubkey: &AppPubkey,
        schedule_id: &str,
        limit: u32,
    ) -> Response {
        if !self.auth.may_read_schedules(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        match store.runs(schedule_id, limit) {
            Ok(rows) => Response::ScheduleRuns(rows.iter().map(run_to_wire).collect()),
            Err(e) => {
                tracing::warn!(error = %e, "reading schedule runs failed");
                Response::Error(RpcError::Internal("could not read run history".into()))
            }
        }
    }

    /// The tick clock as a local datetime, derived from the injected `now_secs`
    /// so tests stay deterministic rather than reading the system clock here.
    fn now_local(&self) -> DateTime<Local> {
        Local
            .timestamp_opt((self.now_secs)() as i64, 0)
            .single()
            // A Unix second is never ambiguous in any zone; the fallback keeps the
            // signature total rather than unwrapping.
            .unwrap_or_else(Local::now)
    }
}

/// Store row → wire, carrying the desktop's own `describe` phrasing so the phone
/// renders the same words without re-deriving them.
fn to_wire(s: &Schedule) -> ScheduleWire {
    ScheduleWire {
        id: s.id.clone(),
        name: s.name.clone(),
        cwd: s.cwd.clone(),
        prompt: s.prompt.clone(),
        agent_id: s.agent_id.clone(),
        recurrence: recurrence_to_wire(&s.recurrence),
        enabled: s.enabled,
        next_fire_at: s.next_fire_at.to_rfc3339(),
        summary: describe(&s.recurrence),
    }
}

fn run_to_wire(r: &ScheduleRun) -> ScheduleRunWire {
    ScheduleRunWire {
        schedule_id: r.schedule_id.clone(),
        fired_at: r.fired_at.to_rfc3339(),
        outcome: match r.outcome {
            RunOutcome::Ok => RunOutcomeWire::Ok,
            RunOutcome::Failed => RunOutcomeWire::Failed,
        },
        session_id: r.session_id.clone(),
        detail: r.detail.clone(),
    }
}

fn recurrence_to_wire(r: &Recurrence) -> RecurrenceWire {
    match *r {
        Recurrence::EveryMinutes(minutes) => RecurrenceWire::EveryMinutes { minutes },
        Recurrence::DailyAt { hour, minute } => RecurrenceWire::DailyAt { hour, minute },
        Recurrence::WeeklyAt { weekday, hour, minute } => {
            RecurrenceWire::WeeklyAt { weekday, hour, minute }
        }
    }
}

/// Wire → validated store recurrence. Goes through the constructors, not the
/// enum literals, so the floor and the time/weekday checks bite here.
fn from_wire(w: RecurrenceWire) -> Result<Recurrence, oximux_agents::schedule::RecurrenceError> {
    match w {
        RecurrenceWire::EveryMinutes { minutes } => Recurrence::every_minutes(minutes),
        RecurrenceWire::DailyAt { hour, minute } => Recurrence::daily_at(hour, minute),
        RecurrenceWire::WeeklyAt { weekday, hour, minute } => {
            Recurrence::weekly_at(weekday, hour, minute)
        }
    }
}
