//! The session health push (v37): a subscriber at or above
//! [`HEALTH_PUSH_MIN_VERSION`](oximux_remote_proto::proto::HEALTH_PUSH_MIN_VERSION)
//! gets the session's health once its driver has published it, then again on
//! every change a person could read. Activity alone (the timestamp moving) is
//! not a change: the events already say that.

use std::sync::{Arc, Weak};

use futures::stream::{self, BoxStream, StreamExt};
use oximux_agents::health::{ExitProof, HealthStatus, Liveness, SessionHealth};
use oximux_agents::session_registry::{SessionHandle, SessionId, SessionRegistry};
use oximux_remote_proto::Transport;
use oximux_remote_proto::delivery::{SessionHealthFrame, SessionHealthWire};
use oximux_remote_proto::proto::Response;

use super::Dispatcher;
use super::stream::Live;
use crate::auth::Peer;

/// Follow `session_id`'s health: wait for it to be registered (a dormant
/// session comes to life on its next prompt), then for its driver's first
/// publish, then forward each readable change. Ends when the session's
/// handle or the registry goes away, after forwarding its last change: an
/// exit is published and unregistered on the same tick, and that frame is
/// the one a reader most needs.
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
        // Weak: holding the handle would keep its watch open past its end.
        let handle = Arc::downgrade(&handle);
        let follow = Follow { rx, handle, session_id, last: None, first: true, published: false };
        stream::unfold(follow, |mut f| async move {
            loop {
                // The current value is the first one looked at; after that,
                // only a change wakes this. A closed watch is a gone session.
                if !std::mem::take(&mut f.first) && f.rx.changed().await.is_err() {
                    return None;
                }
                let now = f.rx.borrow_and_update().clone();
                // Published stays published. A handle already gone was
                // unregistered, so its driver had published: the change it
                // left (its exit) is observed.
                f.published = f.published || f.handle.upgrade().is_none_or(|h| h.health_published());
                if !f.published || f.last.as_ref().is_some_and(|l| same_to_a_reader(l, &now)) {
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
    handle: Weak<SessionHandle>,
    session_id: SessionId,
    /// The last health forwarded.
    last: Option<SessionHealth>,
    first: bool,
    /// The driver has published (seen once; it never unpublishes).
    published: bool,
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
        queued: h.queued,
        recovery_note_owed: h.recovery_note_owed,
        turn_id: h.turn_id.clone(),
        exit_seq: h.exit_seq.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the host knows about a session's health reaches the frame.
    ///
    /// Field by field, because this function is the only crossing: a value
    /// `compute` sets and `wire` forgets is invisible to every client while
    /// every test on either side of it passes. That is not hypothetical — the
    /// owed-note flag below was added with a test on the computation and a
    /// test on a hand-built frame, and neither would have noticed this
    /// function dropping it.
    #[test]
    fn every_fact_the_host_computes_crosses_to_the_frame() {
        let health = SessionHealth {
            status: HealthStatus::Limited,
            liveness: Liveness::Exited(ExitProof::ExitCode(137)),
            limited_until: Some(1_700_000_060_000),
            stalled_since: Some(1_700_000_000_000),
            exited_unexpectedly: true,
            reason: Some("rate limited".into()),
            last_activity_at: 1_700_000_000_111,
            pending_unanswered_send: true,
            held_restored: 3,
            queued: 5,
            recovery_note_owed: true,
            turn_id: Some("inc-1:2".into()),
            exit_seq: Some("exit-1".into()),
        };

        let w = wire(&health);

        assert_eq!(w.status, "limited");
        assert_eq!(w.liveness, "exited");
        assert_eq!(w.exit_proof.as_deref(), Some("exitCode"));
        assert_eq!(w.exit_code, Some(137));
        assert_eq!(w.limited_until, health.limited_until);
        assert_eq!(w.stalled_since, health.stalled_since);
        assert!(w.exited_unexpectedly);
        assert_eq!(w.reason, health.reason);
        assert_eq!(w.last_activity_at, health.last_activity_at);
        assert!(w.pending_unanswered_send);
        assert_eq!(w.held_restored, 3);
        assert_eq!(
            w.queued, 5,
            "and the two counts must not be crossed: a queue reported as a hold invites a \
             `release` that has nothing to lift",
        );
        assert!(w.recovery_note_owed, "an owed note has to cross, or no client can warn about it");
        assert_eq!(w.turn_id, health.turn_id);
        assert_eq!(w.exit_seq, health.exit_seq);
    }

    /// And the other exit proofs keep their own words.
    #[test]
    fn each_exit_proof_crosses_as_its_own_word() {
        for (proof, word) in [
            (ExitProof::UserStop, "userStop"),
            (ExitProof::HostLock, "hostLock"),
            (ExitProof::ConnectionClosed, "connectionClosed"),
        ] {
            let w = wire(&SessionHealth {
                liveness: Liveness::Exited(proof),
                ..SessionHealth {
                    status: HealthStatus::Failed,
                    liveness: Liveness::Live,
                    limited_until: None,
                    stalled_since: None,
                    exited_unexpectedly: false,
                    reason: None,
                    last_activity_at: 0,
                    pending_unanswered_send: false,
                    held_restored: 0,
                    queued: 0,
                    recovery_note_owed: false,
                    turn_id: None,
                    exit_seq: None,
                }
            });
            assert_eq!(w.exit_proof.as_deref(), Some(word));
            assert_eq!(w.exit_code, None, "{word} has no code of its own");
        }
    }
}
