//! The v17 schedule surface: the run-now gates (authorization before
//! capability), and the recorded-run push — delivered to a subscriber that
//! declared v17, never to one that declared an older version, because a push
//! reaches peers that never asked and an old decoder would drop the whole
//! connection on the unknown ordinal.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope, RunNowError, ScheduleRunner};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{HelloReq, RunOutcomeWire, ScheduleRunWire};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn a_run(id: &str) -> ScheduleRunWire {
    ScheduleRunWire {
        schedule_id: id.into(),
        fired_at: "2026-08-04T09:00:00+07:00".into(),
        outcome: RunOutcomeWire::Ok,
        session_id: Some("sess-9".into()),
        detail: None,
    }
}

/// Counts how often the fire actually ran, so a refusal can be asserted as
/// "never reached the runner".
#[derive(Default)]
struct CountingRunner {
    fires: AtomicUsize,
}

#[async_trait::async_trait]
impl ScheduleRunner for CountingRunner {
    async fn run_now(&self, schedule_id: &str) -> Result<ScheduleRunWire, RunNowError> {
        self.fires.fetch_add(1, Ordering::SeqCst);
        Ok(a_run(schedule_id))
    }
}

/// Authorization decides before capability: a session-scoped caller is refused
/// (`Unauthorized`), an authorized caller on a host with no runner learns
/// `Unsupported`, and only a full-scope caller on a runner-equipped host fires.
#[test]
fn run_now_is_gated_authorization_first() {
    let registry = Arc::new(SessionRegistry::new());

    // Full scope, no runner: Unsupported — and honest about it.
    let bare = Arc::new(Dispatcher::new(registry.clone(), Arc::new(AuthStore::new())));
    let (server, client) = duplex_pair();
    let serve = bare.serve_local(&server, LocalScope::Full);
    let script = async move {
        let reply = call(&client, Request::RunScheduleNow { schedule_id: "sch-1".into() }).await;
        assert_eq!(reply, Response::Error(RpcError::Unsupported));
        drop(client);
    };
    block_on(join(serve, script));

    // A runner-equipped host: the session-scoped caller is refused before the
    // runner is consulted; the operator's fire reaches it.
    let runner = Arc::new(CountingRunner::default());
    let host = Arc::new(
        Dispatcher::new(registry, Arc::new(AuthStore::new()))
            .with_schedule_runner(runner.clone()),
    );
    let (server, client) = duplex_pair();
    let serve = host.serve_local(&server, LocalScope::Session("sess-1".into()));
    let script = async move {
        let reply = call(&client, Request::RunScheduleNow { schedule_id: "sch-1".into() }).await;
        assert_eq!(reply, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    block_on(join(serve, script));
    assert_eq!(runner.fires.load(Ordering::SeqCst), 0, "a refusal must never fire");

    let (server, client) = duplex_pair();
    let serve = host.serve_local(&server, LocalScope::Full);
    let script = async move {
        let reply = call(&client, Request::RunScheduleNow { schedule_id: "sch-1".into() }).await;
        match reply {
            Response::ScheduleRunRecorded(run) => {
                assert_eq!(run.schedule_id, "sch-1");
                assert_eq!(run.outcome, RunOutcomeWire::Ok);
            }
            other => panic!("expected the recorded run, got {other:?}"),
        }
        drop(client);
    };
    block_on(join(serve, script));
    assert_eq!(runner.fires.load(Ordering::SeqCst), 1);
}

/// A v17 session-list subscriber receives the recorded-run push.
#[test]
fn a_v17_subscriber_receives_the_run_push() {
    let registry = Arc::new(SessionRegistry::new());
    let (events, _) = tokio::sync::broadcast::channel(16);
    let dispatcher = Arc::new(
        Dispatcher::new(registry, Arc::new(AuthStore::new()))
            .with_schedule_events(events.clone()),
    );

    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let hello = call(
            &client,
            Request::Hello(HelloReq {
                protocol_version: oximux_remote_proto::proto::PROTOCOL_VERSION,
            }),
        )
        .await;
        assert!(matches!(hello, Response::HelloAck(_)));
        let subscribed = call(&client, Request::SubscribeSessions).await;
        assert!(matches!(subscribed, Response::Sessions(_)));

        events.send(a_run("sch-7")).expect("one subscriber");

        let frame = client.recv().await.unwrap().expect("the push frame");
        match Response::from_bytes(&frame).unwrap() {
            Response::ScheduleRunsChanged(run) => assert_eq!(run.schedule_id, "sch-7"),
            other => panic!("expected the run push, got {other:?}"),
        }
        drop(client);
    };
    block_on(join(serve, script));
}

/// A subscriber that declared an older version never receives the push — over
/// two full request round-trips, so a wrongly-merged stream would have had
/// every chance to surface.
#[test]
fn an_older_subscriber_is_never_pushed_a_run() {
    let registry = Arc::new(SessionRegistry::new());
    let (events, _) = tokio::sync::broadcast::channel(16);
    let dispatcher = Arc::new(
        Dispatcher::new(registry, Arc::new(AuthStore::new()))
            .with_schedule_events(events.clone()),
    );

    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let hello = call(&client, Request::Hello(HelloReq { protocol_version: 16 })).await;
        assert!(matches!(hello, Response::HelloAck(_)));
        let subscribed = call(&client, Request::SubscribeSessions).await;
        assert!(matches!(subscribed, Response::Sessions(_)));

        for round in 0..2 {
            events.send(a_run("sch-7")).ok();
            // The Ping round-trip gives the serve loop a full turn in which a
            // wrongly-merged push stream would have been polled and forwarded.
            let reply = call(&client, Request::Ping).await;
            assert_eq!(reply, Response::Pong, "round {round}: only the reply arrives");
        }
        drop(client);
    };
    block_on(join(serve, script));
}
