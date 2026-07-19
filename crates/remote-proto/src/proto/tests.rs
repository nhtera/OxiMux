use super::*;
use oximux_agent_core::thread::{PermissionDecision, ThreadEvent};
use serde_json::json;

/// A `ThreadEvent` variant that carries a populated `serde_json::Value` — the
/// exact shape the postcard-vs-Value limitation bites on.
fn value_bearing_event() -> ThreadEvent {
    ThreadEvent::ToolCallStarted {
        id: "call-1".into(),
        name: "Bash".into(),
        input: json!({ "command": "ls -la", "nested": { "n": 1, "list": [true, null] } }),
    }
}

/// A deliberate tripwire: the version is only allowed to move together with a
/// documented wire change, so an accidental edit fails here.
#[test]
fn protocol_version_is_pinned() {
    assert_eq!(PROTOCOL_VERSION, 2, "v2 = the appended git surface");
}

/// The evidence behind the JSON-in-envelope design: postcard is
/// non-self-describing, so it cannot reconstruct a `serde_json::Value`
/// (`deserialize_any` → `WontImplement`). If this ever starts passing, native
/// postcard for `ThreadEvent` becomes viable and the envelope can be simplified.
#[test]
fn postcard_cannot_round_trip_a_value_bearing_thread_event() {
    let event = value_bearing_event();
    let round_tripped = postcard::to_stdvec(&event)
        .ok()
        .and_then(|bytes| postcard::from_bytes::<ThreadEvent>(&bytes).ok());
    assert!(
        round_tripped.is_none(),
        "postcard unexpectedly decoded a Value-bearing ThreadEvent — the JSON payload workaround may be removable"
    );
}

/// The design in force: the event rides as a JSON string inside a postcard
/// envelope, so the `Value` survives intact.
#[test]
fn host_event_carries_a_value_bearing_event_through_postcard() {
    let event = value_bearing_event();
    let status = SessionStatusWire { last_seq: 42, awaiting_permission: false };
    let frame = HostEvent::new("sess-1", 42, &event, status).expect("encode frame");

    let bytes = frame.to_bytes_via_response();
    let decoded = HostEvent::from_response_bytes(&bytes);

    assert_eq!(decoded.session_id, "sess-1");
    assert_eq!(decoded.seq, 42);
    assert_eq!(decoded.event().expect("decode event"), event);
}

/// Every request variant survives the postcard envelope round-trip.
#[test]
fn requests_round_trip_via_postcard() {
    let requests = vec![
        Request::Register(RegisterReq {
            app_pubkey: [7u8; 32],
            device_name: "Tien's iPhone".into(),
            proof: [3u8; 32],
            timestamp_secs: 1_700_000_000,
            session_id: Some("s1".into()),
        }),
        Request::Connect(ConnectReq { app_pubkey: [1u8; 32], session_token: None }),
        Request::AuthProve(AuthProveReq { signature: vec![9u8; 64] }),
        Request::Ping,
        Request::ListSessions,
        Request::GetSessionInfo { session_id: "s1".into() },
        Request::Steer { session_id: "s1".into(), text: "focus on tests".into() },
        Request::Cancel { session_id: "s1".into() },
        Request::Subscribe { session_id: "s1".into(), after_seq: Some(10) },
        Request::EventsSince { session_id: "s1".into(), after_seq: 10 },
    ];
    for req in requests {
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
    }
}

/// `SendPrompt` (with image attachments) and the `Response` side round-trip too.
#[test]
fn send_prompt_and_responses_round_trip() {
    use oximux_agent_core::thread::ChatImage;
    let req = Request::SendPrompt(SendPromptReq {
        session_id: "s1".into(),
        text: "hello".into(),
        images: vec![ChatImage { media_type: "image/png".into(), data: "aGVsbG8=".into() }],
        corr_id: 99,
    });
    let bytes = req.to_bytes().expect("encode");
    assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);

    let responses = vec![
        Response::Registered { session_token: "tok".into() },
        Response::Challenge { nonce: [5u8; 32] },
        Response::Connected { session_token: "tok2".into() },
        Response::Pong,
        Response::Sessions(vec![SessionSummary {
            session_id: "s1".into(),
            title: "Fix parser".into(),
            model: Some("claude".into()),
            last_seq: 3,
            awaiting_permission: true,
        }]),
        Response::Ack,
        Response::Error(RpcError::AlreadyDecided),
        Response::Error(RpcError::BadRequest("no challenge outstanding".into())),
    ];
    for resp in responses {
        let bytes = resp.to_bytes().expect("encode");
        assert_eq!(Response::from_bytes(&bytes).expect("decode"), resp);
    }
}

/// A `Response::Events` carrying frames round-trips, proving the JSON-in-postcard
/// envelope nests cleanly inside a postcard response.
#[test]
fn events_response_round_trips() {
    let status = SessionStatusWire { last_seq: 2, awaiting_permission: false };
    let frames = vec![
        HostEvent::new("s1", 1, &ThreadEvent::AssistantText("done".into()), status.clone())
            .unwrap(),
        HostEvent::new("s1", 2, &value_bearing_event(), status).unwrap(),
    ];
    let resp = Response::Events(frames.clone());
    let bytes = resp.to_bytes().expect("encode");
    let Response::Events(decoded) = Response::from_bytes(&bytes).expect("decode") else {
        panic!("expected Events");
    };
    assert_eq!(decoded, frames);
    assert_eq!(decoded[1].event().unwrap(), value_bearing_event());
}

/// `ResolvePermission` carries the `Value`-bearing decision as JSON and the
/// whole request still postcard-round-trips.
#[test]
fn resolve_permission_carries_decision_as_json() {
    let decision =
        PermissionDecision::Allow { updated_input: json!({ "command": "rm -rf /tmp/x" }) };
    let payload = ResolvePermissionReq::new("s1", "req-7", &decision).expect("encode decision");
    assert_eq!(payload.decision().expect("decode decision"), decision);

    let req = Request::ResolvePermission(payload);
    let bytes = req.to_bytes().expect("encode");
    let Request::ResolvePermission(decoded) = Request::from_bytes(&bytes).expect("decode") else {
        panic!("expected ResolvePermission");
    };
    assert_eq!(decoded.decision().unwrap(), decision);
}

// Small test-only helpers so `host_event_carries_...` proves the frame survives a
// real postcard envelope, not just a struct clone.
impl HostEvent {
    fn to_bytes_via_response(&self) -> Vec<u8> {
        Response::Events(vec![self.clone()]).to_bytes().expect("encode")
    }
    fn from_response_bytes(bytes: &[u8]) -> Self {
        match Response::from_bytes(bytes).expect("decode") {
            Response::Events(mut v) => v.remove(0),
            _ => panic!("expected Events"),
        }
    }
}
