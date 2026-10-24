use super::*;
use crate::simulator::*;
use oximux_agent_core::thread::{PermissionDecision, ThreadEvent};
use serde::Deserialize;
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
    assert_eq!(
        PROTOCOL_VERSION, 36,
        "v36 = agent delegation (SpawnAgent, ListChildren, CancelAgent, Whoami, their replies, RpcError::Spawn). v35 = opening a folder in the desktop (OpenProject, ProjectOpened). v34 = watching a device from the paired phone (SimViewables, SimSubscribe, SimUnsubscribe, their replies). v33 = the MCP principal (local hello `Mcp`; no frame change). v32 = simulator React Native (Rn, its reply; Net served). v31 = simulator exec (Exec, ExecOutput). v30 = simulator flows (Flow, its reply, SimTargetWire::Exact). v29 = simulator diagnostics (Logs, Crashes, their replies). v28 = simulator device state (DeviceState: location, appearance, privacy, push, \
         status bar, erase, the Net shape). v27 = the simulator verify loop (Settled/AxRefs/Wait/Assert/AsCaller, \
         SimTargetWire::Ref, their replies and errors). v26 = Android's buttons on the simulator surface (SimButtonWire::{{Back, \
         VolumeUp, VolumeDown}}). v25 = the iOS Simulator surface (Simulator request \
         + Simulator reply, the verb set in its own append-only enums)"
    );
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
        Request::OpenProject { path: "/Users/me/Code/OxiMux".into() },
        Request::SpawnAgent(crate::lineage::SpawnAgentReq {
            request_id: "r-1".into(),
            parent: crate::lineage::SpawnParentWire::Caller,
            agent_id: "claude-code".into(),
            model: Some("sonnet".into()),
            prompt: "review the diff".into(),
            cwd: crate::lineage::SpawnCwdWire::NewWorktree { name: Some("review".into()) },
            output_schema: None,
            notify_parent: crate::lineage::NotifyParentWire::Stuck,
            allow_parent_permits: true,
            posture: crate::lineage::SpawnPostureWire::Review,
        }),
        Request::ListChildren { of: "s1".into(), recursive: true },
        Request::CancelAgent { id: "team:run-1".into(), cascade: true },
        Request::Whoami,
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
        Response::ProjectOpened {
            project: ProjectSummaryWire { name: "OxiMux".into(), path: "/Users/me/Code/OxiMux".into() },
            worktree: Some("/Users/me/Code/OxiMux-feature".into()),
        },
        Response::AgentSpawned(crate::lineage::SpawnAgentResult {
            session_id: "c1".into(),
            parent_id: "s1".into(),
            root_id: "s1".into(),
            depth: 1,
            replayed: false,
        }),
        Response::Children(vec![crate::lineage::LineageWire {
            session_id: "c1".into(),
            parent_id: "s1".into(),
            root_id: "s1".into(),
            depth: 1,
            posture: crate::lineage::SpawnPostureWire::Default,
            state: crate::lineage::LineageStateWire::Parked,
            notify_parent: crate::lineage::NotifyParentWire::Auto,
            identity_slot: Some(2),
            created_at: "2026-10-24T12:00:00+07:00".into(),
        }]),
        Response::AgentsCancelled { ended: vec!["g1".into(), "c1".into()] },
        Response::Whoami(crate::lineage::WhoamiWire {
            scope: "session".into(),
            session: Some("s1".into()),
            depth: 0,
            root_id: Some("s1".into()),
            spawn_allowed: true,
            reason: None,
            agents: vec![crate::lineage::AgentRosterWire {
                agent_id: "claude-code".into(),
                display: "Claude".into(),
                default_model: Some("sonnet".into()),
                models: vec![],
                postures: vec![crate::lineage::SpawnPostureWire::Default, crate::lineage::SpawnPostureWire::Review],
                installed: true,
            }],
        }),
        Response::Error(RpcError::Spawn(crate::lineage::SpawnErrorWire {
            kind: crate::lineage::SpawnErrorKind::FanoutExceeded,
            message: "Spawn refused: 8 of 8 active children. Wait on a child, or cancel one.".into(),
        })),
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
    // The v35 folder open, appended last: `OpenProject` is Request ordinal 73
    // and its `ProjectOpened` reply Response ordinal 55.
    assert_eq!(Request::OpenProject { path: String::new() }.to_bytes().expect("encode")[0], 73);
    let opened = Response::ProjectOpened {
        project: ProjectSummaryWire { name: String::new(), path: String::new() },
        worktree: None,
    };
    assert_eq!(opened.to_bytes().expect("encode")[0], 55);
    // The v36 delegation verbs, appended after the folder open: Request
    // ordinals 74..=77, Response ordinals 56..=59, and `RpcError::Spawn` is
    // error ordinal 8 (after `Unsupported`).
    assert_eq!(Request::Whoami.to_bytes().expect("encode"), vec![77]);
    let list = Request::ListChildren { of: String::new(), recursive: false };
    assert_eq!(list.to_bytes().expect("encode")[0], 75);
    let cancelled = Response::AgentsCancelled { ended: vec![] };
    assert_eq!(cancelled.to_bytes().expect("encode")[0], 58);
    let refused = Response::Error(RpcError::Spawn(crate::lineage::SpawnErrorWire {
        kind: crate::lineage::SpawnErrorKind::DepthExceeded,
        message: String::new(),
    }));
    let bytes = refused.to_bytes().expect("encode");
    let unsupported = Response::Error(RpcError::Unsupported).to_bytes().expect("encode");
    assert_eq!(bytes[0], unsupported[0], "same Response::Error ordinal");
    assert_eq!(bytes[1], unsupported[1] + 1, "Spawn follows Unsupported");
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
    // The v16 pairing-administration tail: `PairNew` (index 46), `PairList`
    // (47, a unit variant), `PairRemove` (48).
    let pair_new = Request::PairNew { read_only: false };
    assert_eq!(pair_new.to_bytes().expect("encode")[0], 46);
    assert_eq!(Request::PairList.to_bytes().expect("encode"), vec![47]);
    let pair_rm = Request::PairRemove { pubkey: [0u8; 32] };
    assert_eq!(pair_rm.to_bytes().expect("encode")[0], 48);
    // Their replies: `PairingIssued` is the 35th Response variant (index 34),
    // `PairedDeviceList` the 36th (35).
    let issued = Response::PairingIssued(PairingIssuedWire {
        ticket: String::new(),
        expires_at: 0,
        read_only: false,
    });
    assert_eq!(issued.to_bytes().expect("encode")[0], 34);
    assert_eq!(Response::PairedDeviceList(vec![]).to_bytes().expect("encode")[0], 35);
    // The v17 schedule tail: `RunScheduleNow` (index 49); its reply
    // `ScheduleRunRecorded` (36) and the `ScheduleRunsChanged` push (37).
    let run_now = Request::RunScheduleNow { schedule_id: "sch-1".into() };
    assert_eq!(run_now.to_bytes().expect("encode")[0], 49);
    let run = ScheduleRunWire {
        schedule_id: String::new(),
        fired_at: String::new(),
        outcome: RunOutcomeWire::Ok,
        session_id: None,
        detail: None,
    };
    assert_eq!(Response::ScheduleRunRecorded(run.clone()).to_bytes().expect("encode")[0], 36);
    assert_eq!(Response::ScheduleRunsChanged(run).to_bytes().expect("encode")[0], 37);
    // The v18 automation tail, appended in one batch after `RunScheduleNow`:
    // `CreateHeartbeat` (50), `ListHeartbeats` (51), `DeleteHeartbeat` (52),
    // `TeamRunCreate` (53), `TeamReport` (54), `TeamStatus` (55), `TeamList`
    // (56, a unit variant), `StateGet` (57), `StateSet` (58), `StateDelete`
    // (59), `StateWatch` (60).
    let heartbeat = Request::CreateHeartbeat(CreateHeartbeatReq {
        session_id: None,
        name: String::new(),
        prompt: String::new(),
        recurrence: RecurrenceWire::EveryMinutes { minutes: 5 },
    });
    assert_eq!(heartbeat.to_bytes().expect("encode")[0], 50);
    assert_eq!(
        Request::ListHeartbeats { session_id: None }.to_bytes().expect("encode")[0],
        51
    );
    let delete_hb = Request::DeleteHeartbeat { id: String::new() };
    assert_eq!(delete_hb.to_bytes().expect("encode")[0], 52);
    let team_create = Request::TeamRunCreate(TeamRunCreateReq {
        name: String::new(),
        cwd: String::new(),
        agent_id: None,
        worktree_each: false,
        roles: vec![],
    });
    assert_eq!(team_create.to_bytes().expect("encode")[0], 53);
    let report = Request::TeamReport(TeamReportReq {
        run_id: String::new(),
        role: String::new(),
        ok: true,
        summary: None,
    });
    assert_eq!(report.to_bytes().expect("encode")[0], 54);
    let status = Request::TeamStatus { run_id: String::new() };
    assert_eq!(status.to_bytes().expect("encode")[0], 55);
    assert_eq!(Request::TeamList.to_bytes().expect("encode"), vec![56]);
    let state_get = Request::StateGet { key: String::new() };
    assert_eq!(state_get.to_bytes().expect("encode")[0], 57);
    let state_set = Request::StateSet(StateSetReq {
        key: String::new(),
        value_json: String::new(),
        if_version: None,
    });
    assert_eq!(state_set.to_bytes().expect("encode")[0], 58);
    let state_del = Request::StateDelete { key: String::new() };
    assert_eq!(state_del.to_bytes().expect("encode")[0], 59);
    let state_watch = Request::StateWatch { prefix: None };
    assert_eq!(state_watch.to_bytes().expect("encode")[0], 60);
    // Their replies, in the same batch: `HeartbeatCreated` (38), `Heartbeats`
    // (39), `TeamRun` (40), `TeamRuns` (41), `StateValue` (42),
    // `StateSnapshot` (43), `StateChanged` (44).
    let hb = HeartbeatWire {
        id: String::new(),
        session_id: String::new(),
        name: String::new(),
        prompt: String::new(),
        recurrence: RecurrenceWire::EveryMinutes { minutes: 5 },
        enabled: true,
        next_fire_at: String::new(),
        summary: String::new(),
    };
    assert_eq!(Response::HeartbeatCreated(hb).to_bytes().expect("encode")[0], 38);
    assert_eq!(Response::Heartbeats(vec![]).to_bytes().expect("encode")[0], 39);
    let team = TeamRunWire {
        id: String::new(),
        name: String::new(),
        cwd: String::new(),
        created_at: String::new(),
        closed: false,
        roles: vec![],
    };
    assert_eq!(Response::TeamRun(team).to_bytes().expect("encode")[0], 40);
    assert_eq!(Response::TeamRuns(vec![]).to_bytes().expect("encode")[0], 41);
    assert_eq!(Response::StateValue(None).to_bytes().expect("encode")[0], 42);
    assert_eq!(Response::StateSnapshot(vec![]).to_bytes().expect("encode")[0], 43);
    let changed = Response::StateChanged { key: String::new(), entry: None };
    assert_eq!(changed.to_bytes().expect("encode")[0], 44);
    assert_eq!(Response::StateConflict(None).to_bytes().expect("encode")[0], 45);

    // v19, appended after the v18 batch: `StateWatchFrom` (61) and its replies
    // `StateWatchStarted` (46), `StateChangedAt` (47). The cursor-less
    // `StateWatch` (60) / `StateChanged` (44) keep their ordinals — a v18 peer
    // must go on speaking exactly what it already spoke.
    let watch_from = Request::StateWatchFrom { prefix: None, since_seq: None };
    assert_eq!(watch_from.to_bytes().expect("encode")[0], 61);
    let started = Response::StateWatchStarted(StateWatchStartedWire {
        seq: 0,
        baseline: None,
        replay: vec![],
    });
    assert_eq!(started.to_bytes().expect("encode")[0], 46);
    let changed_at = Response::StateChangedAt(StateChangeWire {
        seq: 0,
        key: String::new(),
        entry: None,
    });
    assert_eq!(changed_at.to_bytes().expect("encode")[0], 47);

    // v21: the worktree progress sidecar — `SetWorktreeProgress` (62),
    // `ListWorktreeProgress` (63), and `WorktreeProgress` (48).
    let set_progress =
        Request::SetWorktreeProgress { id: String::new(), comment: None, phase: None };
    assert_eq!(set_progress.to_bytes().expect("encode")[0], 62);
    let list_progress = Request::ListWorktreeProgress { project_path: None };
    assert_eq!(list_progress.to_bytes().expect("encode")[0], 63);
    assert_eq!(Response::WorktreeProgress(vec![]).to_bytes().expect("encode")[0], 48);

    // v22: per-role agents — `TeamRunCreateV2` (64), `TeamStatusV2` (65), and
    // `TeamRunV2` (49). The v18 team verbs (53-56) and `TeamRun` (40) keep
    // their ordinals above: a stale CLI must go on speaking what it spoke.
    let create_v2 = Request::TeamRunCreateV2(TeamRunCreateV2Req {
        name: String::new(),
        cwd: String::new(),
        agent_id: None,
        worktree_each: false,
        roles: vec![],
    });
    assert_eq!(create_v2.to_bytes().expect("encode")[0], 64);
    let status_v2 = Request::TeamStatusV2 { run_id: String::new() };
    assert_eq!(status_v2.to_bytes().expect("encode")[0], 65);
    let run_v2 = TeamRunV2Wire {
        id: String::new(),
        name: String::new(),
        cwd: String::new(),
        created_at: String::new(),
        closed: false,
        roles: vec![],
    };
    assert_eq!(Response::TeamRunV2(run_v2).to_bytes().expect("encode")[0], 49);

    // v23: cron schedules — `CreateScheduleV2` (66), `ListSchedulesV2` (67),
    // and the `SchedulesV2` (50) / `ScheduleCreatedV2` (51) replies. The v10
    // schedule verbs (`ListSchedules` 32, `CreateSchedule` 33) and their
    // replies keep their ordinals: a stale phone must go on speaking what it
    // spoke, and it is the one the stand-in recurrence exists for.
    assert_eq!(Request::ListSchedules.to_bytes().expect("encode")[0], 32);
    let create_v1 = Request::CreateSchedule {
        name: String::new(),
        cwd: String::new(),
        prompt: String::new(),
        agent_id: None,
        recurrence: RecurrenceWire::EveryMinutes { minutes: 5 },
    };
    assert_eq!(create_v1.to_bytes().expect("encode")[0], 33);
    let create_sched_v2 = Request::CreateScheduleV2 {
        name: String::new(),
        cwd: String::new(),
        prompt: String::new(),
        agent_id: None,
        recurrence: RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() },
    };
    assert_eq!(create_sched_v2.to_bytes().expect("encode")[0], 66);
    assert_eq!(Request::ListSchedulesV2.to_bytes().expect("encode")[0], 67);

    // v24: worktree base refs — `CreateWorktreeV2` (68), answered by the
    // existing `WorktreeCreated` (32). `CreateWorktree` (43) is pinned above
    // and stays served; the two verbs are interchangeable when the base is
    // `Default`, which is what lets the client gate on the verb.
    let create_wt_v2 = Request::CreateWorktreeV2 {
        project_path: String::new(),
        slug: String::new(),
        base: CreateBaseWire::Default,
    };
    assert_eq!(create_wt_v2.to_bytes().expect("encode")[0], 68);
    assert_eq!(Response::SchedulesV2(vec![]).to_bytes().expect("encode")[0], 50);
    let sched_v2 = ScheduleV2Wire {
        id: String::new(),
        name: String::new(),
        cwd: String::new(),
        prompt: String::new(),
        agent_id: None,
        recurrence: RecurrenceV2Wire::EveryMinutes { minutes: 5 },
        enabled: false,
        next_fire_at: String::new(),
        summary: String::new(),
    };
    assert_eq!(Response::ScheduleCreatedV2(sched_v2).to_bytes().expect("encode")[0], 51);

    // v25: the simulator — one request (69) and one reply (52) for every verb.
    let sim = Request::Simulator(SimRequestWire { worktree: None, cmd: SimCmdWire::Status });
    assert_eq!(sim.to_bytes().expect("encode")[0], 69);
    assert_eq!(Response::Simulator(Ok(SimReplyWire::Done)).to_bytes().expect("encode")[0], 52);
}

/// Every simulator verb and reply survives the codec, and the verb enum's
/// ordinals are pinned: a CLI and a desktop built a release apart must agree
/// on what `Tap` means.
#[test]
fn simulator_verbs_round_trip_and_keep_their_ordinals() {
    let point = SimPointWire { x: 10.5, y: 20.0 };
    let cmds = [
        SimCmdWire::Status,
        SimCmdWire::Devices,
        SimCmdWire::Attach { device: Some("iPhone 17 Pro".into()) },
        SimCmdWire::Detach,
        SimCmdWire::Screenshot { full: false },
        SimCmdWire::Ax { max: 200 },
        SimCmdWire::Tap(SimTargetWire::Label("Settings".into())),
        SimCmdWire::Swipe { from: point, to: SimPointWire { x: 1.0, y: 2.0 }, duration_ms: 300 },
        SimCmdWire::Type { text: "héllo".into(), paste: true },
        SimCmdWire::Button(SimButtonWire::AppSwitcher),
        SimCmdWire::Rotate(SimOrientationWire::LandscapeLeft),
        SimCmdWire::Launch { bundle_id: "com.apple.Preferences".into(), relaunch: true },
        SimCmdWire::OpenUrl { url: "https://example.com".into() },
        SimCmdWire::Install { path: "/w/Build/App.app".into() },
        SimCmdWire::Shutdown { force: false },
    ];
    // v26: Android's buttons append to the button enum, after the iOS ones.
    for (ordinal, button) in [(5u8, SimButtonWire::Back), (6, SimButtonWire::VolumeUp), (7, SimButtonWire::VolumeDown)] {
        let req = Request::Simulator(SimRequestWire { worktree: None, cmd: SimCmdWire::Button(button) });
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(*bytes.last().unwrap(), ordinal, "{button:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
    }
    for (ordinal, cmd) in cmds.into_iter().enumerate() {
        let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: cmd.clone() });
        let bytes = req.to_bytes().expect("encode");
        // After the envelope ordinal and the `Some("/w")` worktree (tag,
        // length, two bytes), the verb's own ordinal.
        assert_eq!(bytes[5] as usize, ordinal, "{cmd:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
    }

    let device = SimDeviceWire {
        udid: "81CE1BE8-0000-0000-0000-000000000000".into(),
        name: "iPhone 17 Pro".into(),
        runtime: "iOS 26.0".into(),
        state: "Booted".into(),
    };
    let replies = [
        Ok(SimReplyWire::Status(SimStatusWire {
            available: true,
            reason: None,
            xcode: Some("26.0".into()),
            worktree: "/w".into(),
            device: Some(device.clone()),
            streaming: true,
            consent: SimConsentWire::Denied { retry_after_secs: 540 },
            agent_control: true,
        })),
        Ok(SimReplyWire::Devices(vec![device.clone()])),
        Ok(SimReplyWire::Attached(device)),
        Ok(SimReplyWire::Screenshot { png: vec![0x89, b'P', b'N', b'G'], width: 402, height: 874, scale: 1.0 }),
        Ok(SimReplyWire::Ax(vec![SimAxNodeWire {
            depth: 1,
            role: "Button".into(),
            label: Some("Settings".into()),
            identifier: None,
            value: None,
            enabled: true,
            frame: [0.0, 1.5, 100.0, 44.0],
        }])),
        Err(SimErrorWire::ConsentPending),
        Err(SimErrorWire::ConsentDenied { retry_after_secs: 600 }),
        Err(SimErrorWire::PathOutsideWorktree),
        Err(SimErrorWire::Refused("booted by the user".into())),
    ];
    for reply in replies {
        let resp = Response::Simulator(reply);
        assert_eq!(Response::from_bytes(&resp.to_bytes().expect("encode")).expect("decode"), resp);
    }
}

/// v28 appends device state after the verify loop (verb 21), each change in
/// its own append-only enum.
#[test]
fn device_state_appends_and_round_trips() {
    let state = |s| SimCmdWire::DeviceState(s);
    let cmds = [
        (0u8, state(SimStateCmdWire::Location(SimLocationWire::Set { lat: 10.77, lon: 106.7 }))),
        (0, state(SimStateCmdWire::Location(SimLocationWire::Clear))),
        (1, state(SimStateCmdWire::Appearance { dark: true })),
        (2, state(SimStateCmdWire::Privacy { action: SimPrivacyActionWire::Reset, service: "photos".into(), app: "a.b".into() })),
        (3, state(SimStateCmdWire::Push { app: "a.b".into(), payload: r#"{"aps":{}}"#.into() })),
        (4, state(SimStateCmdWire::StatusBar { clean: false })),
        (5, state(SimStateCmdWire::Erase { force: true })),
        (6, state(SimStateCmdWire::Net(SimNetWire::Speed("lte".into())))),
    ];
    for (ordinal, cmd) in cmds {
        let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: cmd.clone() });
        let bytes = req.to_bytes().expect("encode");
        assert_eq!((bytes[5], bytes[6]), (21, ordinal), "{cmd:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
        assert!(!cmd.uses_verify(), "not a verify-loop verb: never answered with its errors");
    }
}

/// v29 appends diagnostics after device state (verbs 22 and 23, replies 9
/// and 10).
#[test]
fn diagnostics_append_and_round_trip() {
    let logs = SimCmdWire::Logs(SimLogsWire { since_secs: 30, app: Some("a.b".into()), grep: Some("boom".into()), level: SimLogLevelWire::Error, lines: 200 });
    let crashes = SimCmdWire::Crashes(SimCrashesWire { since_secs: 600, app: None });
    for (ordinal, cmd) in [(22u8, logs), (23, crashes)] {
        let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: cmd.clone() });
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(bytes[5], ordinal, "{cmd:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
        assert!(!cmd.uses_verify() && cmd.is_control(), "a control verb, never answered with the v27 errors");
    }
    let crash = SimCrashWire {
        id: "B8E1C770".into(),
        time: "2026-10-12 15:54:29".into(),
        app: "a.b".into(),
        exception: "EXC_BREAKPOINT (SIGTRAP)".into(),
        reason: "Fatal error: Index out of range".into(),
        frames: vec!["libswiftCore.dylib _assertionFailure".into()],
    };
    let replies = [
        (9u8, SimReplyWire::Logs { lines: vec!["one".into(), "two".into()], truncated: true }),
        (10, SimReplyWire::Crashes(SimCrashListWire { crashes: vec![crash], warnings: vec!["ANR in a.b".into()] })),
    ];
    for (ordinal, reply) in replies {
        let resp = Response::Simulator(Ok(reply.clone()));
        let bytes = resp.to_bytes().expect("encode");
        assert_eq!(Response::from_bytes(&bytes).expect("decode"), resp);
        // The reply's ordinal follows the envelope's and `Ok`'s.
        assert_eq!(bytes[2], ordinal, "{reply:?}");
    }
}

/// v32 appends React Native after exec (verb 26, reply 13).
#[test]
fn react_native_appends_and_round_trips() {
    let cmds = [
        SimCmdWire::Rn(SimRnCmdWire::Network { seconds: 10, grep: Some("/api".into()), bodies: true, app: Some("a.b".into()) }),
        SimCmdWire::Rn(SimRnCmdWire::Profile { seconds: 5, top: 20, app: None }),
    ];
    for cmd in cmds {
        let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: cmd.clone() });
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(bytes[5], 26, "{cmd:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
        assert!(cmd.uses_verify() && cmd.is_control(), "a control verb from a client that knows every error");
    }
    let request = SimRnRequestWire {
        method: "POST".into(),
        url: "https://api.example.com/login?token=[redacted]".into(),
        status: Some(200),
        ms: Some(120),
        size: Some(512),
        mime: Some("application/json".into()),
        failed: None,
        request_headers: vec![("Authorization".into(), "[redacted]".into())],
        response_headers: Vec::new(),
        request_body: Some(r#"{"password":"[redacted]"}"#.into()),
        response_body: None,
    };
    let function = SimRnFunctionWire { name: "render".into(), url: "http://localhost:8081/index.bundle".into(), line: 12, self_ms: 40.5 };
    let replies = [
        SimReplyWire::Rn(SimRnWire::Network(SimRnNetworkWire { app: "a.b".into(), requests: vec![request], truncated: false, warnings: Vec::new() })),
        SimReplyWire::Rn(SimRnWire::Profile(SimRnProfileWire { app: "a.b".into(), duration_ms: 5000.0, sampled_ms: 4990.0, functions: vec![function], warnings: vec!["devtools".into()] })),
    ];
    for reply in replies {
        let resp = Response::Simulator(Ok(reply.clone()));
        let bytes = resp.to_bytes().expect("encode");
        assert_eq!(bytes[2], 13, "{reply:?}");
        assert_eq!(Response::from_bytes(&bytes).expect("decode"), resp);
    }
}

/// v27 appends the verify loop after every older variant: the verbs from 15,
/// `Ref` after `Id`, the replies after `Ax`, the errors after `Refused`.
#[test]
fn the_verify_loop_appends_and_round_trips() {
    let tap = Box::new(SimCmdWire::Tap(SimTargetWire::Ref(7)));
    let cmds = [
        (15usize, SimCmdWire::Settled { action: tap.clone(), timeout_ms: 3000 }),
        (16, SimCmdWire::AxRefs { max: 100 }),
        (17, SimCmdWire::Wait { target: SimTargetWire::Label("Home".into()), gone: false, timeout_ms: 5000 }),
        (18, SimCmdWire::Assert { target: SimTargetWire::Id("title".into()), check: SimCheckWire::Text("Hi".into()) }),
        (19, SimCmdWire::AsCaller { caller: u64::MAX, cmd: Box::new(SimCmdWire::Settled { action: tap, timeout_ms: 0 }) }),
        (20, SimCmdWire::TypeInto { target: SimTargetWire::Ref(3), text: "a@b.co".into(), paste: false }),
    ];
    for (ordinal, cmd) in cmds {
        let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: cmd.clone() });
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(bytes[5] as usize, ordinal, "{cmd:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
    }
    // `Tap(Ref(7))`: the verb (6), the target (3), the ref.
    let req = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: SimCmdWire::Tap(SimTargetWire::Ref(7)) });
    assert_eq!(&req.to_bytes().expect("encode")[5..], [6, 3, 7]);

    let node = SimAxNodeWire {
        depth: 0,
        role: "TextField".into(),
        label: Some("Email".into()),
        identifier: None,
        value: None,
        enabled: true,
        frame: [16.0, 300.0, 370.0, 44.0],
    };
    let settled = SimSettleWire {
        outcome: Some(SimSettleOutcomeWire::SettledFrame),
        elapsed_ms: 900,
        diff: vec![SimDiffLineWire { op: SimDiffOpWire::Added, r: Some(31), node: node.clone() }],
        unchanged: 12,
        warnings: vec!["moved".into()],
        target: Some(node.clone()),
        at: Some(SimPointWire { x: 201.0, y: 322.0 }),
    };
    let replies = [
        (6u8, Ok(SimReplyWire::Settled(settled))),
        (7, Ok(SimReplyWire::AxRefs(vec![SimRefNodeWire { r: 1, node: node.clone() }]))),
        (8, Ok(SimReplyWire::Matched { node: Some(node), elapsed_ms: 250 })),
        (11, Err(SimErrorWire::StaleRef("stale".into()))),
        (12, Err(SimErrorWire::AssertFailed("found nothing".into()))),
        (13, Err(SimErrorWire::TimedOut("5 s".into()))),
    ];
    for (ordinal, reply) in replies {
        let resp = Response::Simulator(reply);
        let bytes = resp.to_bytes().expect("encode");
        // The envelope, then `Ok`/`Err`, then the variant's ordinal.
        assert_eq!(bytes[2], ordinal, "{resp:?}");
        assert_eq!(Response::from_bytes(&bytes).expect("decode"), resp);
    }
}

/// Nested envelopes are bounded on decode: a frame of a hundred thousand
/// `AsCaller`s is refused, not a stack overflow before authentication — while
/// the deepest legal shape (caller → settle → verb) still decodes.
#[test]
fn nested_verbs_are_bounded_on_decode() {
    let legal = Request::Simulator(SimRequestWire {
        worktree: None,
        cmd: SimCmdWire::AsCaller {
            caller: 1,
            cmd: Box::new(SimCmdWire::Settled { action: Box::new(SimCmdWire::Detach), timeout_ms: 0 }),
        },
    });
    assert_eq!(Request::from_bytes(&legal.to_bytes().unwrap()).unwrap(), legal);
    let three = SimCmdWire::AsCaller { caller: 1, cmd: Box::new(SimCmdWire::AsCaller { caller: 2, cmd: Box::new(SimCmdWire::AsCaller { caller: 3, cmd: Box::new(SimCmdWire::Detach) }) }) };
    let bytes = Request::Simulator(SimRequestWire { worktree: None, cmd: three }).to_bytes().unwrap();
    assert!(Request::from_bytes(&bytes).is_err(), "three deep");
    // The envelope, `None`, then 100k × (AsCaller = 19, caller 0), then Detach.
    let mut hostile = Request::Simulator(SimRequestWire { worktree: None, cmd: SimCmdWire::Detach }).to_bytes().unwrap();
    let tail = hostile.split_off(2);
    for _ in 0..100_000 {
        hostile.extend_from_slice(&[19, 0]);
    }
    hostile.extend_from_slice(&tail);
    assert!(Request::from_bytes(&hostile).is_err());
    // The counter unwinds on failure: a legal frame still decodes after.
    assert_eq!(Request::from_bytes(&legal.to_bytes().unwrap()).unwrap(), legal);
}

/// A v26 client cannot decode the v27 errors, so a verb it could have sent
/// never gets one: it hears `Failed`, with the same words.
#[test]
fn only_v27_verbs_get_the_v27_errors() {
    let old = [
        SimCmdWire::Tap(SimTargetWire::Label("OK".into())),
        SimCmdWire::Ax { max: 10 },
        SimCmdWire::Type { text: "a".into(), paste: false },
    ];
    for cmd in &old {
        assert!(!cmd.uses_verify(), "{cmd:?}");
        for err in [SimErrorWire::StaleRef("a".into()), SimErrorWire::AssertFailed("b".into()), SimErrorWire::TimedOut("c".into())] {
            let said = err.to_string();
            assert_eq!(err.for_verb(cmd), SimErrorWire::Failed(said));
        }
        assert_eq!(SimErrorWire::NoDevice.for_verb(cmd), SimErrorWire::NoDevice);
    }
    let new = SimCmdWire::Tap(SimTargetWire::Ref(1));
    assert_eq!(SimErrorWire::StaleRef("x".into()).for_verb(&new), SimErrorWire::StaleRef("x".into()));
    let wait = SimCmdWire::Wait { target: SimTargetWire::Id("a".into()), gone: true, timeout_ms: 1 };
    assert_eq!(SimErrorWire::TimedOut("x".into()).for_verb(&wait), SimErrorWire::TimedOut("x".into()));
}

/// The v10 schedule shape is frozen, byte for byte.
///
/// The twin of `the_v18_team_shape_is_frozen`. It catches a field inserted or
/// appended anywhere in `ScheduleWire`, and a `RecurrenceWire` variant
/// reordered — the mistakes that silently shift every later byte.
///
/// It deliberately does **not** catch a variant *appended* to
/// `RecurrenceWire`, and cannot: postcard leaves the existing ordinals where
/// they are, so an encoder emitting `WeeklyAt` produces identical bytes either
/// way. That danger lives on the decode side and is guarded separately by
/// [`a_recurrence_wire_variant_may_never_be_appended`] and
/// [`a_v10_decoder_cannot_read_a_cron_recurrence`].
#[test]
fn the_v10_schedule_shape_is_frozen() {
    let wire = ScheduleWire {
        id: "s".into(),
        name: "n".into(),
        cwd: "/".into(),
        prompt: "p".into(),
        agent_id: None,
        recurrence: RecurrenceWire::WeeklyAt { weekday: 4, hour: 17, minute: 15 },
        enabled: true,
        next_fire_at: "t".into(),
        summary: "u".into(),
    };
    let bytes = postcard::to_allocvec(&wire).expect("encode");
    assert_eq!(
        bytes,
        vec![
            1, b's', // id
            1, b'n', // name
            1, b'/', // cwd
            1, b'p', // prompt
            0,       // agent_id: None
            2, 4, 17, 15, // recurrence: WeeklyAt (variant 2) + weekday/hour/minute
            1,       // enabled: true
            1, b't', // next_fire_at
            1, b'u', // summary
        ],
        "a v10 phone decodes these exact bytes; nothing may be inserted or appended"
    );

    // And the third variant is still ordinal 2 — a fourth appended here would
    // not move it, which is precisely why the danger is invisible without this.
    assert_eq!(
        postcard::to_allocvec(&RecurrenceWire::WeeklyAt { weekday: 0, hour: 0, minute: 0 })
            .expect("encode"),
        vec![2, 0, 0, 0]
    );
}

/// The reason both team shapes exist: a v18 `TeamRunWire` frame must decode
/// identically under this build, byte for byte, as the shape it always was.
///
/// If someone "simplifies" this later by adding `agent_id` to `TeamRoleWire`
/// instead, this fails — which is the entire point. Postcard encodes struct
/// fields positionally, so the appended field would make a stale CLI misparse
/// every role after the first and drop the connection.
#[test]
fn the_v18_team_shape_is_frozen() {
    let v18 = Response::TeamRun(TeamRunWire {
        id: "run-1".into(),
        name: "sweep".into(),
        cwd: "/w".into(),
        created_at: "t".into(),
        closed: false,
        roles: vec![TeamRoleWire {
            name: "impl".into(),
            session_id: None,
            status: TeamRoleStatusWire::Running,
            summary: None,
            updated_at: "t".into(),
        }],
    });
    let bytes = v18.to_bytes().expect("encode");
    // Hand-computed from the v18 field order: ordinal 40, then id/name/cwd/
    // created_at as length-prefixed strings, `closed` as 0, one role, and that
    // role's five fields. A change to any field's position or presence moves
    // these bytes.
    assert_eq!(
        bytes,
        vec![
            40, // Response::TeamRun
            5, b'r', b'u', b'n', b'-', b'1', //
            5, b's', b'w', b'e', b'e', b'p', //
            2, b'/', b'w', //
            1, b't',  //
            0,    // closed
            1,    // one role
            4, b'i', b'm', b'p', b'l', //
            0, // session_id: None
            0, // status: Running
            0, // summary: None
            1, b't', //
        ],
        "the v18 team reply is frozen — a field added to it strands every stale CLI"
    );
    assert_eq!(Response::from_bytes(&bytes).expect("decode"), v18);
}

/// **A fourth `RecurrenceWire` variant must not compile.**
///
/// The obvious "simplification" of this phase is to append `Cron` here instead
/// of adding v23 verbs, and it is invisible to every byte-level test: postcard
/// keeps the existing ordinals, so nothing an encoder produces changes. The
/// breakage is on the far side — see
/// [`a_v10_decoder_cannot_read_a_cron_recurrence`] — and by the time a phone
/// hits it there is no test left to run.
///
/// So the guard is the compiler. Adding a variant makes this match
/// non-exhaustive, and the build stops with this comment attached to it.
#[test]
fn a_recurrence_wire_variant_may_never_be_appended() {
    fn exhaustive(w: &RecurrenceWire) -> &'static str {
        match w {
            RecurrenceWire::EveryMinutes { .. } => "interval",
            RecurrenceWire::DailyAt { .. } => "daily",
            RecurrenceWire::WeeklyAt { .. } => "weekly",
        }
    }
    assert_eq!(exhaustive(&RecurrenceWire::EveryMinutes { minutes: 5 }), "interval");
    assert_eq!(exhaustive(&RecurrenceWire::DailyAt { hour: 9, minute: 0 }), "daily");
    assert_eq!(
        exhaustive(&RecurrenceWire::WeeklyAt { weekday: 0, hour: 9, minute: 0 }),
        "weekly"
    );
}

/// The reason v23 needed new verbs, stated as an executable fact.
///
/// `OldRecurrence` is a stand-in for the enum a paired phone was built with:
/// exactly three variants. A cron recurrence encodes as ordinal 3, and that
/// decoder rejects the frame — not the one field, the **frame**. Since
/// `RecurrenceWire` rides inside `Vec<ScheduleWire>`, one cron schedule would
/// have taken every other row down with it.
#[test]
fn a_v10_decoder_cannot_read_a_cron_recurrence() {
    #[derive(Debug, Deserialize)]
    enum OldRecurrence {
        #[allow(dead_code)]
        EveryMinutes { minutes: u32 },
        #[allow(dead_code)]
        DailyAt { hour: u8, minute: u8 },
        #[allow(dead_code)]
        WeeklyAt { weekday: u8, hour: u8, minute: u8 },
    }

    // What a v23 host would put on the wire if cron rode the old enum.
    let cron = postcard::to_allocvec(&RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() })
        .expect("encode");
    assert_eq!(cron[0], 3, "cron would be the fourth ordinal");
    assert!(
        postcard::from_bytes::<OldRecurrence>(&cron).is_err(),
        "a three-variant decoder must reject ordinal 3 — this is the whole reason for v23"
    );

    // And the three it does know still decode, so the rejection is about the
    // new ordinal rather than the stand-in decoder being broken.
    for known in [
        RecurrenceV2Wire::EveryMinutes { minutes: 5 },
        RecurrenceV2Wire::DailyAt { hour: 9, minute: 0 },
        RecurrenceV2Wire::WeeklyAt { weekday: 4, hour: 17, minute: 15 },
    ] {
        let bytes = postcard::to_allocvec(&known).expect("encode");
        assert!(
            postcard::from_bytes::<OldRecurrence>(&bytes).is_ok(),
            "{known:?} is one the old decoder knows"
        );
    }
}


/// v34: the phone's view RPCs append after the simulator's (requests 70–72,
/// replies 53–54) and survive the codec.
#[test]
fn sim_view_rpcs_append_and_round_trip() {
    let token = [9u8; 16];
    let reqs = [
        (Request::SimViewables, 70),
        (Request::SimSubscribe { device: "E0E0".into() }, 71),
        (Request::SimUnsubscribe { token }, 72),
    ];
    for (req, ordinal) in reqs {
        let bytes = req.to_bytes().expect("encode");
        assert_eq!(bytes[0], ordinal, "{req:?}");
        assert_eq!(Request::from_bytes(&bytes).expect("decode"), req);
    }
    let row = SimViewableWire {
        device: "E0E0".into(),
        name: "iPhone 17".into(),
        worktree: "oximux · main".into(),
        class: SimViewClassWire::AndroidPhone,
        streaming: true,
    };
    let subscribed = SimSubscribedWire { token, width: 590, height: 1278 };
    let resps = [
        (Response::SimViewables(Ok(vec![row])), 53),
        (Response::SimViewables(Err(SimErrorWire::Refused("off".into()))), 53),
        (Response::SimSubscribed(Ok(subscribed)), 54),
        (Response::SimSubscribed(Err(SimErrorWire::ConsentPending)), 54),
    ];
    for (resp, ordinal) in resps {
        let bytes = resp.to_bytes().expect("encode");
        assert_eq!(bytes[0], ordinal, "{resp:?}");
        assert_eq!(Response::from_bytes(&bytes).expect("decode"), resp);
    }
}
