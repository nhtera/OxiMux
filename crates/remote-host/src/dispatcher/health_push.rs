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

/// Follow `session_id`'s health: the persisted verdict first if nothing is
/// running it, then wait for it to be registered (a dormant session comes to
/// life on its next prompt), then for its driver's first publish, then forward
/// each readable change. Ends when the session's handle or the registry goes
/// away, after forwarding its last change: an exit is published and
/// unregistered on the same tick, and that frame is the one a reader most
/// needs.
///
/// **The persisted verdict is what makes a dormant session answerable.** Health
/// is published by whatever drives a session, so without it a client asking
/// after a session nothing is running waited on a publisher that would never
/// come and concluded nothing — about the very sessions a verdict matters most
/// for, the ones whose provider died and which nothing has touched since. The
/// catalog answers that from storage ([`SessionCatalog::verdict`]), and it is
/// read here rather than at the subscribe so the ordinary live path is
/// untouched: a session that *is* registered never reaches it.
///
/// Sent as the same frame a driver's publish sends, through the same
/// [`Dispatcher::forward_health`] and its per-frame authorization recheck. It
/// does not open the session — that is the whole point — so the live arm below
/// still waits, and the driver's own first publish is forwarded when it comes
/// even if it says the same thing, which is the confirmation a client wants.
///
/// **Read when there is nothing to attach to, not when the stream is built.**
/// Between building this and first polling it, a session can come to life —
/// another device's message opens it — and a storage read done at build time
/// would then send a verdict that was already out of date, ahead of the live
/// one, to a subscriber the old code would simply have attached. Asking at
/// attach time removes that: storage is consulted only in the branch where the
/// registry has nothing, which is also what keeps an ordinary live subscribe
/// off the disk.
///
/// What remains is the property a late subscriber has always had here: a
/// session that registers *and* unregisters inside the window leaves no handle
/// to follow, so the one frame that says a process died is missed and the
/// verdict stands until something registers again. That is why the frame is a
/// snapshot rather than a subscription — true when taken, with `liveness`
/// saying it was taken from storage — and why the reader that exists today
/// (`oximux status`) takes one frame and leaves rather than holding the
/// subscription open.
///
/// [`SessionCatalog::verdict`]: crate::catalog::SessionCatalog::verdict
pub(super) fn health_stream(
    registry: Arc<SessionRegistry>,
    catalog: Option<Arc<dyn crate::catalog::SessionCatalog>>,
    session_id: SessionId,
) -> BoxStream<'static, Live> {
    stream::once(async move {
        // The registry is authoritative for anything it holds: a persisted
        // verdict beside a live one is a second opinion on a session that
        // already has a publisher.
        if registry.get(&session_id).is_some() {
            return follow_live(registry, session_id);
        }
        let persisted = catalog
            .and_then(|c| c.verdict(&session_id))
            .map(|health| Live::Health { session_id: session_id.clone(), health });
        stream::iter(persisted).chain(follow_live(registry, session_id)).boxed()
    })
    .flatten()
    .boxed()
}

/// The live half: nothing until a driver publishes, then every readable change.
fn follow_live(registry: Arc<SessionRegistry>, session_id: SessionId) -> BoxStream<'static, Live> {
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
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A catalog that keeps one verdict and counts who asked for it.
    struct Persisted {
        verdict: Option<SessionHealth>,
        reads: AtomicUsize,
    }

    impl Persisted {
        fn holding(verdict: Option<SessionHealth>) -> Arc<Self> {
            Arc::new(Self { verdict, reads: AtomicUsize::new(0) })
        }
    }

    #[async_trait::async_trait]
    impl crate::catalog::SessionCatalog for Persisted {
        fn dormant(&self) -> Vec<crate::catalog::DormantSession> {
            Vec::new()
        }

        fn transcript(&self, _: &str) -> Option<crate::catalog::DormantTranscript> {
            None
        }

        fn choices(&self, _: &str) -> Option<crate::catalog::DormantChoices> {
            None
        }

        fn verdict(&self, _: &str) -> Option<SessionHealth> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.verdict.clone()
        }

        async fn open(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn failed(reason: &str) -> SessionHealth {
        SessionHealth {
            status: HealthStatus::Failed,
            liveness: Liveness::Unverifiable,
            limited_until: None,
            stalled_since: None,
            exited_unexpectedly: false,
            reason: Some(reason.into()),
            last_activity_at: 1_700_000_000_000,
            pending_unanswered_send: false,
            held_restored: 0,
            queued: 0,
            recovery_note_owed: false,
            turn_id: None,
            exit_seq: None,
        }
    }

    /// A session nothing is running answers from storage, straight away.
    ///
    /// Before this the stream had nothing to say until a driver published, so
    /// it said nothing at all about a session with no driver — and that is the
    /// session a verdict is most wanted for.
    #[tokio::test]
    async fn a_dormant_sessions_persisted_verdict_is_the_first_frame() {
        let registry = Arc::new(SessionRegistry::new());
        let catalog = Persisted::holding(Some(failed("agent process exited (code 137)")));

        let mut stream = health_stream(registry, Some(catalog), "cold-1".into());

        let first = stream.next().now_or_never().flatten().expect("a frame without waiting");
        let Live::Health { session_id, health } = first else {
            panic!("the first frame is the persisted verdict, not another kind of push")
        };
        assert_eq!(session_id, "cold-1");
        assert_eq!(health.status, HealthStatus::Failed);
        assert_eq!(health.reason.as_deref(), Some("agent process exited (code 137)"));
    }

    /// A host that keeps no verdict sends none. Answering `Idle` on its behalf
    /// would be the invention this whole path exists to avoid: the client says
    /// what it saw, which is nothing.
    #[tokio::test]
    async fn a_host_with_no_persisted_verdict_sends_no_frame() {
        let registry = Arc::new(SessionRegistry::new());
        let catalog = Persisted::holding(None);

        let mut stream = health_stream(registry, Some(catalog), "cold-1".into());

        assert!(
            stream.next().now_or_never().is_none(),
            "nothing ready: the stream is back to waiting for a driver",
        );
    }

    /// And a live session is never answered from storage. The registry is
    /// authoritative for anything it holds — a persisted verdict beside a
    /// running publisher is a second opinion — and the guard also keeps the
    /// ordinary subscribe off the disk entirely.
    ///
    /// Polled before the count is read, because the read is lazy: asserting on
    /// the count first would pass whatever the live branch did.
    #[tokio::test]
    async fn a_live_session_is_never_answered_from_storage() {
        let registry = Arc::new(SessionRegistry::new());
        let _handle = registry.register(
            "s-1".into(),
            Arc::new(oximux_agents::thread::StubConnection::default()),
        );
        let catalog = Persisted::holding(Some(failed("stale")));

        let mut stream = health_stream(registry, Some(catalog.clone()), "s-1".into());

        assert!(
            stream.next().now_or_never().is_none(),
            "no frame goes out until its driver publishes one",
        );
        assert_eq!(catalog.reads.load(Ordering::SeqCst), 0, "and the disk was not read");
    }

    /// Storage is read once for the subscribe, however often the stream is
    /// polled afterwards.
    ///
    /// A review read it as a per-poll cost. `stream::once` polls its future to
    /// completion once and the read lives inside it, so the count settles at
    /// one — which is what makes a blob load acceptable here at all.
    #[tokio::test]
    async fn the_persisted_verdict_is_read_once_however_often_the_stream_is_polled() {
        let registry = Arc::new(SessionRegistry::new());
        let catalog = Persisted::holding(Some(failed("agent process exited (code 137)")));

        let mut stream = health_stream(registry, Some(catalog.clone()), "cold-1".into());

        assert!(stream.next().now_or_never().flatten().is_some(), "the verdict");
        for _ in 0..5 {
            assert!(stream.next().now_or_never().is_none(), "then it waits for a driver");
        }
        assert_eq!(catalog.reads.load(Ordering::SeqCst), 1, "one read, not one per poll");
    }

    /// A session that comes to life between the subscribe and the first look is
    /// attached to, not answered from storage.
    ///
    /// The window is real: another device's message can open a session in the
    /// moment between this stream being built and the loop first polling it.
    /// Reading storage at build time sent such a subscriber a verdict that was
    /// already out of date, ahead of the live one — and to a subscriber the
    /// code before this path would simply have attached.
    #[tokio::test]
    async fn a_session_that_comes_to_life_before_the_first_look_is_not_answered_from_storage() {
        let registry = Arc::new(SessionRegistry::new());
        let catalog = Persisted::holding(Some(failed("what the last host left")));

        // Built while nothing is running it...
        let mut stream = health_stream(registry.clone(), Some(catalog.clone()), "s-1".into());
        // ...and opened before anything polls it.
        let _handle = registry.register(
            "s-1".into(),
            Arc::new(oximux_agents::thread::StubConnection::default()),
        );

        assert!(
            stream.next().now_or_never().is_none(),
            "the live session is followed, and nothing stale goes out ahead of its publish",
        );
        assert_eq!(catalog.reads.load(Ordering::SeqCst), 0, "storage was never asked");
    }

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
