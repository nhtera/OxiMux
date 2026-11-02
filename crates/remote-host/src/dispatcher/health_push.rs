//! The session health push (v37): a subscriber at or above
//! [`HEALTH_PUSH_MIN_VERSION`](oximux_remote_proto::proto::HEALTH_PUSH_MIN_VERSION)
//! gets the session's health once its driver has published it, then again on
//! every change a person could read. Activity alone (the timestamp moving) is
//! not a change: the events already say that.

use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use oximux_agents::health::{ExitProof, HealthStatus, Liveness, SessionHealth};
use oximux_agents::session_registry::{SessionId, SessionRegistry};
use oximux_remote_proto::Transport;
use oximux_remote_proto::delivery::{SessionHealthFrame, SessionHealthWire};
use oximux_remote_proto::proto::Response;

use super::Dispatcher;
use super::stream::Live;
use crate::auth::Peer;

/// Follow `session_id`'s health: wait for it to be registered (a dormant
/// session comes to life on its next prompt), then for its driver's first
/// publish, then forward each readable change. Ends when the session's
/// handle or the registry goes away.
pub(super) fn health_stream(registry: Arc<SessionRegistry>, session_id: SessionId) -> BoxStream<'static, Live> {
    stream::once(async move {
        let mut changes = registry.subscribe_changes();
        let handle = loop {
            if let Some(handle) = registry.get(&session_id) {
                break handle;
            }
            if changes.changed().await.is_err() {
                return stream::empty().boxed();
            }
        };
        let rx = handle.subscribe_health();
        drop(handle);
        let follow = Follow { rx, registry, session_id, last: None, first: true };
        stream::unfold(follow, |mut f| async move {
            loop {
                // The current value is the first one looked at; after that,
                // only a change wakes this. A closed watch is a gone session.
                if !std::mem::take(&mut f.first) && f.rx.changed().await.is_err() {
                    return None;
                }
                let now = f.rx.borrow_and_update().clone();
                let published = f.registry.get(&f.session_id).is_some_and(|h| h.health_published());
                if !published || f.last.as_ref().is_some_and(|l| same_to_a_reader(l, &now)) {
                    continue;
                }
                f.last = Some(now.clone());
                let live = Live::Health { session_id: f.session_id.clone(), health: now };
                return Some((live, f));
            }
        })
        .boxed()
    })
    .flatten()
    .boxed()
}

struct Follow {
    rx: tokio::sync::watch::Receiver<SessionHealth>,
    registry: Arc<SessionRegistry>,
    session_id: SessionId,
    /// The last health forwarded.
    last: Option<SessionHealth>,
    first: bool,
}

/// Equal but for when the session last did something.
fn same_to_a_reader(a: &SessionHealth, b: &SessionHealth) -> bool {
    SessionHealth { last_activity_at: 0, ..a.clone() } == SessionHealth { last_activity_at: 0, ..b.clone() }
}

impl Dispatcher {
    /// Push one health frame, rechecking that the peer may still read the
    /// session. Returns whether the transport is still writable.
    pub(super) async fn forward_health(
        &self,
        peer: &Peer,
        transport: &dyn Transport,
        session_id: SessionId,
        health: SessionHealth,
    ) -> bool {
        if !self.may_read_session(peer, &session_id) {
            return true;
        }
        let Some(core) = self.host_core.as_ref() else { return true };
        let frame = SessionHealthFrame::new(session_id, core.instance(), &wire(&health));
        self.send(transport, Response::SessionHealth(frame)).await
    }
}

pub(super) fn wire(h: &SessionHealth) -> SessionHealthWire {
    let status = match h.status {
        HealthStatus::Idle => "idle",
        HealthStatus::Working => "working",
        HealthStatus::NeedsPermission => "needsPermission",
        HealthStatus::NeedsAnswer => "needsAnswer",
        HealthStatus::Limited => "limited",
        HealthStatus::Failed => "failed",
        HealthStatus::Interrupted => "interrupted",
    };
    let (exit_proof, exit_code) = match h.liveness {
        Liveness::Exited(ExitProof::ExitCode(code)) => (Some("exitCode"), Some(code)),
        Liveness::Exited(ExitProof::UserStop) => (Some("userStop"), None),
        Liveness::Exited(ExitProof::HostLock) => (Some("hostLock"), None),
        Liveness::Exited(ExitProof::ConnectionClosed) => (Some("connectionClosed"), None),
        _ => (None, None),
    };
    SessionHealthWire {
        status: status.into(),
        liveness: h.liveness.wire_name().into(),
        exit_proof: exit_proof.map(str::to_string),
        exit_code,
        limited_until: h.limited_until,
        stalled_since: h.stalled_since,
        exited_unexpectedly: h.exited_unexpectedly,
        reason: h.reason.clone(),
        last_activity_at: h.last_activity_at,
        pending_unanswered_send: h.pending_unanswered_send,
        held_restored: h.held_restored,
        turn_id: h.turn_id.clone(),
        exit_seq: h.exit_seq.clone(),
    }
}
