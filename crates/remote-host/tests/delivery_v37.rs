//! v37 over the in-memory loopback: `SendV2` sends a key once (a repeat sends
//! nothing and answers where the first send stands), and a subscriber that
//! can decode it is pushed the session's health.

use std::sync::Arc;

use futures::future::join;
use oximux_agents::lineage::LineageStore;
use oximux_agents::send_receipts::{KeyKind, ReceiptState, key_hash};
use oximux_agents::health::{ExitProof, HealthInputs, Process};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{StubConnection, ThreadEvent};
use oximux_remote_host::{AuthStore, Dispatcher, HostConfig, HostCore, HostCoreDeps, LocalScope, PriorOwner};
use oximux_remote_proto::Transport;
use oximux_remote_proto::delivery::{ReceiptStateWire, SendOutcomeWire, SendV2Req};
use oximux_remote_proto::messages::HelloReq;
use oximux_remote_proto::proto::{PROTOCOL_VERSION, Request, Response, RpcError};
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
    registry: Arc<SessionRegistry>,
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
            registry: registry.clone(),
            config: HostConfig::default(),
        });
        core.start(PriorOwner::None);
        core
    });
    if let Some(core) = &core {
        dispatcher = dispatcher.with_host_core(core.clone());
    }
    Host { dispatcher, registry, core, stub }
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

/// Refused, with the host's own retry armed for it: to a caller repeating
/// the key it is still on its way, so it is not sent again by hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_send_the_host_will_retry_reads_pending() {
    let h = host(true);
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(call(&*c, send("k-1", "run the tests")).await, Response::SendOutcome(SendOutcomeWire::Queued));
    })
    .await;
    let uuid = h.stub.sent()[0]["uuid"].as_str().unwrap().to_string();
    let core = h.core.clone().unwrap();
    let (line, now) = (key_hash(KeyKind::Gate, &uuid), chrono::Local::now());
    core.lineage().receipts().settle("s1", &line, ReceiptState::Rejected, core.instance(), now).unwrap();
    let retries = core.lineage().retries();
    retries.arm("s1", &line, "window", "Usage limit reached", 1, core.instance(), now).unwrap();
    let pending = Response::SendOutcome(SendOutcomeWire::Duplicate(ReceiptStateWire::Pending));
    let rejected = Response::SendOutcome(SendOutcomeWire::Duplicate(ReceiptStateWire::Rejected));
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(call(&*c, send("k-1", "run the tests")).await, pending);
    })
    .await;
    retries.resolve("s1", oximux_agents::retry::store::Resolution::Cancelled, now).unwrap();
    as_scope(&h, LocalScope::Full, |c| async move {
        assert_eq!(call(&*c, send("k-1", "run the tests")).await, rejected, "nothing will send it now");
    })
    .await;
    assert_eq!(h.stub.sent().len(), 1);
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

/// The next pushed frame, or `None` if nothing comes within half a second.
async fn next_push(client: &dyn Transport) -> Option<Response> {
    let frame = tokio::time::timeout(std::time::Duration::from_millis(500), client.recv()).await.ok()?;
    Some(Response::from_bytes(&frame.unwrap().expect("a frame")).unwrap())
}

fn working(at: i64) -> HealthInputs {
    HealthInputs { turn_active: true, process: Process::Live, last_activity_at: at, ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v37_subscriber_is_pushed_health_changes_but_not_bare_activity() {
    let h = host(true);
    let handle = h.registry.get("s1").unwrap();
    let instance = h.core.as_ref().unwrap().instance().to_string();
    as_scope(&h, LocalScope::Full, |c| async move {
        call(&*c, Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await;
        assert!(matches!(call(&*c, Request::Subscribe { session_id: "s1".into(), after_seq: None }).await, Response::Events(_)));
        assert_eq!(next_push(&*c).await, None, "nothing observed yet: no frame");

        let now = oximux_agents::session_health::now_ms();
        handle.publish_health(working(now));
        let Some(Response::SessionHealth(frame)) = next_push(&*c).await else { panic!("expected a health frame") };
        assert_eq!((frame.session_id.as_str(), frame.host_instance_id.as_str()), ("s1", instance.as_str()));
        let health = frame.health().unwrap();
        assert_eq!((health.status.as_str(), health.liveness.as_str()), ("working", "live"));

        // Only the activity time moved: no frame. Then it fails: a frame.
        handle.publish_health(working(now + 1_000));
        handle.publish_health(HealthInputs { turn_active: false, last_turn_failure: Some("boom".into()), ..working(now + 2_000) });
        let got = next_push(&*c).await;
        let Some(Response::SessionHealth(frame)) = got else { panic!("expected a health frame, got {got:?}") };
        assert_eq!(frame.health().unwrap().status, "failed");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_v36_subscriber_is_never_pushed_health() {
    let h = host(true);
    let handle = h.registry.get("s1").unwrap();
    as_scope(&h, LocalScope::Full, |c| async move {
        call(&*c, Request::Hello(HelloReq { protocol_version: 36 })).await;
        assert!(matches!(call(&*c, Request::Subscribe { session_id: "s1".into(), after_seq: None }).await, Response::Events(_)));
        handle.publish_health(working(oximux_agents::session_health::now_ms()));
        handle.ingest(ThreadEvent::Notice("after".into()));
        // The next frame is the event: no health frame came first.
        assert!(matches!(next_push(&*c).await, Some(Response::Event(_))));
    })
    .await;
}

/// No host core keeps health, so none is pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_core_pushes_no_health() {
    let h = host(false);
    let handle = h.registry.get("s1").unwrap();
    as_scope(&h, LocalScope::Full, |c| async move {
        call(&*c, Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await;
        assert!(matches!(call(&*c, Request::Subscribe { session_id: "s1".into(), after_seq: None }).await, Response::Events(_)));
        handle.publish_health(working(oximux_agents::session_health::now_ms()));
        handle.ingest(ThreadEvent::Notice("after".into()));
        assert!(matches!(next_push(&*c).await, Some(Response::Event(_))));
    })
    .await;
}

/// A driver's first publish is pushed even when it reads as the default: a
/// freshly opened chat still tells its subscriber what it is and which host
/// says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_publish_like_the_default_is_still_pushed() {
    let h = host(true);
    let handle = h.registry.get("s1").unwrap();
    as_scope(&h, LocalScope::Full, |c| async move {
        call(&*c, Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await;
        assert!(matches!(call(&*c, Request::Subscribe { session_id: "s1".into(), after_seq: None }).await, Response::Events(_)));
        handle.publish_health(HealthInputs::default());
        assert!(matches!(next_push(&*c).await, Some(Response::SessionHealth(_))));
    })
    .await;
}

/// A process exit is published and the session unregistered on the same
/// tick: the subscriber still gets that last frame, whether or not it had
/// seen one before, and the push then ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_published_as_the_session_closes_still_reaches_the_subscriber() {
    for seen_before in [true, false] {
        let h = host(true);
        let registry = h.registry.clone();
        as_scope(&h, LocalScope::Full, |c| async move {
            call(&*c, Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await;
            assert!(matches!(call(&*c, Request::Subscribe { session_id: "s1".into(), after_seq: None }).await, Response::Events(_)));
            let now = oximux_agents::session_health::now_ms();
            if seen_before {
                registry.get("s1").unwrap().publish_health(working(now));
                assert!(matches!(next_push(&*c).await, Some(Response::SessionHealth(_))));
            }
            let exited = HealthInputs { process: Process::Exited(ExitProof::ExitCode(137)), ..working(now) };
            registry.get("s1").unwrap().publish_health(exited);
            drop(registry.unregister("s1"));
            let got = next_push(&*c).await;
            let Some(Response::SessionHealth(frame)) = got else { panic!("expected the exit frame, got {got:?}") };
            let health = frame.health().unwrap();
            assert_eq!((health.status.as_str(), health.exit_code), ("failed", Some(137)), "seen_before={seen_before}");
        })
        .await;
    }
}
