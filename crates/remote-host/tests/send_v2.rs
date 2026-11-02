//! `SendV2` (v37) over the in-memory loopback: a key sends once, a repeat
//! sends nothing and answers where the first send stands.

use std::sync::Arc;

use futures::future::join;
use oximux_agents::lineage::LineageStore;
use oximux_agents::send_receipts::{KeyKind, ReceiptState, key_hash};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, HostConfig, HostCore, HostCoreDeps, LocalScope, PriorOwner};
use oximux_remote_proto::Transport;
use oximux_remote_proto::delivery::{ReceiptStateWire, SendOutcomeWire, SendV2Req};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn send(key: &str, text: &str) -> Request {
    Request::SendV2(SendV2Req {
        session_id: "s1".into(),
        text: text.into(),
        images: vec![],
        idempotency_key: key.into(),
        corr_id: 1,
    })
}

struct Host {
    dispatcher: Dispatcher,
    core: Option<Arc<HostCore>>,
    stub: StubConnection,
}

fn host(with_core: bool) -> Host {
    let registry = Arc::new(SessionRegistry::new());
    let stub = StubConnection::default();
    registry.register("s1".into(), Arc::new(stub.clone()));
    let mut dispatcher = Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()));
    let core = with_core.then(|| {
        let db = oximux_storage::db::open_memory().unwrap();
        let core = HostCore::new(HostCoreDeps {
            rt: tokio::runtime::Handle::current(),
            lineage: LineageStore::new(db.conn()),
            registry,
            config: HostConfig::default(),
        });
        core.start(PriorOwner::None);
        core
    });
    if let Some(core) = &core {
        dispatcher = dispatcher.with_host_core(core.clone());
    }
    Host { dispatcher, core, stub }
}

async fn as_scope<F, Fut>(host: &Host, scope: LocalScope, script: F)
where
    F: FnOnce(Arc<dyn Transport>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (client, server) = duplex_pair();
    let client: Arc<dyn Transport> = Arc::new(client);
    let serve = host.dispatcher.serve_local(&server, scope);
    let run = script(client.clone());
    join(serve, async move {
        run.await;
        drop(client);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_sends_once_and_a_repeat_reports_the_first_sends_state() {
    let h = host(true);
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(call(&*c, send("k-1", "run the tests")).await, Response::SendOutcome(SendOutcomeWire::Queued));
        assert_eq!(
            call(&*c, send("k-1", "run the tests")).await,
            Response::SendOutcome(SendOutcomeWire::Duplicate(ReceiptStateWire::Pending)),
            "not sent again, and nothing has started on it yet"
        );
    })
    .await;
    let sent = h.stub.sent();
    assert_eq!(sent.len(), 1, "one line written: {sent:?}");

    // The agent starts on it: whoever drives the session records that under
    // the line's uuid, and a repeat of the key now says so.
    let uuid = sent[0]["uuid"].as_str().expect("the line is tagged").to_string();
    let core = h.core.clone().unwrap();
    core.lineage()
        .receipts()
        .settle("s1", &key_hash(KeyKind::Gate, &uuid), ReceiptState::Accepted, core.instance(), chrono::Local::now())
        .unwrap();
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(
            call(&*c, send("k-1", "run the tests")).await,
            Response::SendOutcome(SendOutcomeWire::Duplicate(ReceiptStateWire::Accepted))
        );
        assert_eq!(call(&*c, send("k-2", "then lint")).await, Response::SendOutcome(SendOutcomeWire::Queued));
    })
    .await;
    let sent = h.stub.sent();
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[1]["uuid"], sent[0]["uuid"], "a new key is a new send");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_key_or_an_empty_prompt_is_refused_before_anything_is_recorded() {
    let h = host(true);
    as_scope(&h, LocalScope::Full, |c| async move {
        assert!(matches!(call(&*c, send("", "x")).await, Response::Error(RpcError::BadRequest(_))));
        assert!(matches!(call(&*c, send(&"k".repeat(129), "x")).await, Response::Error(RpcError::BadRequest(_))));
        assert!(matches!(call(&*c, send("k-1", "  ")).await, Response::Error(RpcError::BadRequest(_))));
        // The refused empty prompt did not burn its key.
        assert_eq!(call(&*c, send("k-1", "now with text")).await, Response::SendOutcome(SendOutcomeWire::Queued));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_is_refused_and_a_host_without_receipts_says_unsupported() {
    let h = host(true);
    as_scope(&h, LocalScope::Session("other".into()), |c| async move {
        assert_eq!(call(&*c, send("k-1", "x")).await, Response::Error(RpcError::Unauthorized));
    })
    .await;
    assert!(h.stub.sent().is_empty());

    let h = host(false);
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(call(&*c, send("k-1", "x")).await, Response::Error(RpcError::Unsupported));
    })
    .await;
    assert!(h.stub.sent().is_empty(), "nothing sent without a promise to keep");
}
