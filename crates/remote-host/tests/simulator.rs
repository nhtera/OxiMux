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
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope, PairingSlot, SimPrincipal, SimulatorControl, registration_proof};
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

/// Records the worktree and the verb of each call.
#[derive(Default)]
struct Spy {
    worktrees: Mutex<Vec<String>>,
    principals: Mutex<Vec<SimPrincipal>>,
    cmds: Mutex<Vec<SimCmdWire>>,
}

#[async_trait::async_trait]
impl SimulatorControl for Spy {
    async fn run(&self, worktree: &str, principal: SimPrincipal, cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        self.worktrees.lock().unwrap().push(worktree.to_string());
        self.principals.lock().unwrap().push(principal);
        self.cmds.lock().unwrap().push(cmd);
        Ok(SimReplyWire::Done)
    }
}

/// A v27 verb — a caller's envelope around a settled tap by ref — crosses
/// the dispatcher untouched, and a confined agent's still lands on its own
/// worktree: the envelope names a caller, never a worktree.
#[test]
fn a_verify_loop_verb_reaches_the_simulator_as_sent() {
    use oximux_remote_proto::simulator::SimTargetWire;
    let spy = Arc::new(Spy::default());
    let cmd = SimCmdWire::AsCaller {
        caller: 42,
        cmd: Box::new(SimCmdWire::Settled { action: Box::new(SimCmdWire::Tap(SimTargetWire::Ref(7))), timeout_ms: 0 }),
    };
    let req = Request::Simulator(SimRequestWire { worktree: Some("/work/other".into()), cmd: cmd.clone() });
    let got = local(&host(Some(spy.clone())), LocalScope::Session("sess-1".into()), vec![req]);
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done))]);
    assert_eq!(*spy.worktrees.lock().unwrap(), ["/work/mine"]);
    assert_eq!(*spy.cmds.lock().unwrap(), [cmd]);
}

/// A device-state change (v28) is forwarded as sent, confined to the
/// session's worktree like every other verb.
#[test]
fn a_device_state_change_reaches_the_simulator_as_sent() {
    use oximux_remote_proto::simulator::{SimLocationWire, SimStateCmdWire};
    let spy = Arc::new(Spy::default());
    let cmd = SimCmdWire::DeviceState(SimStateCmdWire::Location(SimLocationWire::Set { lat: 10.77, lon: 106.7 }));
    let req = Request::Simulator(SimRequestWire { worktree: Some("/work/other".into()), cmd: cmd.clone() });
    let got = local(&host(Some(spy.clone())), LocalScope::Session("sess-1".into()), vec![req]);
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done))]);
    assert_eq!(*spy.worktrees.lock().unwrap(), ["/work/mine"]);
    assert_eq!(*spy.cmds.lock().unwrap(), [cmd]);
}

/// A diagnostics read (v29) is forwarded as sent, confined to the session's
/// worktree like every other verb.
#[test]
fn a_log_read_reaches_the_simulator_as_sent() {
    use oximux_remote_proto::simulator::{SimLogLevelWire, SimLogsWire};
    let spy = Arc::new(Spy::default());
    let cmd = SimCmdWire::Logs(SimLogsWire { since_secs: 30, app: Some("com.a.b".into()), grep: None, level: SimLogLevelWire::Error, lines: 200 });
    let req = Request::Simulator(SimRequestWire { worktree: Some("/work/other".into()), cmd: cmd.clone() });
    let got = local(&host(Some(spy.clone())), LocalScope::Session("sess-1".into()), vec![req]);
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done))]);
    assert_eq!(*spy.worktrees.lock().unwrap(), ["/work/mine"]);
    assert_eq!(*spy.cmds.lock().unwrap(), [cmd]);
}

/// An outside MCP client is its own principal and names its worktree; an
/// agent and the operator are agents.
#[test]
fn the_principal_follows_the_credential() {
    let spy = Arc::new(Spy::default());
    let got = local(&host(Some(spy.clone())), LocalScope::Mcp, vec![sim(Some("/work/repo"))]);
    assert_eq!(got, vec![Response::Simulator(Ok(SimReplyWire::Done))]);
    local(&host(Some(spy.clone())), LocalScope::Full, vec![sim(Some("/work/repo"))]);
    local(&host(Some(spy.clone())), LocalScope::Session("sess-1".into()), vec![sim(None)]);
    assert_eq!(*spy.principals.lock().unwrap(), [SimPrincipal::Mcp, SimPrincipal::Agent, SimPrincipal::Agent]);
    assert_eq!(*spy.worktrees.lock().unwrap(), ["/work/repo", "/work/repo", "/work/mine"]);
}

/// A verb that never finishes, and says when it is dropped.
struct Hang(Arc<std::sync::atomic::AtomicBool>);

struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SimulatorControl for Hang {
    async fn run(&self, _: &str, _: SimPrincipal, _: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        let _flag = DropFlag(self.0.clone());
        futures::future::pending().await
    }
}

/// A caller that goes away (a client timeout, a cancelled MCP call) takes its
/// verb with it: nothing keeps acting on the device for nobody.
#[test]
fn a_caller_that_leaves_drops_its_verb() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_simulator(Arc::new(Hang(dropped.clone())));
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Mcp);
    let script = async move {
        client.send(sim(Some("/work/repo")).to_bytes().unwrap()).await.unwrap();
        drop(client);
    };
    block_on(join(serve, script));
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the verb was dropped with its caller");
}

/// A caller that sends another request and then leaves still takes its verb
/// with it: a queued frame does not stop the connection being watched.
#[test]
fn a_caller_that_pipelines_then_leaves_drops_its_verb() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_simulator(Arc::new(Hang(dropped.clone())));
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        client.send(sim(Some("/work/repo")).to_bytes().unwrap()).await.unwrap();
        client.send(sim(Some("/work/repo")).to_bytes().unwrap()).await.unwrap();
        drop(client);
    };
    block_on(join(serve, script));
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the verb was dropped with its caller");
}

/// The first verb waits until released; the rest answer at once.
struct Gate(Mutex<Option<futures::channel::oneshot::Receiver<()>>>);

#[async_trait::async_trait]
impl SimulatorControl for Gate {
    async fn run(&self, _: &str, _: SimPrincipal, cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        let gate = self.0.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        Ok(match cmd {
            SimCmdWire::Status => SimReplyWire::Done,
            _ => SimReplyWire::Screenshot { png: vec![], width: 1, height: 1, scale: 1.0 },
        })
    }
}

/// A request sent while a verb runs (pipelined) is kept, and answered after
/// it, in order — once each.
#[test]
fn a_pipelined_request_is_answered_after_the_verb_before_it() {
    let (release, gate) = futures::channel::oneshot::channel();
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_simulator(Arc::new(Gate(Mutex::new(Some(gate)))));
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        client.send(sim(Some("/w")).to_bytes().unwrap()).await.unwrap();
        let status = Request::Simulator(SimRequestWire { worktree: Some("/w".into()), cmd: SimCmdWire::Status });
        client.send(status.to_bytes().unwrap()).await.unwrap();
        let _ = release.send(());
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(Response::from_bytes(&client.recv().await.unwrap().unwrap()).unwrap());
        }
        drop(client);
        got
    };
    let got = block_on(join(serve, script)).1;
    assert!(matches!(got[0], Response::Simulator(Ok(SimReplyWire::Screenshot { .. }))), "{got:?}");
    assert_eq!(got[1], Response::Simulator(Ok(SimReplyWire::Done)));
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
