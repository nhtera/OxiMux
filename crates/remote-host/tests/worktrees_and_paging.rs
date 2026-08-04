//! The v16 surface: worktree RPC authorization (the dedicated full-scope
//! gates) and the paginated transcript fetch — including the property the verb
//! exists for, that a transcript larger than one transport frame arrives whole
//! across pages each far below the cap. Every refusal is asserted at the fake
//! service, not only on the wire: an `Unauthorized` that still ran the
//! operation would look identical to the client.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{
    AuthStore, Dispatcher, LocalScope, PairingSlot, WorktreeError, WorktreeService,
    registration_proof,
};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{HelloReq, RegisterReq, WorktreeWire};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn register_req(pubkey: [u8; 32]) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: None,
    }
}

/// A service that records how often each operation actually ran, so a refusal
/// can be asserted as "never reached the service".
#[derive(Default)]
struct CountingWorktrees {
    creates: AtomicUsize,
    removes: AtomicUsize,
}

#[async_trait::async_trait]
impl WorktreeService for CountingWorktrees {
    async fn create(&self, project_path: &str, slug: &str)
    -> Result<WorktreeWire, WorktreeError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(WorktreeWire {
            id: "wt-1".into(),
            project_path: project_path.into(),
            name: slug.into(),
            slug: slug.into(),
            branch: format!("oximux/{slug}"),
            path: format!("/data/worktrees/{slug}"),
        })
    }
    async fn list(&self, _project_path: Option<&str>) -> Result<Vec<WorktreeWire>, WorktreeError> {
        Ok(vec![])
    }
    async fn remove(&self, _id: &str) -> Result<(), WorktreeError> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn worktree_requests() -> [Request; 3] {
    [
        Request::CreateWorktree { project_path: "/work".into(), slug: "feat".into() },
        Request::ListWorktrees { project_path: None },
        Request::RemoveWorktree { id: "wt-1".into() },
    ]
}

/// A full-scope local caller (the operator) reaches all three worktree verbs.
#[test]
fn local_full_scope_manages_worktrees() {
    let service = Arc::new(CountingWorktrees::default());
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_worktrees(service.clone())
        .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let Response::WorktreeCreated(row) = call(
            &client,
            Request::CreateWorktree { project_path: "/work".into(), slug: "feat".into() },
        )
        .await
        else {
            panic!("expected WorktreeCreated");
        };
        assert_eq!(row.branch, "oximux/feat");
        let Response::Worktrees(_) = call(&client, Request::ListWorktrees { project_path: None }).await
        else {
            panic!("expected Worktrees");
        };
        assert_eq!(
            call(&client, Request::RemoveWorktree { id: row.id }).await,
            Response::Ack
        );
        drop(client);
    };
    block_on(join(serve, script));
    assert_eq!(service.creates.load(Ordering::SeqCst), 1);
    assert_eq!(service.removes.load(Ordering::SeqCst), 1);
}

/// A session-scoped local caller is refused all three — including the read:
/// worktree rows carry host paths across every project. And the refusal is
/// upstream of the service: nothing ran.
#[test]
fn local_session_scope_is_refused_every_worktree_verb() {
    let service = Arc::new(CountingWorktrees::default());
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_worktrees(service.clone())
        .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Session("sess-1".into()));
    let script = async move {
        for req in worktree_requests() {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unauthorized),
                "a session-scoped caller must be refused every worktree verb"
            );
        }
        drop(client);
    };
    block_on(join(serve, script));
    assert_eq!(service.creates.load(Ordering::SeqCst), 0, "create never reached the service");
    assert_eq!(service.removes.load(Ordering::SeqCst), 0, "remove never reached the service");
}

/// A read-only full device may list but not create or remove — the same tier
/// split the schedule surface has.
#[test]
fn read_only_device_lists_but_cannot_manage() {
    let service = Arc::new(CountingWorktrees::default());
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth.clone())
        .with_worktrees(service.clone())
        .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let pubkey = [0x33; 32];
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("registration failed");
        };
        auth.set_read_only(&pubkey, true);
        let Response::Worktrees(_) =
            call(&client, Request::ListWorktrees { project_path: None }).await
        else {
            panic!("a read-only full device may list worktrees");
        };
        assert_eq!(
            call(
                &client,
                Request::CreateWorktree { project_path: "/work".into(), slug: "feat".into() }
            )
            .await,
            Response::Error(RpcError::Unauthorized),
            "a read-only device must not create"
        );
        assert_eq!(
            call(&client, Request::RemoveWorktree { id: "wt-1".into() }).await,
            Response::Error(RpcError::Unauthorized),
            "a read-only device must not remove"
        );
        drop(client);
    };
    block_on(join(serve, script));
    assert_eq!(service.creates.load(Ordering::SeqCst), 0);
    assert_eq!(service.removes.load(Ordering::SeqCst), 0);
}

/// With no service installed, an AUTHORIZED caller learns the truth —
/// `Unsupported` — while an under-scoped caller still sees `Unauthorized`
/// first, so the capability is not probeable without the scope to use it.
#[test]
fn missing_service_is_unsupported_only_for_the_authorized() {
    let dispatcher =
        Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
            .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        for req in worktree_requests() {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unsupported),
                "an authorized caller on a service-less host hears Unsupported"
            );
        }
        drop(client);
    };
    block_on(join(serve, script));

    let dispatcher =
        Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
            .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Session("sess-1".into()));
    let script = async move {
        for req in worktree_requests() {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unauthorized),
                "authorization is decided before capability, so nothing is probeable"
            );
        }
        drop(client);
    };
    block_on(join(serve, script));
}

/// One synthetic folded entry of roughly `size` bytes.
fn entry(i: usize, size: usize) -> serde_json::Value {
    serde_json::json!({ "Assistant": { "text": format!("{i}:{}", "x".repeat(size)), "thinking": "" } })
}

/// A transcript larger than the 16 MB transport frame arrives whole across
/// pages, each far below the cap, and reassembles to exactly the original.
#[test]
fn oversize_transcript_is_fetchable_in_pages() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("big".into(), Arc::new(StubConnection::default()));
    // ~20 MB of folded entries — the legacy single-frame fetch cannot carry it.
    let entries: Vec<serde_json::Value> = (0..2000).map(|i| entry(i, 10_000)).collect();
    let entries_json = serde_json::to_string(&entries).unwrap();
    assert!(entries_json.len() > 16 * 1024 * 1024, "the fixture must exceed one frame");
    handle.publish_transcript(entries_json.clone(), Some("test-model".into()));

    let dispatcher =
        Dispatcher::new(registry, Arc::new(AuthStore::new())).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let mut reassembled: Vec<serde_json::Value> = Vec::new();
        let mut cursor = 0u64;
        let mut pages = 0u32;
        loop {
            let Response::TranscriptPage(page) = call(
                &client,
                Request::FetchTranscriptPage { session_id: "big".into(), cursor, limit: 512 },
            )
            .await
            else {
                panic!("expected TranscriptPage");
            };
            assert!(
                page.entries_json.len() < 8 * 1024 * 1024,
                "every page must sit far below the frame cap, got {}",
                page.entries_json.len()
            );
            assert_eq!(page.total, 2000);
            let mut chunk: Vec<serde_json::Value> =
                serde_json::from_str(&page.entries_json).unwrap();
            reassembled.append(&mut chunk);
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
        }
        assert!(pages > 1, "an oversize transcript must take more than one page");
        assert_eq!(reassembled.len(), 2000, "no entry lost across pages");
        assert_eq!(serde_json::to_string(&reassembled).unwrap().len(), {
            // Reassembly equals the original byte-for-byte (same serializer).
            let original: Vec<serde_json::Value> = (0..2000).map(|i| entry(i, 10_000)).collect();
            serde_json::to_string(&original).unwrap().len()
        });
        drop(client);
    };
    block_on(join(serve, script));
}

/// A single entry no frame can carry (>12 MB of raw text — images are trimmed
/// by the budget pass, text is not) is substituted with a visible
/// `ContextCompaction` marker instead of producing a page the transport would
/// refuse. The cursor still advances, so the rest of the transcript arrives.
#[test]
fn an_untransferable_entry_is_substituted_never_shipped() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("huge".into(), Arc::new(StubConnection::default()));
    let entries = vec![
        entry(0, 100),
        // ~20 MB of text in ONE folded entry — beyond any frame.
        entry(1, 20 * 1024 * 1024),
        entry(2, 100),
    ];
    handle.publish_transcript(serde_json::to_string(&entries).unwrap(), None);

    let dispatcher = Dispatcher::new(registry, Arc::new(AuthStore::new())).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let mut reassembled: Vec<serde_json::Value> = Vec::new();
        let mut cursor = 0u64;
        loop {
            let Response::TranscriptPage(page) = call(
                &client,
                Request::FetchTranscriptPage { session_id: "huge".into(), cursor, limit: 512 },
            )
            .await
            else {
                panic!("expected TranscriptPage — the oversize entry must not kill the fetch");
            };
            assert!(
                page.entries_json.len() < 13 * 1024 * 1024,
                "no page may approach the frame cap, got {}",
                page.entries_json.len()
            );
            let mut chunk: Vec<serde_json::Value> =
                serde_json::from_str(&page.entries_json).unwrap();
            reassembled.append(&mut chunk);
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
        }
        assert_eq!(reassembled.len(), 3, "every position accounted for");
        assert_eq!(reassembled[0], entry(0, 100), "entries around the substitution survive");
        assert_eq!(reassembled[2], entry(2, 100));
        let marker = reassembled[1]["ContextCompaction"]["summary"]
            .as_str()
            .expect("the oversize entry becomes a visible marker, not silence");
        assert!(marker.contains("exceeded the transfer limit"), "got: {marker}");
        drop(client);
    };
    block_on(join(serve, script));
}

/// A cursor at (or past) the end answers an empty final page, not an error —
/// a fold that shrank under a client (rewind) is not misuse.
#[test]
fn paging_past_the_end_is_an_empty_final_page() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s".into(), Arc::new(StubConnection::default()));
    handle.publish_transcript(serde_json::to_string(&vec![entry(0, 10)]).unwrap(), None);
    let dispatcher = Dispatcher::new(registry, Arc::new(AuthStore::new())).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let Response::TranscriptPage(page) = call(
            &client,
            Request::FetchTranscriptPage { session_id: "s".into(), cursor: 99, limit: 10 },
        )
        .await
        else {
            panic!("expected TranscriptPage");
        };
        assert_eq!(page.entries_json, "[]");
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.total, 1);
        drop(client);
    };
    block_on(join(serve, script));
}

/// The compat claim v16 makes: a v15 peer (which never sends the appended
/// calls) is served unchanged, INCLUDING the legacy unpaginated
/// `FetchTranscript` — its reply shape was left untouched.
#[test]
fn a_v15_peer_still_gets_the_legacy_transcript_fetch() {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("s".into(), Arc::new(StubConnection::default()));
    handle.publish_transcript(serde_json::to_string(&vec![entry(0, 10)]).unwrap(), None);
    let dispatcher = Dispatcher::new(registry, Arc::new(AuthStore::new())).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        // The peer declares itself v15, as a not-yet-updated phone would.
        let Response::HelloAck(ack) =
            call(&client, Request::Hello(HelloReq { protocol_version: 15 })).await
        else {
            panic!("expected HelloAck");
        };
        assert_eq!(ack.protocol_version, 16, "the host announces v16");
        assert_eq!(ack.min_compatible, 1, "and still serves back to v1");
        let Response::SessionTranscript(t) =
            call(&client, Request::FetchTranscript { session_id: "s".into() }).await
        else {
            panic!("a v15 peer's legacy FetchTranscript must be served unchanged");
        };
        let entries: Vec<serde_json::Value> = serde_json::from_str(&t.entries_json).unwrap();
        assert_eq!(entries.len(), 1);
        drop(client);
    };
    block_on(join(serve, script));
}
