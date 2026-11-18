//! `oximux status` — is a host there, and what is it running?

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

pub async fn run(client: &Client) -> Result<(Value, String), Failure> {
    let sessions = match client.call(Request::ListSessions).await? {
        Response::Sessions(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListSessions", &other)),
    };
    let awaiting = sessions.iter().filter(|s| s.awaiting_permission).count();
    let data = json!({
        "reachable": true,
        "host": {
            "protocol_version": client.host_version,
            "min_compatible": client.host_min_compatible,
        },
        "sessions": { "total": sessions.len(), "awaiting_permission": awaiting },
    });
    let human = format!(
        "host reachable (protocol v{})\nsessions: {} total, {} awaiting permission",
        client.host_version,
        sessions.len(),
        awaiting
    );
    Ok((data, human))
}

/// How long to wait for the first health frame before saying nothing has
/// published one.
///
/// Not a measure of work: a live session's pump publishes before its open is
/// even acknowledged, so the frame is normally already queued behind the
/// subscribe reply and this only covers being scheduled. It exists because the
/// absence of a frame is itself the answer for a session this host is not
/// running, and that answer has to arrive promptly — on a server most sessions
/// are dormant most of the time.
const FIRST_HEALTH_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

/// `oximux status <session>` — the host's own verdict on one session.
///
/// Read from the health the host publishes (the v37 push), not derived from
/// the event stream. The difference is the point: a session can be silent
/// because its turn is thinking, because a limit closed it and a retry is
/// armed, because its process died, or because nobody can tell — and the
/// event stream looks identical in all four. The host already distinguishes
/// them for the phone; this is the same answer, for whoever is on the box.
///
/// **It never starts anything.** A diagnostic that spawned the provider it was
/// asked about would answer a question nobody asked, and on a headless host
/// the spawn is a real process with a real bill. So the whole read is one
/// `Subscribe` — which waits for a dormant session rather than building it —
/// and deliberately not `GetSessionInfo`, which is in serve's build list and
/// resurrected the very session being examined. That showed up as a session
/// reading `restoredUnconfirmed` moments after its agent had exited: not the
/// host's verdict at all, but the echo of this verb's own side effect.
pub async fn session(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    client.require_version(
        oximux_remote_proto::proto::HEALTH_PUSH_MIN_VERSION,
        "status <session>",
    )?;
    // One call, and it is the host that decides whether this session exists
    // and whether this caller may read it: `Subscribe` answers `Unauthorized`
    // or `UnknownSession` itself, and the host's rule for reading is wider
    // than the session list is — an agent's listing holds only its own
    // session, while a parent may read the children it spawned. Gating on the
    // listing refused a parent asking after its own child.
    //
    // The host sends the current health first, so this is a read. It arrives
    // as a push *after* the subscribe reply, so both arrival points are
    // handled: one during the call, one after it.
    //
    // The reply replays the retained ring, which this discards. That is the
    // price of not asking for a cursor first: the only request that reports one
    // is `GetSessionInfo`, and asking it would start the agent.
    let mut health = None;
    let reply = client
        .call_streaming(Request::Subscribe { session_id: session.into(), after_seq: None }, |push| {
            if let Response::SessionHealth(frame) = push {
                health.get_or_insert(frame);
            }
        })
        .await?;
    match reply {
        Response::Events(_) => {}
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("Subscribe", &other)),
    }
    if health.is_none() {
        // Bounded, because no frame is the answer for a session nothing is
        // running. Events keep flowing on a subscribed connection and are not
        // what this asked for.
        let first = async {
            loop {
                if let Response::SessionHealth(frame) = client.recv_frame().await? {
                    return Ok::<_, Failure>(frame);
                }
            }
        };
        health = tokio::time::timeout(FIRST_HEALTH_WITHIN, first).await.ok().transpose()?;
    }
    let Some(frame) = health else { return Ok(no_verdict(session)) };
    let wire = frame.health().map_err(|e| {
        Failure::new("decode", exit::ERROR, format!("the host's health frame did not parse: {e}"))
    })?;

    let data = json!({
        "session_id": session,
        // The host published a verdict, which is the only thing this reports.
        // Whether the session is "open" is deliberately not claimed: see
        // `no_verdict`.
        "health_published": true,
        // Which host process published this. A client that reconnects and sees
        // a different id knows the host restarted, so whatever it was tracking
        // per-session has to be re-read rather than resumed.
        "host_instance_id": frame.host_instance_id,
        "health": {
            "status": wire.status,
            "liveness": wire.liveness,
            "exit_proof": wire.exit_proof,
            "exit_code": wire.exit_code,
            "limited_until": wire.limited_until,
            "stalled_since": wire.stalled_since,
            "exited_unexpectedly": wire.exited_unexpectedly,
            "reason": wire.reason,
            "last_activity_at": wire.last_activity_at,
            "pending_unanswered_send": wire.pending_unanswered_send,
            "held_restored": wire.held_restored,
            "queued": wire.queued,
            "recovery_note_owed": wire.recovery_note_owed,
            "turn_id": wire.turn_id,
            "exit_seq": wire.exit_seq,
        },
    });
    let mut human = format!("{session}: {} ({})", wire.status, wire.liveness);
    if let Some(reason) = &wire.reason {
        human.push_str(&format!("\n{reason}"));
    }
    if wire.recovery_note_owed {
        human.push_str(
            "\nthe next message will carry a note telling the agent a restart cut its turn short",
        );
    }
    if wire.held_restored > 0 {
        // "Lifts the hold", not "sends them": with a prompt open or a turn
        // running, release makes these ordinary queued sends and they go
        // when the gate opens. Promising a send would be the same mistake
        // the verb's own help made about a Stop — telling a person something
        // will happen that may not.
        human.push_str(&format!(
            "\n{} message(s) held from before a restart — `oximux release {session}` lifts the hold",
            wire.held_restored
        ));
    }
    // Said separately, because the two answer different questions: the hold
    // asks the person to act, while a queue is the session getting on with
    // it.
    //
    // Not a post-release readout. Nothing publishes health on the release
    // itself — the counts belong to the pump, and a second writer would be a
    // second opinion — and a release into a busy gate wakes nobody, so these
    // numbers catch up on the session's next event or within one save tick.
    // A `status` run straight after a `release` may still show the counts
    // from before it; the release's own reply is what answers for that
    // release.
    if wire.queued > 0 {
        human.push_str(&format!(
            "\n{} message(s) waiting to go out",
            wire.queued
        ));
    }
    Ok((data, human))
}

/// The answer when the host published no verdict within the budget.
///
/// Reports the observation and nothing more. The ordinary cause is a session
/// this host is not running — most of them, on a server — but a pump that
/// registered its session and then wedged before publishing looks identical
/// from out here, and so does a push that simply took longer than the budget.
/// Calling any of those "stopped" would be the verb inventing the one thing it
/// was built to stop other surfaces inventing: a definite state with no
/// evidence behind it.
///
/// So the likely reading is offered to the person as a likelihood, and the
/// machine-readable half says only what was seen.
fn no_verdict(session: &str) -> (Value, String) {
    (
        json!({ "session_id": session, "health_published": false, "health": Value::Null }),
        format!(
            "{session}: no health published in {}s\n\
             a running session publishes before it is open, so this host is \
             most likely not running it — `oximux ls` lists what it knows, and \
             a message opens it",
            FIRST_HEALTH_WITHIN.as_secs(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::delivery::{SessionHealthFrame, SessionHealthWire};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A host answering the two calls this verb makes, with frames queued in
    /// the order a real host sends them.
    ///
    /// The ordering is the part worth modelling: the health frame is a *push*
    /// that arrives after the subscribe reply, so a verb that only looked at
    /// the reply would find nothing. One queue, popped in order, reproduces
    /// that without a timing assumption.
    struct ScriptedHost {
        /// Frames to hand back, oldest first. Shared so a late verdict can be
        /// queued from a task without cloning the host.
        outbox: Arc<Mutex<VecDeque<Vec<u8>>>>,
        /// What the subscribe is answered with.
        answer: Answer,
        health: SessionHealthWire,
    }

    enum Answer {
        /// A verdict follows the reply, after this long.
        Health(std::time::Duration),
        /// The reply, and then nothing — a session nothing is running.
        Silence,
        /// The host refuses the subscribe.
        Refused(oximux_remote_proto::proto::RpcError),
    }

    impl ScriptedHost {
        fn live_with(health: SessionHealthWire) -> Arc<Self> {
            Self::new(Answer::Health(std::time::Duration::ZERO), health)
        }

        fn silent() -> Arc<Self> {
            Self::new(Answer::Silence, SessionHealthWire::default())
        }

        fn refusing(err: oximux_remote_proto::proto::RpcError) -> Arc<Self> {
            Self::new(Answer::Refused(err), SessionHealthWire::default())
        }

        /// A host whose verdict arrives later than the budget allows.
        fn slow() -> Arc<Self> {
            Self::new(
                Answer::Health(FIRST_HEALTH_WITHIN + std::time::Duration::from_secs(1)),
                SessionHealthWire { status: "working".into(), liveness: "live".into(), ..SessionHealthWire::default() },
            )
        }

        fn new(answer: Answer, health: SessionHealthWire) -> Arc<Self> {
            Arc::new(Self { outbox: Arc::new(Mutex::new(VecDeque::new())), answer, health })
        }

        fn queue(&self, frames: Vec<Response>) {
            let mut outbox = self.outbox.lock().unwrap();
            for frame in frames {
                outbox.push_back(frame.to_bytes().expect("a response"));
            }
        }
    }

    #[async_trait::async_trait]
    impl oximux_remote_proto::Transport for ScriptedHost {
        async fn send(&self, frame: Vec<u8>) -> Result<(), oximux_remote_proto::TransportError> {
            // One request, and it is `Subscribe`. Every other arm panics —
            // `GetSessionInfo` above all, which builds a dormant session on
            // serve, so a read that sent it would start the agent it was asked
            // about. This is what keeps that from coming back.
            let Request::Subscribe { session_id, .. } = Request::from_bytes(&frame).expect("a request")
            else {
                panic!("status should send nothing but Subscribe");
            };
            match &self.answer {
                Answer::Refused(err) => self.queue(vec![Response::Error(err.clone())]),
                Answer::Silence => self.queue(vec![Response::Events(vec![])]),
                Answer::Health(after) => {
                    // The reply, then an event that is not what we asked for:
                    // a verb that stopped at the first push would read the
                    // event and never find the health.
                    self.queue(vec![
                        Response::Events(vec![]),
                        Response::Event(
                            oximux_remote_proto::messages::HostEvent::new(
                                &session_id,
                                8,
                                &oximux_agent_core::thread::ThreadEvent::AssistantText("hi".into()),
                                oximux_remote_proto::messages::SessionStatusWire {
                                    last_seq: 8,
                                    awaiting_permission: true,
                                },
                            )
                            .expect("a frame"),
                        ),
                    ]);
                    let frame = Response::SessionHealth(SessionHealthFrame::new(
                        &session_id,
                        "host-instance-7",
                        &self.health,
                    ));
                    if after.is_zero() {
                        self.queue(vec![frame]);
                    } else {
                        // Queued late, so the budget decides what the verb
                        // reports rather than the test's ordering.
                        let outbox = self.outbox.clone();
                        let after = *after;
                        tokio::spawn(async move {
                            tokio::time::sleep(after).await;
                            outbox.lock().unwrap().push_back(frame.to_bytes().expect("a response"));
                        });
                    }
                }
            }
            Ok(())
        }

        async fn recv(&self) -> Result<Option<Vec<u8>>, oximux_remote_proto::TransportError> {
            // A connection with nothing to say is quiet, not closed. Returning
            // `None` here reported "the host closed the connection", which is
            // a different thing from a session that has published no health —
            // and the verb must tell those apart.
            loop {
                if let Some(frame) = self.outbox.lock().unwrap().pop_front() {
                    return Ok(Some(frame));
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }

    fn client_over(host: Arc<ScriptedHost>) -> Client {
        crate::client::client_for_test(host, oximux_remote_proto::proto::PROTOCOL_VERSION)
    }

    /// The host's verdict reaches the caller whole — every field, not a
    /// summary of them.
    ///
    /// A machine reader is the point: a supervisor deciding whether to wait,
    /// retry or page someone needs `liveness` and `limited_until`, and a
    /// human-shaped line would make it parse prose.
    #[tokio::test]
    async fn a_live_sessions_whole_health_is_reported() {
        let host = ScriptedHost::live_with(SessionHealthWire {
            status: "limited".into(),
            liveness: "live".into(),
            limited_until: Some(1_700_000_060_000),
            reason: Some("rate limited until 09:01".into()),
            held_restored: 2,
            queued: 1,
            turn_id: "inc-1:3".to_string().into(),
            ..SessionHealthWire::default()
        });
        let (data, human) = session(&client_over(host), "s-1").await.expect("status");

        assert_eq!(data["health_published"], true);
        assert_eq!(data["health"]["status"], "limited");
        assert_eq!(data["health"]["liveness"], "live");
        assert_eq!(data["health"]["limited_until"], 1_700_000_060_000i64);
        assert_eq!(data["health"]["held_restored"], 2);
        assert_eq!(
            data["health"]["queued"], 1,
            "the queue is reported apart from the hold, so a reader can follow a release",
        );
        assert_eq!(data["health"]["turn_id"], "inc-1:3");
        assert_eq!(
            data["host_instance_id"], "host-instance-7",
            "which host process said so, so a reconnect can tell a restart",
        );
        assert!(human.contains("limited"), "{human}");
        assert!(human.contains("rate limited until 09:01"), "the reason is for a person: {human}");
        assert!(human.contains("held from before a restart"), "a held queue is actionable: {human}");
        assert!(
            human.contains("oximux release s-1") && human.contains("lifts the hold"),
            "it names the verb that acts on it, and says what the verb actually does — the \
             lines may still wait on a turn or a prompt: {human}",
        );
        assert!(
            !human.contains("sends them"),
            "and must not promise a send that an open prompt would hold: {human}",
        );
        assert!(
            human.contains("1 message(s) waiting to go out"),
            "the queue is said apart from the hold, in a person's words too: {human}",
        );
    }

    /// An owed recovery note is told to whoever is about to send a message.
    ///
    /// Their next message will carry a paragraph they did not write — the
    /// host has always known, and until now said so to nobody.
    #[tokio::test]
    async fn an_owed_recovery_note_is_reported_before_the_next_message() {
        let host = ScriptedHost::live_with(SessionHealthWire {
            status: "idle".into(),
            liveness: "live".into(),
            recovery_note_owed: true,
            ..SessionHealthWire::default()
        });
        let (data, human) = session(&client_over(host), "s-1").await.expect("status");

        assert_eq!(data["health"]["recovery_note_owed"], true);
        assert!(
            human.contains("carry a note"),
            "and a person is told what their next message will do: {human}",
        );
    }

    /// A session the host publishes no verdict for answers promptly, and
    /// without sending the one request that would have started it.
    ///
    /// The scripted host panics on anything but `Subscribe`, so this pins the
    /// no-side-effects half too: `GetSessionInfo` builds a dormant session on
    /// serve, and reaching for it is how the first draft of this verb
    /// resurrected the agent it was asked to describe.
    #[tokio::test]
    async fn a_session_with_no_published_verdict_says_so_and_starts_nothing() {
        let (data, human) = session(&client_over(ScriptedHost::silent()), "s-1").await.expect("status");

        assert_eq!(data["health_published"], false);
        assert_eq!(data["health"], Value::Null);
        assert!(human.contains("no health published"), "{human}");
        assert!(
            human.contains("most likely"),
            "the likely reading is offered as a likelihood, not as a state: {human}",
        );
    }

    /// A verdict that arrives later than the budget is reported as *not
    /// published*, never as a session that stopped.
    ///
    /// The distinction is the whole point of this verb. Every other surface
    /// infers a state from silence, which is what makes a rate-limited
    /// session, a dead one and a slow one look alike; a diagnostic that did
    /// the same with its own timeout would be inventing exactly that.
    #[tokio::test(start_paused = true)]
    async fn a_verdict_that_arrives_late_is_not_reported_as_a_stopped_session() {
        let (data, human) = session(&client_over(ScriptedHost::slow()), "s-1").await.expect("status");

        assert_eq!(data["health_published"], false, "nothing was published in time: {data}");
        assert_eq!(data["health"], Value::Null, "and no verdict may be invented: {data}");
        assert!(
            !human.contains("not running it\n") && !human.contains("stopped"),
            "a late push is not evidence of a stopped session: {human}",
        );
    }

    /// The host decides whether the session exists and whether this caller may
    /// read it, and its answer is passed through.
    ///
    /// Not a listing check: the host's reading rule is wider than any one
    /// caller's session list — an agent's listing holds only its own session,
    /// while a parent may read the children it spawned — so gating on the
    /// listing refused a parent asking after its own child.
    #[tokio::test]
    async fn the_host_decides_whether_a_session_can_be_read() {
        use oximux_remote_proto::proto::RpcError;

        let err = session(&client_over(ScriptedHost::refusing(RpcError::UnknownSession)), "s-1")
            .await
            .expect_err("refused");
        assert_eq!(err.code, "unknown-session");

        let err = session(&client_over(ScriptedHost::refusing(RpcError::Unauthorized)), "s-1")
            .await
            .expect_err("refused");
        assert_eq!(err.code, "denied");
    }
}
