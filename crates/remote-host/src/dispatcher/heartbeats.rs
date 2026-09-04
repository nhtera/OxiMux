//! The heartbeat RPC handlers — a session's own recurring wake-ups.
//!
//! These share the schedule store with [`super::schedules`] but not its gates.
//! An ordinary schedule is a deferred *spawn*, so it wants full scope; a
//! heartbeat is a deferred prompt into a session that already exists, so it
//! wants exactly the authority to prompt that session
//! ([`AuthStore::may_manage_heartbeats`](crate::auth::AuthStore::may_manage_heartbeats)).
//! That difference is the whole reason a confined agent can arm its own wake-up
//! without being handed the keys to the host.
//!
//! Which session a call acts on is resolved **before** the gate, and never from
//! a bare client claim: a caller that IS a session may omit the id and have it
//! read from its proven scope; anyone else must name one, which is then
//! scope-checked like any other session id.

use oximux_agents::schedule::{NewSchedule, Recurrence, Schedule, ScheduleTarget, describe};
use oximux_remote_proto::messages::{CreateHeartbeatReq, HeartbeatWire, RecurrenceWire};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    /// Arm a wake-up inside an existing session.
    pub(super) fn create_heartbeat(&self, peer: &Peer, req: CreateHeartbeatReq) -> Response {
        let Some(session_id) = target_session(peer, req.session_id.as_deref()) else {
            return Response::Error(RpcError::BadRequest(
                "name a session: only a caller that IS one may omit it".into(),
            ));
        };
        if !self.auth.may_manage_heartbeats(peer, &session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        // The session must exist *here*, or a heartbeat would sit armed against
        // an id that will never resolve, failing once per cadence forever.
        if self.registry.get(&session_id).is_none()
            && self.catalog.as_ref().is_none_or(|c| c.transcript(&session_id).is_none())
        {
            return Response::Error(RpcError::UnknownSession);
        }
        let recurrence = match from_wire(req.recurrence) {
            Ok(r) => r,
            Err(e) => return Response::Error(RpcError::BadRequest(e.to_string())),
        };
        // `cwd` is carried for symmetry with a spawning schedule but never
        // used by the fire path — the target session already has one.
        let new = NewSchedule {
            name: req.name,
            cwd: String::new(),
            prompt: req.prompt,
            agent_id: None,
            recurrence,
        };
        match store.create_heartbeat(new, &session_id, self.now_local()) {
            Ok(schedule) => Response::HeartbeatCreated(to_wire(&schedule, &session_id)),
            // The per-session cap is a caller error, not a host fault: the text
            // names the limit and nothing else, so it is safe to forward.
            Err(e) => Response::Error(RpcError::BadRequest(e.to_string())),
        }
    }

    /// The wake-ups armed on a session.
    pub(super) fn list_heartbeats(&self, peer: &Peer, session_id: Option<&str>) -> Response {
        let Some(session_id) = target_session(peer, session_id) else {
            return Response::Error(RpcError::BadRequest(
                "name a session: only a caller that IS one may omit it".into(),
            ));
        };
        if !self.auth.may_read_heartbeats(peer, &session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match store.for_session(&session_id) {
            Ok(rows) => {
                Response::Heartbeats(rows.iter().map(|s| to_wire(s, &session_id)).collect())
            }
            Err(e) => {
                tracing::warn!(error = %e, "listing heartbeats failed");
                Response::Error(RpcError::Internal("could not read heartbeats".into()))
            }
        }
    }

    /// Disarm one wake-up.
    ///
    /// Authorized against the session the heartbeat **actually targets**, read
    /// from the store — never one the caller names. A confined agent handed
    /// another session's heartbeat id is therefore refused rather than allowed
    /// to delete work it cannot see.
    pub(super) fn delete_heartbeat(&self, peer: &Peer, id: &str) -> Response {
        // The coarse tier check comes FIRST, before the store is even consulted.
        // The per-target check below cannot stand alone: an id matching no row
        // names no session to scope against, so reaching the idempotent
        // "already gone" answer without this would let a read-only device get
        // `Ack` out of a write verb simply by naming an id that never existed.
        if !self.auth.may_attempt_heartbeat_writes(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.schedules.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        let existing = match store.get(id) {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(error = %e, "reading a heartbeat failed");
                return Response::Error(RpcError::Internal("could not read the heartbeat".into()));
            }
        };
        let Some(schedule) = existing else {
            // Idempotent: one already gone is success. Safe to answer without a
            // per-target check because the tier gate above already ran, and
            // returning the same `Ack` whether or not the id existed is what
            // keeps an id from being probed for existence.
            return Response::Ack;
        };
        let ScheduleTarget::ExistingSession(target) = &schedule.target else {
            // A spawning schedule is not a heartbeat; deleting it belongs to the
            // full-scope schedule verb, and answering here would route around it.
            return Response::Error(RpcError::BadRequest(
                "that id is a schedule, not a heartbeat — use `schedule rm`".into(),
            ));
        };
        if !self.auth.may_manage_heartbeats(peer, target) {
            return Response::Error(RpcError::Unauthorized);
        }
        match store.delete(id) {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, "deleting a heartbeat failed");
                Response::Error(RpcError::Internal("could not delete the heartbeat".into()))
            }
        }
    }
}

/// Which session a heartbeat call acts on: the one named, else the one the
/// caller IS.
///
/// `None` means the caller named nothing and is not a session — an operator or
/// a paired device, both of which must say what they mean.
fn target_session(peer: &Peer, named: Option<&str>) -> Option<String> {
    match named {
        Some(id) if !id.is_empty() => Some(id.to_string()),
        _ => peer.own_session().map(str::to_string),
    }
}

fn to_wire(s: &Schedule, session_id: &str) -> HeartbeatWire {
    HeartbeatWire {
        id: s.id.clone(),
        session_id: session_id.to_string(),
        name: s.name.clone(),
        prompt: s.prompt.clone(),
        recurrence: super::schedules::recurrence_to_wire(&s.recurrence, s.next_fire_at),
        enabled: s.enabled,
        next_fire_at: s.next_fire_at.to_rfc3339(),
        summary: describe(&s.recurrence),
    }
}


/// Wire → validated recurrence, through the constructors so the interval floor
/// and the time checks bite here rather than in the store.
fn from_wire(w: RecurrenceWire) -> Result<Recurrence, oximux_agents::schedule::RecurrenceError> {
    match w {
        RecurrenceWire::EveryMinutes { minutes } => Recurrence::every_minutes(minutes),
        RecurrenceWire::DailyAt { hour, minute } => Recurrence::daily_at(hour, minute),
        RecurrenceWire::WeeklyAt { weekday, hour, minute } => {
            Recurrence::weekly_at(weekday, hour, minute)
        }
    }
}
