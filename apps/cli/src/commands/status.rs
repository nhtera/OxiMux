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
/// the spawn is a real process with a real bill. So this uses only the two
/// requests that do not build a dormant session — `ListSessions` and
/// `Subscribe` — and deliberately not `GetSessionInfo`, which is in serve's
/// build list and resurrected the very session being examined. That showed up
/// as a session reading `restoredUnconfirmed` moments after its agent had
/// exited: not the host's verdict at all, but the echo of this verb's own
/// side effect.
pub async fn session(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    client.require_version(
        oximux_remote_proto::proto::HEALTH_PUSH_MIN_VERSION,
        "status <session>",
    )?;
    // Known at all? The listing covers live and dormant sessions alike and
    // builds neither, so an unknown id is told apart from a quiet one without
    // touching anything.
    let known = match client.call(Request::ListSessions).await? {
        Response::Sessions(rows) => rows.iter().any(|s| s.session_id == session),
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListSessions", &other)),
    };
    if !known {
        return Err(rpc_failure(oximux_remote_proto::proto::RpcError::UnknownSession));
    }

    // Subscribing waits for a session rather than building it, and the host
    // sends the current health first — so this is a read. It arrives as a push
    // *after* the subscribe reply, so both arrival points are handled: one
    // during the call, one after it.
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
    let Some(frame) = health else { return Ok(not_published(session)) };
    let wire = frame.health().map_err(|e| {
        Failure::new("decode", exit::ERROR, format!("the host's health frame did not parse: {e}"))
    })?;

    let data = json!({
        "session_id": session,
        "open": true,
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
            "turn_id": wire.turn_id,
            "exit_seq": wire.exit_seq,
        },
    });
    let mut human = format!("{session}: {} ({})", wire.status, wire.liveness);
    if let Some(reason) = &wire.reason {
        human.push_str(&format!("\n{reason}"));
    }
    if wire.held_restored > 0 {
        human.push_str(&format!(
            "\n{} message(s) held from before a restart — send or discard them to release the queue",
            wire.held_restored
        ));
    }
    Ok((data, human))
}

/// The answer for a session this host knows but has published no health for.
///
/// Worded for what was observed rather than for what it probably means. The
/// ordinary cause is a session this host is not running — most of them, on a
/// server — but a pump that registered its session and then wedged before
/// publishing looks the same from here, and that is worth knowing rather than
/// being told "not running".
fn not_published(session: &str) -> (Value, String) {
    (
        json!({ "session_id": session, "open": false, "health": Value::Null }),
        format!(
            "{session}: no health published — this host is not running it\n\
             send it a message to open it (`oximux ls` shows every session it knows)"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::delivery::{SessionHealthFrame, SessionHealthWire};
    use oximux_remote_proto::messages::SessionSummary;
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
        /// Frames to hand back, oldest first.
        outbox: Mutex<VecDeque<Vec<u8>>>,
        /// Whether the session is in the host's registry.
        live: bool,
        /// What `ListSessions` knows, for telling dormant from unknown.
        listed: Vec<&'static str>,
        health: SessionHealthWire,
    }

    impl ScriptedHost {
        fn live_with(health: SessionHealthWire) -> Arc<Self> {
            Arc::new(Self {
                outbox: Mutex::new(VecDeque::new()),
                live: true,
                listed: vec!["s-1"],
                health,
            })
        }

        fn dormant(listed: Vec<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                outbox: Mutex::new(VecDeque::new()),
                live: false,
                listed,
                health: SessionHealthWire::default(),
            })
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
            match Request::from_bytes(&frame).expect("a request") {
                // `GetSessionInfo` is absent on purpose: it builds a dormant
                // session on serve, so a status read that sent it would start
                // the agent it was asked about. The panic arm below is what
                // keeps that from coming back.
                Request::ListSessions => self.queue(vec![Response::Sessions(
                    self.listed
                        .iter()
                        .map(|id| SessionSummary {
                            session_id: (*id).to_string(),
                            title: String::new(),
                            model: None,
                            last_seq: 0,
                            awaiting_permission: false,
                        })
                        .collect(),
                )]),
                Request::Subscribe { session_id, .. } if self.live => self.queue(vec![
                    // The reply, then an event that is not what we asked for,
                    // then the push that is. A verb that stopped at the first
                    // push would read the event and never find the health.
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
                    Response::SessionHealth(SessionHealthFrame::new(
                        &session_id,
                        "host-instance-7",
                        &self.health,
                    )),
                ]),
                // A session nothing is running: the subscribe is answered and
                // then simply never pushes health, which is what a dormant
                // session looks like from a client.
                Request::Subscribe { .. } => self.queue(vec![Response::Events(vec![])]),
                other => panic!("status should not have sent {other:?}"),
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
            turn_id: "inc-1:3".to_string().into(),
            ..SessionHealthWire::default()
        });
        let (data, human) = session(&client_over(host), "s-1").await.expect("status");

        assert_eq!(data["open"], true);
        assert_eq!(data["health"]["status"], "limited");
        assert_eq!(data["health"]["liveness"], "live");
        assert_eq!(data["health"]["limited_until"], 1_700_000_060_000i64);
        assert_eq!(data["health"]["held_restored"], 2);
        assert_eq!(data["health"]["turn_id"], "inc-1:3");
        assert_eq!(
            data["host_instance_id"], "host-instance-7",
            "which host process said so, so a reconnect can tell a restart",
        );
        assert!(human.contains("limited"), "{human}");
        assert!(human.contains("rate limited until 09:01"), "the reason is for a person: {human}");
        assert!(human.contains("held from before a restart"), "a held queue is actionable: {human}");
    }

    /// A session nothing is running answers promptly, saying no health was
    /// published — and without sending the one request that would have
    /// started it.
    ///
    /// The scripted host panics on `GetSessionInfo`, so this also pins the
    /// no-side-effects half: that request builds a dormant session on serve,
    /// and reaching for it here is how the first draft of this verb
    /// resurrected the agent it was asked to describe.
    #[tokio::test]
    async fn a_session_nothing_is_running_says_so_and_starts_nothing() {
        let (data, human) =
            session(&client_over(ScriptedHost::dormant(vec!["s-1"])), "s-1").await.expect("status");

        assert_eq!(data["open"], false);
        assert_eq!(data["health"], Value::Null);
        assert!(human.contains("not running it"), "{human}");
    }

    /// An id this host has never heard of is refused, not reported as dormant.
    #[tokio::test]
    async fn an_unknown_session_is_refused() {
        let err = session(&client_over(ScriptedHost::dormant(vec!["someone-else"])), "s-1")
            .await
            .expect_err("refused");
        assert_eq!(err.code, "unknown-session");
    }
}
