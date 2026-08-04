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
    assert_eq!(PROTOCOL_VERSION, 16, "v16 = the appended CLI working set");
}

/// The floor moves only on a genuinely breaking change, never merely because
/// [`PROTOCOL_VERSION`] moved. Raising it strands every peer below it, so this
/// tripwire forces that to be a deliberate edit rather than a reflex bump
/// alongside an append.
#[test]
fn min_compatible_version_is_pinned() {
    assert_eq!(
        MIN_COMPATIBLE_VERSION, 1,
        "every version so far is append-only, so v1 peers are still understood"
    );
}

/// The core policy: an append-only wire means *older is fine*. If this ever
/// flips to equality-matching, shipping one appended RPC on the desktop would
/// disconnect every already-paired phone until each one updated.
#[test]
fn an_older_peer_is_still_compatible() {
    assert!(is_compatible(1), "a v1 peer never sends the appended calls — it is serviceable");
    assert!(is_compatible(PROTOCOL_VERSION));
}

/// A newer peer is accepted too: it knows we are older and is responsible for
/// confining itself to what we understand. Refusing it would force both ends to
/// upgrade in lock-step — the coupling this handshake exists to remove.
#[test]
fn a_newer_peer_is_compatible() {
    assert!(is_compatible(PROTOCOL_VERSION + 10));
}

/// Silence is a version claim, not a missing one: a peer predating the handshake
/// *is* a v1 peer, so the gate still applies to it.
#[test]
fn a_silent_peer_is_read_as_v1() {
    assert_eq!(ASSUMED_VERSION_WHEN_SILENT, 1);
    assert_eq!(is_compatible(ASSUMED_VERSION_WHEN_SILENT), is_compatible(1));
}

/// `Hello`/`HelloAck` were appended, so every pre-existing variant must keep its
/// ordinal — the whole basis for old peers still working. Encoding a v1-era
/// request and decoding it back proves the ordinals did not shift.
#[test]
fn appending_the_handshake_kept_earlier_variants_stable() {
    let ping = Request::Ping;
    let bytes = ping.to_bytes().expect("encode");
    assert_eq!(Request::from_bytes(&bytes).expect("decode"), ping);

    let hello = Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION });
    let bytes = hello.to_bytes().expect("encode");
    assert_eq!(Request::from_bytes(&bytes).expect("decode"), hello);
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
        Request::ListChoices { session_id: "s1".into() },
        Request::SetModel { session_id: "s1".into(), model: "opus-5".into() },
        Request::SetPermissionMode { session_id: "s1".into(), mode: "plan".into() },
        Request::FetchTranscript { session_id: "s1".into() },
        Request::ListProjects,
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
        // Empty lists are a legitimate answer (a backend with no catalog), so the
        // populated and empty shapes both have to survive the wire.
        Response::Choices(SessionChoices {
            models: vec![Choice {
                id: "opus-5".into(),
                label: "Opus 5".into(),
                description: Some("most capable".into()),
            }],
            modes: vec![],
            current_model: Some("opus-5".into()),
            current_mode: None,
        }),
        Response::Choices(SessionChoices {
            models: vec![],
            modes: vec![],
            current_model: None,
            current_mode: None,
        }),
        Response::SessionTranscript(SessionTranscriptWire {
            session_id: "s1".into(),
            seq: 7,
            entries_json: "[]".into(),
            model: Some("claude".into()),
        }),
        Response::Projects(vec![ProjectSummaryWire {
            name: "OxiMux".into(),
            path: "/Users/me/Code/OxiMux".into(),
        }]),
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

/// The append-only guarantee, asserted on **bytes** rather than a round trip.
///
/// Encoding and decoding with the same binary agree no matter where the ordinals
/// sit, so `appending_..._kept_earlier_variants_stable` above cannot actually
/// catch an inserted variant — it would pass while every already-paired phone
/// silently misread every call. Postcard writes an enum as its variant index, so
/// pinning the literal byte is what makes an insertion fail here instead of in
/// the field.
///
/// `Ping` is the 4th variant, so it encodes as a single `3`. If this fails,
/// someone inserted a variant above it rather than appending — that is a
/// breaking change and needs `MIN_COMPATIBLE_VERSION` raised, not just a
/// `PROTOCOL_VERSION` bump.
#[test]
fn early_variants_keep_their_literal_ordinals() {
    assert_eq!(Request::Ping.to_bytes().expect("encode"), vec![3]);
    assert_eq!(Request::ListSessions.to_bytes().expect("encode"), vec![4]);
    // The v10 schedule surface appended after the v9 forge tail: `ListSchedules`
    // is the 33rd variant (index 32). Pinning it catches an insertion anywhere in
    // the 32 variants before it, which would shift every paired phone's decode.
    assert_eq!(Request::ListSchedules.to_bytes().expect("encode"), vec![32]);
    // The v11 voice surface appended after the schedule tail: `TranscribeAudio`
    // is the 38th variant (index 37). Its first encoded byte is that ordinal;
    // the string + varint payload follows, so pin only the discriminant.
    let transcribe = Request::TranscribeAudio { audio_base64: String::new(), sample_rate: 0 };
    assert_eq!(transcribe.to_bytes().expect("encode")[0], 37);
    // The v12 session-list subscription appended after the voice tail:
    // `SubscribeSessions` is the 39th Request variant (index 38, a unit variant, so
    // its whole encoding is that single ordinal byte).
    assert_eq!(Request::SubscribeSessions.to_bytes().expect("encode"), vec![38]);
    // The v13 transcript-snapshot fetch appended after the subscription tail:
    // `FetchTranscript` is the 40th Request variant (index 39). Pin the discriminant;
    // the `session_id` string payload follows.
    let fetch = Request::FetchTranscript { session_id: String::new() };
    assert_eq!(fetch.to_bytes().expect("encode")[0], 39);
    // The v14 project list appended after the transcript-fetch tail: `ListProjects`
    // is the 41st Request variant (index 40, a unit variant, so its whole encoding
    // is that single ordinal byte).
    assert_eq!(Request::ListProjects.to_bytes().expect("encode"), vec![40]);
    // `SessionsChanged` is the 29th Response variant (index 28); pin the
    // discriminant, the `Vec` payload follows.
    let changed = Response::SessionsChanged(vec![]);
    assert_eq!(changed.to_bytes().expect("encode")[0], 28);
    // `SessionTranscript` is the 30th Response variant (index 29), appended after
    // `SessionsChanged`.
    let transcript = Response::SessionTranscript(SessionTranscriptWire {
        session_id: String::new(),
        seq: 0,
        entries_json: "[]".into(),
        model: None,
    });
    assert_eq!(transcript.to_bytes().expect("encode")[0], 29);
    // `Projects` is the 31st Response variant (index 30), appended after
    // `SessionTranscript`.
    let projects = Response::Projects(vec![]);
    assert_eq!(projects.to_bytes().expect("encode")[0], 30);
    // The v15 self-unpair appended after the project tail: `Unpair` is the 42nd
    // Request variant (index 41, a unit variant, so its whole encoding is that
    // single ordinal byte).
    assert_eq!(Request::Unpair.to_bytes().expect("encode"), vec![41]);
    // The v16 CLI working set appended after the unpair tail, in this order:
    // `FetchTranscriptPage` (index 42), `CreateWorktree` (43), `ListWorktrees`
    // (44), `RemoveWorktree` (45). Pin each discriminant; payloads follow.
    let page = Request::FetchTranscriptPage { session_id: String::new(), cursor: 0, limit: 0 };
    assert_eq!(page.to_bytes().expect("encode")[0], 42);
    let create = Request::CreateWorktree { project_path: String::new(), slug: String::new() };
    assert_eq!(create.to_bytes().expect("encode")[0], 43);
    let list = Request::ListWorktrees { project_path: None };
    assert_eq!(list.to_bytes().expect("encode")[0], 44);
    let remove = Request::RemoveWorktree { id: String::new() };
    assert_eq!(remove.to_bytes().expect("encode")[0], 45);
    // The matching v16 Response tail: `TranscriptPage` is the 32nd Response
    // variant (index 31), `WorktreeCreated` 33rd (32), `Worktrees` 34th (33).
    let page = Response::TranscriptPage(TranscriptPageWire {
        session_id: String::new(),
        seq: 0,
        entries_json: "[]".into(),
        next_cursor: None,
        total: 0,
        model: None,
    });
    assert_eq!(page.to_bytes().expect("encode")[0], 31);
    let wt = WorktreeWire {
        id: String::new(),
        project_path: String::new(),
        name: String::new(),
        slug: String::new(),
        branch: String::new(),
        path: String::new(),
    };
    assert_eq!(Response::WorktreeCreated(wt).to_bytes().expect("encode")[0], 32);
    assert_eq!(Response::Worktrees(vec![]).to_bytes().expect("encode")[0], 33);
    // `RpcError::Unsupported` appended after `IncompatibleVersion`: index 6
    // inside the error enum, which rides behind `Error`'s own ordinal (8).
    let err = Response::Error(RpcError::Unsupported);
    assert_eq!(err.to_bytes().expect("encode"), vec![8, 6]);
}
