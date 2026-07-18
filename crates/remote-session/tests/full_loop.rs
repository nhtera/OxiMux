//! The end-to-end proof: the client [`RemoteSession`] drives the real
//! `remote-host` `Dispatcher` over the in-memory loopback — pairing, commands, and
//! reconnect all cross our actual wire code on both sides, with no network.

use std::sync::Arc;

use futures::executor::block_on;
use futures::future::join;
use oximux_agent_core::thread::{ChatThread, PermissionDecision, PermissionKind, ThreadEntry, ThreadEvent};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_proto::messages::SessionStatusWire;
use oximux_remote_proto::{HostEvent, PairingTicket};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_session::{ClientSigner, FoldOutcome, RemoteSession, SessionSubscription};
use serde_json::json;

/// All assistant-message text folded into a thread, whitespace-stripped — asserts
/// the fold saw the events without coupling to entry chunking or the `\n`
/// separators the fold inserts between finalized blocks.
fn assistant_text(thread: &ChatThread) -> String {
    thread
        .entries
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::Assistant(msg) => Some(msg.text.as_str()),
            _ => None,
        })
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];
const CLIENT_SEED: [u8; 32] = [7u8; 32];

fn ticket(session_id: Option<&str>) -> PairingTicket {
    PairingTicket {
        endpoint_id: [0u8; 32],
        handshake_secret: SECRET,
        session_id: session_id.map(Into::into),
    }
}

/// A registry with one session `sess-1` carrying a text event and an outstanding
/// permission request.
fn seeded_registry() -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    let conn = StubConnection::default()
        .with_capabilities(AgentCapabilities { supports_steer: true, ..Default::default() });
    let handle = registry.register("sess-1".into(), Arc::new(conn));
    handle.ingest(ThreadEvent::AssistantText("hi".into()));
    handle.ingest(ThreadEvent::PermissionRequested {
        request_id: "req-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({ "command": "ls" }),
        description: "run ls".into(),
        suggestions: vec![],
        kind: PermissionKind::Tool,
    });
    registry
}

#[test]
fn client_pairs_and_drives_a_session_over_the_loopback() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    let (client_transport, server) = duplex_pair();
    let client = RemoteSession::new(Arc::new(client_transport), ClientSigner::from_seed(&CLIENT_SEED));

    let serve = dispatcher.serve(&server);
    let script = async move {
        // Pair, then the token is cached for a fast reconnect.
        client.pair(&ticket(None), "phone", NOW).await.expect("pair");
        assert!(client.session_token().is_some(), "reconnect token cached on pair");

        // The seeded session shows up with its real seq + awaiting-permission.
        let sessions = client.list_sessions().await.expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-1");
        assert_eq!(sessions[0].last_seq, 2);
        assert!(sessions[0].awaiting_permission);

        // Gap-fill decodes back into the exact events, `Value` intact.
        let backlog = client.events_since("sess-1", 0).await.expect("events_since");
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog[0].event().unwrap(), ThreadEvent::AssistantText("hi".into()));

        // Commands ack.
        client.send_prompt("sess-1", "go", &[], 1).await.expect("send_prompt");
        client.steer("sess-1", "focus").await.expect("steer");
        client.cancel("sess-1").await.expect("cancel");

        // Resolve is idempotent: first call wins, a re-resolve reports already-decided.
        let allow = PermissionDecision::Allow { updated_input: json!({}) };
        assert!(client.resolve_permission("sess-1", "req-1", &allow).await.expect("resolve"));
        assert!(
            !client.resolve_permission("sess-1", "req-1", &allow).await.expect("re-resolve"),
            "already-decided is Ok(false), not an error"
        );
        // client dropped here → serve loop ends
    };

    block_on(join(serve, script));
}

#[test]
fn client_subscribes_and_folds_backlog_then_live_events() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let registry = seeded_registry();
    // A handle clone the script ingests into after subscribing, to drive the live
    // edge (the registry itself moves into the dispatcher).
    let handle = registry.get("sess-1").expect("seeded session");
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client_transport, server) = duplex_pair();
    let client = RemoteSession::new(Arc::new(client_transport), ClientSigner::from_seed(&CLIENT_SEED));

    let serve = dispatcher.serve(&server);
    let script = async move {
        client.pair(&ticket(None), "phone", NOW).await.expect("pair");

        // Subscribe replays the backlog (seq 1 = "hi", seq 2 = permission request).
        let mut sub = SessionSubscription::new("sess-1");
        let backlog = client.subscribe("sess-1", 0).await.expect("subscribe");
        assert_eq!(backlog.len(), 2);
        assert_eq!(sub.apply_batch(&backlog).unwrap(), FoldOutcome::Applied { seq: 2 });
        assert_eq!(sub.last_seq(), 2);
        assert_eq!(assistant_text(sub.thread()), "hi", "backlog folded into the thread");

        // A live event ingested now arrives as a pushed frame and folds in order.
        handle.ingest(ThreadEvent::AssistantText("there".into()));
        let frame = client.next_event().await.expect("next_event").expect("a live frame");
        assert_eq!(frame.seq, 3);
        assert_eq!(sub.apply(&frame).unwrap(), FoldOutcome::Applied { seq: 3 });
        assert_eq!(sub.last_seq(), 3);
        assert_eq!(assistant_text(sub.thread()), "hithere", "live event folded onto the backlog");
        // client dropped here → serve loop ends
    };

    block_on(join(serve, script));
}

/// A `seq` jump is reported as a gap (not folded); after `events_since` backfills
/// the hole, re-feeding the frames in order folds cleanly. Pure — no transport.
#[test]
fn subscription_detects_gap_and_resyncs_in_order() {
    let status = SessionStatusWire { last_seq: 5, awaiting_permission: false };
    let frame = |seq: u64, text: &str| {
        HostEvent::new("s", seq, &ThreadEvent::AssistantText(text.into()), status.clone()).unwrap()
    };

    let mut sub = SessionSubscription::new("s");
    assert_eq!(sub.apply(&frame(1, "a")).unwrap(), FoldOutcome::Applied { seq: 1 });
    assert_eq!(sub.apply(&frame(2, "b")).unwrap(), FoldOutcome::Applied { seq: 2 });

    // seq 5 skips 3,4 → gap, not folded, cursor unmoved.
    assert_eq!(sub.apply(&frame(5, "e")).unwrap(), FoldOutcome::Gap { resume_from: 2 });
    assert_eq!(sub.last_seq(), 2, "a gapped frame does not advance the cursor");

    // events_since(2) would return 3,4,5; re-feeding them in order catches up.
    assert_eq!(
        sub.apply_batch(&[frame(3, "c"), frame(4, "d"), frame(5, "e")]).unwrap(),
        FoldOutcome::Applied { seq: 5 }
    );
    assert_eq!(sub.last_seq(), 5);
    assert_eq!(assistant_text(sub.thread()), "abcde", "no gap, no dupe after resync");

    // A re-sent duplicate is ignored, cursor holds.
    assert_eq!(sub.apply(&frame(4, "d")).unwrap(), FoldOutcome::Applied { seq: 5 });
    assert_eq!(assistant_text(sub.thread()), "abcde", "duplicate not re-folded");
}

/// When the missed span has aged out of the host's backlog, an `events_since`
/// reply starts ahead of the cursor and re-feeding it yields the SAME
/// `resume_from` — the unrecoverable-history signal a driver uses to reset from a
/// fresh snapshot rather than retry forever.
#[test]
fn subscription_reports_permanent_gap_when_history_aged_out() {
    let status = SessionStatusWire { last_seq: 60, awaiting_permission: false };
    let frame = |seq: u64| {
        HostEvent::new("s", seq, &ThreadEvent::AssistantText("x".into()), status.clone()).unwrap()
    };

    let mut sub = SessionSubscription::new("s");
    // A live jump from the fresh cursor (0) to seq 51 → gap, resume_from 0.
    assert_eq!(sub.apply(&frame(51)).unwrap(), FoldOutcome::Gap { resume_from: 0 });

    // events_since(0) can only return what's still retained — say 51.. (1..=50 aged
    // out). Re-feeding still starts ahead of the cursor → the SAME resume_from.
    let retained: Vec<HostEvent> = (51..=53).map(frame).collect();
    assert_eq!(
        sub.apply_batch(&retained).unwrap(),
        FoldOutcome::Gap { resume_from: 0 },
        "unchanged resume_from across a backfill = unrecoverable, reset from snapshot"
    );
    assert_eq!(sub.last_seq(), 0, "cursor never advanced past the lost span");
}

#[test]
fn client_reconnects_via_token_fast_path_and_challenge() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    // Connection 1: pair, capture the reconnect token, then drop the connection.
    let (c1, s1) = duplex_pair();
    let client1 = RemoteSession::new(Arc::new(c1), ClientSigner::from_seed(&CLIENT_SEED));
    let token = block_on(join(dispatcher.serve(&s1), async move {
        client1.pair(&ticket(None), "phone", NOW).await.expect("pair");
        client1.session_token().expect("token issued")
    }))
    .1;

    // Connection 2: same identity, seeded token → the fast path (no challenge).
    let (c2, s2) = duplex_pair();
    let client2 = RemoteSession::new(Arc::new(c2), ClientSigner::from_seed(&CLIENT_SEED));
    client2.set_session_token(Some(token));
    block_on(join(dispatcher.serve(&s2), async move {
        client2.connect().await.expect("token reconnect");
        assert!(client2.list_sessions().await.is_ok(), "authenticated via token");
    }));

    // Connection 3: same identity, NO token → the Ed25519 challenge path.
    let (c3, s3) = duplex_pair();
    let client3 = RemoteSession::new(Arc::new(c3), ClientSigner::from_seed(&CLIENT_SEED));
    block_on(join(dispatcher.serve(&s3), async move {
        client3.connect().await.expect("challenge reconnect");
        assert!(client3.session_token().is_some(), "challenge issues a fresh token");
        assert!(client3.list_sessions().await.is_ok(), "authenticated via challenge");
    }));
}
