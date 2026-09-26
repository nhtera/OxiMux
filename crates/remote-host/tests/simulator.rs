//! `Request::Simulator` at the RPC boundary: who is served, and which worktree
//! the desktop is asked about.
//!
//! | caller                  | no simulator  | worktree the desktop sees        |
//! |-------------------------|---------------|----------------------------------|
//! | local operator          | `Unsupported` | the one the request names        |
//! | session-confined agent  | `Unsupported` | its session's cwd, never its own |
//! | paired phone            | `Unauthorized` (with or without a simulator)     |

use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope, PairingSlot, SimulatorControl, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimReplyWire, SimRequestWire};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

/// Records the worktree each call was about.
#[derive(Default)]
struct Spy {
    worktrees: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SimulatorControl for Spy {
    async fn run(&self, worktree: &str, _cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        self.worktrees.lock().unwrap().push(worktree.to_string());
        Ok(SimReplyWire::Done)
    }
}

fn sim(worktree: Option<&str>) -> Request {
    Request::Simulator(SimRequestWire { worktree: worktree.map(Into::into), cmd: SimCmdWire::Screenshot { full: false } })
}

/// A host with `sess-1` working in `/work/mine`, and optionally a simulator.
fn host(spy: Option<Arc<Spy>>) -> Dispatcher {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { cwd: Some("/work/mine".into()), ..Default::default() });
    registry.register("sess-2".into(), Arc::new(StubConnection::default()));
    let dispatcher = Dispatcher::new(registry, Arc::new(AuthStore::new()));
    match spy {
        Some(spy) => dispatcher.with_simulator(spy),
        None => dispatcher,
    }
}

fn local(dispatcher: &Dispatcher, scope: LocalScope, reqs: Vec<Request>) -> Vec<Response> {
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, scope);
    let script = async move {
        let mut out = Vec::new();
        for req in reqs {
            out.push(call(&client, req).await);
        }
        drop(client);
        out
    };
    block_on(join(serve, script)).1
}

#[test]
fn the_operator_reaches_the_worktree_it_names() {
    let spy = Arc::new(Spy::default());
    let got = local(&host(Some(spy.clone())), LocalScope::Full, vec![sim(Some("/work/other"))]);
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done))]);
    assert_eq!(*spy.worktrees.lock().unwrap(), vec!["/work/other".to_string()]);
}

#[test]
fn the_operator_must_name_a_worktree() {
    let spy = Arc::new(Spy::default());
    let got = local(&host(Some(spy.clone())), LocalScope::Full, vec![sim(None), sim(Some("  "))]);
    for response in got {
        assert!(matches!(response, Response::Error(RpcError::BadRequest(_))), "{response:?}");
    }
    assert!(spy.worktrees.lock().unwrap().is_empty());
}

/// **The confinement.** A confined agent that names another project's worktree
/// still reaches only its own session's — the request field is not consulted.
#[test]
fn a_confined_agent_cannot_name_another_worktree() {
    let spy = Arc::new(Spy::default());
    let got = local(
        &host(Some(spy.clone())),
        LocalScope::Session("sess-1".into()),
        vec![sim(Some("/work/someone-else")), sim(None)],
    );
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done)); 2]);
    assert_eq!(*spy.worktrees.lock().unwrap(), vec!["/work/mine".to_string(); 2]);
}

/// A session with no working directory has no worktree to resolve: no device,
/// and the desktop is never asked.
#[test]
fn a_confined_agent_without_a_cwd_has_no_device() {
    let spy = Arc::new(Spy::default());
    let got = local(&host(Some(spy.clone())), LocalScope::Session("sess-2".into()), vec![sim(Some("/work/mine"))]);
    assert_eq!(got, vec![Response::Simulator(Err(SimErrorWire::NoDevice))]);
    assert!(spy.worktrees.lock().unwrap().is_empty());
}

/// Headless `oximux serve`: a caller who may use a simulator hears there is
/// none.
#[test]
fn a_host_without_a_simulator_says_unsupported() {
    for scope in [LocalScope::Full, LocalScope::Session("sess-1".into())] {
        let got = local(&host(None), scope, vec![sim(Some("/work/mine"))]);
        assert_eq!(got, vec![Response::Error(RpcError::Unsupported)]);
    }
}

/// A paired phone never drives the simulator — and cannot tell whether the host
/// has one.
#[test]
fn a_paired_phone_is_refused_with_or_without_a_simulator() {
    const SECRET: [u8; 16] = [0x22; 16];
    const NOW: u64 = 1_700_000_000;
    fn clock() -> u64 {
        NOW
    }
    for spy in [None, Some(Arc::new(Spy::default()))] {
        let auth = Arc::new(AuthStore::new());
        auth.set_pairing(PairingSlot::new(SECRET, None, false));
        let mut dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth).with_clock(clock);
        if let Some(spy) = &spy {
            dispatcher = dispatcher.with_simulator(spy.clone());
        }
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&[0x33; 32]).verifying_key().to_bytes();
        let (server, client) = duplex_pair();
        let serve = dispatcher.serve(&server);
        let script = async move {
            let register = RegisterReq {
                app_pubkey: pubkey,
                device_name: "phone".into(),
                proof: registration_proof(&SECRET, &pubkey, NOW),
                timestamp_secs: NOW,
                session_id: None,
            };
            assert!(matches!(call(&client, Request::Register(register)).await, Response::Registered { .. }));
            let got = call(&client, sim(Some("/work/mine"))).await;
            drop(client);
            got
        };
        let got = block_on(join(serve, script)).1;
        assert_eq!(got, Response::Error(RpcError::Unauthorized));
        if let Some(spy) = spy {
            assert!(spy.worktrees.lock().unwrap().is_empty());
        }
    }
}
