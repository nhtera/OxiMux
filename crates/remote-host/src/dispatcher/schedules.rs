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

use chrono::{DateTime, Local, TimeZone, Timelike};

use oximux_agents::schedule::{NewSchedule, Recurrence, Schedule, ScheduleRun, describe};
use oximux_remote_proto::messages::{
    RecurrenceV2Wire, RecurrenceWire, ScheduleRunWire, ScheduleV2Wire, ScheduleWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    /// Every **spawning** schedule the desktop holds. A full-scope read; empty
    /// is normal.
    ///
    /// Heartbeats share this table but are deliberately excluded. `ScheduleWire`
    /// has no target field and cannot grow one (it rides inside a `Vec` in a
    /// reply older clients already ask for, where postcard would misparse every
    /// element after an appended field), so a heartbeat listed here would show
    /// as an ordinary schedule offering edits — change the cwd, change the agent
    /// — that mean nothing for a session already open. They have their own verb.
    pub(super) fn list_schedules(&self, peer: &Peer) -> Response {
        match self.readable_schedules(peer) {
            Ok(rows) => Response::Schedules(rows.iter().map(to_wire).collect()),
            Err(e) => Response::Error(e),
        }
    }

    /// [`Self::list_schedules`] for a v23 peer: the same rows, with cron
    /// expressions intact rather than the stand-in the v18 shape must use.
    pub(super) fn list_schedules_v2(&self, peer: &Peer) -> Response {
        match self.readable_schedules(peer) {
            Ok(rows) => Response::SchedulesV2(rows.iter().map(to_wire_v2).collect()),
            Err(e) => Response::Error(e),
        }
    }

    /// The scope check, the store lookup and the read, shared by both list
    /// verbs so the two shapes can never disagree about *which* schedules a
    /// peer may see — only about how they are rendered.
    fn readable_schedules(&self, peer: &Peer) -> Result<Vec<Schedule>, RpcError> {
        if !self.auth.may_read_schedules(peer) {
            return Err(RpcError::Unauthorized);
        }
        // Same answer a device lacking scope gets: whether this desktop keeps
        // schedules is not something an unauthorized client should probe.
        let store = self.schedules.as_ref().ok_or(RpcError::Unauthorized)?;
        store.list_spawning().map_err(|e| {
            tracing::warn!(error = %e, "listing schedules failed");
            RpcError::Internal("could not read schedules".into())
        })
    }

    /// Create a schedule from a recurrence the phone chose.
    ///
    /// The recurrence is rebuilt through the desktop's own constructors, so an
    /// interval under the floor or an impossible time is refused rather than
    /// stored — the phone's pickers cannot express those, but a hand-crafted
    /// frame could, and the store must not be the last line of defence.
    pub(super) fn create_schedule(
        &self,
        peer: &Peer,
        name: String,
        cwd: String,
        prompt: String,
        agent_id: Option<String>,
        recurrence: RecurrenceWire,
    ) -> Response {
        match self.store_schedule(peer, name, cwd, prompt, agent_id, from_wire(recurrence)) {
            Ok(schedule) => Response::ScheduleCreated(to_wire(&schedule)),
            Err(e) => Response::Error(e),
        }
    }

    /// [`Self::create_schedule`] for a v23 peer, whose recurrence may be cron.
    ///
    /// Same gate, same constructors, same store call — the only difference is
    /// which wire enum arrives and which reply shape leaves. Cron changes *when*
    /// a schedule fires, never what it is allowed to do, so it earns no
    /// different privilege.
    pub(super) fn create_schedule_v2(
        &self,
        peer: &Peer,
        name: String,
        cwd: String,
        prompt: String,
        agent_id: Option<String>,
        recurrence: RecurrenceV2Wire,
    ) -> Response {
        match self.store_schedule(peer, name, cwd, prompt, agent_id, from_wire_v2(recurrence)) {
            Ok(schedule) => Response::ScheduleCreatedV2(to_wire_v2(&schedule)),
            Err(e) => Response::Error(e),
        }
    }

    /// The write gate, the recurrence validation and the insert, shared by both
    /// create verbs.
    ///
    /// Takes the recurrence already converted, as a `Result`, so each verb owns
    /// only its own wire mapping — and so neither can skip the constructors
    /// that enforce the floor.
    fn store_schedule(
        &self,
        peer: &Peer,
        name: String,
        cwd: String,
        prompt: String,
        agent_id: Option<String>,
        recurrence: Result<Recurrence, oximux_agents::schedule::RecurrenceError>,
    ) -> Result<Schedule, RpcError> {
        if !self.auth.may_manage_schedules(peer) {
            return Err(RpcError::Unauthorized);
        }
        let store = self.schedules.as_ref().ok_or(RpcError::Unauthorized)?;
        // The constructor's message ("interval must be at least 5 minutes", or
        // croner's own "Pattern must have 5 fields") is safe to forward: it
        // names no host path, only the rule broken.
        let recurrence = recurrence.map_err(|e| RpcError::BadRequest(e.to_string()))?;
        let new = NewSchedule { name, cwd, prompt, agent_id, recurrence };
        store.create(new, self.now_local()).map_err(|e| {
            // A store error here is a disk/SQL failure, whose text can name the
            // database path — logged, not forwarded.
            tracing::warn!(error = %e, "creating schedule failed");
            RpcError::Internal("could not create the schedule".into())
        })
    }

    /// Delete a schedule. Idempotent — deleting one already gone is success.
    pub(super) fn delete_schedule(&self, peer: &Peer, id: &str) -> Response {
        if !self.auth.may_manage_schedules(peer) {
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
        peer: &Peer,
        id: &str,
        enabled: bool,
    ) -> Response {
        if !self.auth.may_manage_schedules(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        // Resolve first. `set_enabled` is an UPDATE with no row check, so an id
        // that matches nothing is a silent no-op it reports as success — and a
        // caller told "paused" about a schedule that does not exist has been
        // lied to. Deleting is idempotent on purpose (the goal state is reached
        // either way); pausing has no such reading, so it refuses by name,
        // exactly as `run_schedule_now` already does.
        match store.get(id) {
            Ok(Some(_)) => {}
            Ok(None) => return Response::Error(RpcError::BadRequest("no such schedule".into())),
            Err(e) => {
                tracing::warn!(error = %e, "resolving schedule before toggle failed");
                return Response::Error(RpcError::Internal("could not update the schedule".into()));
            }
        }
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

    /// Fire one schedule right now, without touching its cadence.
    ///
    /// Authorization first (the same write tier as creating a schedule — this
    /// spawns a session), capability second: only the process holding the
    /// ticker lock installs a runner, so on a desktop+serve box the loser
    /// answers `Unsupported` to an *authorized* caller rather than racing the
    /// owner. Refusals are `Error`s; a fire that ran and failed is a normal
    /// [`Response::ScheduleRunRecorded`] whose run says what went wrong.
    pub(super) async fn run_schedule_now(&self, peer: &Peer, schedule_id: &str) -> Response {
        use crate::schedule_runner::RunNowError;

        if !self.auth.may_manage_schedules(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(runner) = self.schedule_runner.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match runner.run_now(schedule_id).await {
            Ok(run) => Response::ScheduleRunRecorded(run),
            Err(RunNowError::NoSuchSchedule) => {
                Response::Error(RpcError::BadRequest("no such schedule".into()))
            }
            Err(RunNowError::AlreadyFiring) => {
                Response::Error(RpcError::BadRequest("that schedule is already firing".into()))
            }
            Err(RunNowError::Unavailable) => Response::Error(RpcError::Internal(
                "the host cannot start a session right now".into(),
            )),
            Err(RunNowError::Failed) => {
                Response::Error(RpcError::Internal("could not record the run".into()))
            }
        }
    }

    /// A schedule's recent run history, most recent first. A full-scope read.
    pub(super) fn schedule_runs(
        &self,
        peer: &Peer,
        schedule_id: &str,
        limit: u32,
    ) -> Response {
        if !self.auth.may_read_schedules(peer) {
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
    /// Shared with the heartbeat and team handlers, which need the same clock.
    pub(super) fn now_local(&self) -> DateTime<Local> {
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
        recurrence: recurrence_to_wire(&s.recurrence, s.next_fire_at),
        enabled: s.enabled,
        next_fire_at: s.next_fire_at.to_rfc3339(),
        summary: describe(&s.recurrence),
    }
}

fn run_to_wire(r: &ScheduleRun) -> ScheduleRunWire {
    crate::schedule_runner::schedule_run_to_wire(r)
}

fn to_wire_v2(s: &Schedule) -> ScheduleV2Wire {
    ScheduleV2Wire {
        id: s.id.clone(),
        name: s.name.clone(),
        cwd: s.cwd.clone(),
        prompt: s.prompt.clone(),
        agent_id: s.agent_id.clone(),
        recurrence: recurrence_to_wire_v2(&s.recurrence),
        enabled: s.enabled,
        next_fire_at: s.next_fire_at.to_rfc3339(),
        summary: describe(&s.recurrence),
    }
}

/// Store → the v18 wire enum, which has no cron variant and cannot gain one.
///
/// **This is the single place a cron schedule is degraded**, and both the
/// schedule and heartbeat surfaces route through it so the two can never grow
/// different stand-ins. A cron schedule becomes a `DailyAt` at the wall-clock
/// time of its next fire: the closest thing the v18 vocabulary can say, and
/// close enough to read sensibly if a peer ever surfaced it.
///
/// It is safe *because* nothing acts on it. `ScheduleWire.summary` and
/// `.next_fire_at` travel beside it and stay exact, and every consumer of a
/// listed schedule reads those instead of this field.
///
/// The invariant that keeps it safe is **"no UI prefills a recurrence from a
/// listed row"** — deliberately stated that way rather than as "no edit verb
/// exists". No edit verb does exist, but `DeleteSchedule` + `CreateSchedule`
/// would compose into one without any wire change at all, and such an edit
/// would write this stand-in back over a real cron rule. See the note on
/// [`ScheduleWire::recurrence`] before building one.
/// `next_fire_at` is the schedule's **stored** next fire — the same value that
/// ships in the frame beside this field — and not a freshly computed one. Those
/// two differ for any pattern with more than one fire a day: `0 9,17 * * *`
/// listed at 10:00 would recompute to 17:00 while the frame's `next_fire_at`
/// still reads 09:00, giving one frame two answers about one schedule. Taking
/// it as an argument also keeps this function off the system clock, which the
/// injected [`Dispatcher::now_local`] exists to avoid.
pub(super) fn recurrence_to_wire(
    r: &Recurrence,
    next_fire_at: DateTime<Local>,
) -> RecurrenceWire {
    match *r {
        Recurrence::EveryMinutes(minutes) => RecurrenceWire::EveryMinutes { minutes },
        Recurrence::DailyAt { hour, minute } => RecurrenceWire::DailyAt { hour, minute },
        Recurrence::WeeklyAt { weekday, hour, minute } => {
            RecurrenceWire::WeeklyAt { weekday, hour, minute }
        }
        Recurrence::Cron(_) => RecurrenceWire::DailyAt {
            hour: next_fire_at.hour() as u8,
            minute: next_fire_at.minute() as u8,
        },
    }
}

fn recurrence_to_wire_v2(r: &Recurrence) -> RecurrenceV2Wire {
    match *r {
        Recurrence::EveryMinutes(minutes) => RecurrenceV2Wire::EveryMinutes { minutes },
        Recurrence::DailyAt { hour, minute } => RecurrenceV2Wire::DailyAt { hour, minute },
        Recurrence::WeeklyAt { weekday, hour, minute } => {
            RecurrenceV2Wire::WeeklyAt { weekday, hour, minute }
        }
        Recurrence::Cron(ref expr) => RecurrenceV2Wire::Cron { expr: expr.clone() },
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

/// The v23 wire → a validated store recurrence.
///
/// `Recurrence::cron` is where a bad expression is refused, so this is the only
/// path a cron pattern may reach the store by — the same reason [`from_wire`]
/// goes through constructors rather than enum literals.
fn from_wire_v2(
    w: RecurrenceV2Wire,
) -> Result<Recurrence, oximux_agents::schedule::RecurrenceError> {
    match w {
        RecurrenceV2Wire::EveryMinutes { minutes } => Recurrence::every_minutes(minutes),
        RecurrenceV2Wire::DailyAt { hour, minute } => Recurrence::daily_at(hour, minute),
        RecurrenceV2Wire::WeeklyAt { weekday, hour, minute } => {
            Recurrence::weekly_at(weekday, hour, minute)
        }
        RecurrenceV2Wire::Cron { expr } => Recurrence::cron(&expr),
    }
}
