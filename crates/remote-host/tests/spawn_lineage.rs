//! Agent delegation (v36) end to end over the in-memory loopback: an agent's
//! confined credential spawning, listing, cancelling and asking who it is.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::join;
use oximux_agents::lineage::{LineageState, LineageStore};
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::launcher::ChildCtx;
use oximux_remote_host::{
    AuthStore, Dispatcher, HostConfig, HostCore, HostCoreDeps, LaunchError, LocalScope, PriorOwner,
    SessionLauncher,
};
use oximux_remote_proto::Transport;
use oximux_remote_proto::lineage::{
    AgentRosterWire, NotifyParentWire, SpawnAgentReq, SpawnCwdWire, SpawnErrorKind, SpawnParentWire,
    SpawnPostureWire,
};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

/// Starts children by registering a stub session for each; can be told to fail.
struct FakeLauncher {
    registry: Arc<SessionRegistry>,
    cwd: std::path::PathBuf,
    launched: AtomicUsize,
    fail: AtomicBool,
    contexts: Mutex<Vec<ChildCtx>>,
    roster: Vec<AgentRosterWire>,
}

#[async_trait::async_trait]
impl SessionLauncher for FakeLauncher {
    async fn create(&self, cwd: &str, agent: Option<&str>, model: Option<&str>) -> Result<String, LaunchError> {
        self.create_child(cwd, agent, model, ChildCtx::default()).await
    }

    async fn create_child(
        &self,
        _cwd: &str,
        _agent: Option<&str>,
        _model: Option<&str>,
        ctx: ChildCtx,
    ) -> Result<String, LaunchError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(LaunchError::Failed);
        }
        self.contexts.lock().unwrap().push(ctx);
        let n = self.launched.fetch_add(1, Ordering::SeqCst);
        let id = format!("child-{n}");
        let handle = self.registry.register(id.clone(), Arc::new(StubConnection::default()));
        handle.set_meta(SessionMeta { cwd: Some(self.cwd.clone()), ..Default::default() });
        handle.set_working(true);
        Ok(id)
    }

    fn roster(&self) -> Vec<AgentRosterWire> {
        self.roster.clone()
    }
}

/// A launcher that keeps the trait's default `create_child`.
struct PlainLauncher;

#[async_trait::async_trait]
impl SessionLauncher for PlainLauncher {
    async fn create(&self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<String, LaunchError> {
        Ok("plain-child".into())
    }
}

struct Host {
    dispatcher: Dispatcher,
    registry: Arc<SessionRegistry>,
    core: Arc<HostCore>,
    launcher: Arc<FakeLauncher>,
    _dir: tempfile::TempDir,
}

fn host(owner: bool, roster: Vec<AgentRosterWire>) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let parent = registry.register("parent".into(), Arc::new(StubConnection::default()));
    parent.set_meta(SessionMeta { cwd: Some(dir.path().to_path_buf()), ..Default::default() });
    let db = oximux_storage::db::open_memory().unwrap();
    let core = HostCore::new(HostCoreDeps {
        rt: tokio::runtime::Handle::current(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    if owner {
        core.start(PriorOwner::None);
    }
    let launcher = Arc::new(FakeLauncher {
        registry: registry.clone(),
        cwd: dir.path().to_path_buf(),
        launched: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
        contexts: Mutex::new(Vec::new()),
        roster,
    });
    let dispatcher = Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
        .with_launcher(launcher.clone())
        .with_host_core(core.clone());
    Host { dispatcher, registry, core, launcher, _dir: dir }
}

fn spawn(request: &str) -> SpawnAgentReq {
    SpawnAgentReq {
        request_id: request.into(),
        parent: SpawnParentWire::Caller,
        agent_id: "claude-code".into(),
        model: None,
        prompt: "summarize the README".into(),
        cwd: SpawnCwdWire::Inherit,
        output_schema: None,
        notify_parent: NotifyParentWire::Auto,
        allow_parent_permits: false,
        posture: SpawnPostureWire::Default,
    }
}

fn spawn_error(resp: Response) -> (SpawnErrorKind, String) {
    match resp {
        Response::Error(RpcError::Spawn(e)) => (e.kind, e.message),
        other => panic!("expected a spawn refusal, got {other:?}"),
    }
}

/// Run `script` against `host` as the agent whose session is `scope`.
async fn as_agent<F, Fut>(host: &Host, scope: &str, script: F)
where
    F: FnOnce(Arc<dyn Transport>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (client, server) = duplex_pair();
    let client: Arc<dyn Transport> = Arc::new(client);
    let serve = host.dispatcher.serve_local(&server, LocalScope::Session(scope.into()));
    let run = script(client.clone());
    join(serve, async move {
        run.await;
        drop(client);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_spawns_a_child_that_opens_beside_it_without_focus() {
    let h = host(true, vec![]);
    as_agent(&h, "parent", |c| async move {
        let Response::AgentSpawned(r) = call(&*c, Request::SpawnAgent(spawn("r1"))).await else {
            panic!("expected AgentSpawned");
        };
        assert_eq!((r.parent_id.as_str(), r.root_id.as_str(), r.depth, r.replayed), ("parent", "parent", 1, false));
        // A replay of the same request starts nothing new.
        let Response::AgentSpawned(again) = call(&*c, Request::SpawnAgent(spawn("r1"))).await else {
            panic!("expected AgentSpawned");
        };
        assert!(again.replayed);
        assert_eq!(again.session_id, r.session_id);
    })
    .await;
    assert_eq!(h.launcher.launched.load(Ordering::SeqCst), 1, "one launch for two identical requests");
    let ctx = h.launcher.contexts.lock().unwrap()[0].clone();
    assert_eq!(ctx.parent_session_id.as_deref(), Some("parent"));
    assert!(!ctx.activate, "a spawned child never steals focus");
    assert_eq!(h.core.lineage().row("child-0").unwrap().unwrap().state, LineageState::Active);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_at_the_default_depth_cannot_spawn() {
    let h = host(true, vec![]);
    as_agent(&h, "parent", |c| async move {
        assert!(matches!(call(&*c, Request::SpawnAgent(spawn("r1"))).await, Response::AgentSpawned(_)));
    })
    .await;
    as_agent(&h, "child-0", |c| async move {
        let (kind, msg) = spawn_error(call(&*c, Request::SpawnAgent(spawn("g1"))).await);
        assert_eq!(kind, SpawnErrorKind::DepthExceeded);
        assert!(msg.ends_with("Complete this task yourself."), "{msg}");
        let Response::Whoami(me) = call(&*c, Request::Whoami).await else { panic!("Whoami") };
        assert_eq!((me.scope.as_str(), me.depth, me.spawn_allowed), ("session", 1, false));
        assert_eq!(me.root_id.as_deref(), Some("parent"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_host_refuses_to_spawn() {
    let h = host(false, vec![]);
    as_agent(&h, "parent", |c| async move {
        let (kind, _) = spawn_error(call(&*c, Request::SpawnAgent(spawn("r1"))).await);
        assert_eq!(kind, SpawnErrorKind::NotAllowed);
    })
    .await;
    assert_eq!(h.launcher.launched.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_failed_launches_trip_the_breaker_and_leave_no_reservation() {
    let h = host(true, vec![]);
    h.launcher.fail.store(true, Ordering::SeqCst);
    as_agent(&h, "parent", |c| async move {
        for i in 0..3 {
            let resp = call(&*c, Request::SpawnAgent(spawn(&format!("f{i}")))).await;
            assert!(matches!(resp, Response::Error(RpcError::BadRequest(_))), "{resp:?}");
        }
        let (kind, msg) = spawn_error(call(&*c, Request::SpawnAgent(spawn("f3"))).await);
        assert_eq!(kind, SpawnErrorKind::StartFailing);
        assert!(msg.contains("fix the cause or complete the task yourself"), "{msg}");
    })
    .await;
    assert!(h.core.lineage().find_by_request("parent", "f0").unwrap().is_none(), "rolled back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launcher_that_cannot_hold_a_posture_refuses_instead_of_downgrading() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let parent = registry.register("parent".into(), Arc::new(StubConnection::default()));
    parent.set_meta(SessionMeta { cwd: Some(dir.path().to_path_buf()), ..Default::default() });
    let db = oximux_storage::db::open_memory().unwrap();
    let core = HostCore::new(HostCoreDeps {
        rt: tokio::runtime::Handle::current(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.start(PriorOwner::None);
    let dispatcher = Dispatcher::new(registry, Arc::new(AuthStore::new()))
        .with_launcher(Arc::new(PlainLauncher))
        .with_host_core(core.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Session("parent".into()));
    join(serve, async move {
        let mut review = spawn("rev");
        review.posture = SpawnPostureWire::Review;
        let (kind, _) = spawn_error(call(&client, Request::SpawnAgent(review)).await);
        assert_eq!(kind, SpawnErrorKind::PostureUnenforceable);
        drop(client);
    })
    .await;
    assert!(core.lineage().find_by_request("parent", "rev").unwrap().is_none(), "nothing left reserved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_agent_is_refused_with_the_installed_ids() {
    let roster = vec![
        AgentRosterWire {
            agent_id: "claude-code".into(),
            display: "Claude".into(),
            default_model: None,
            models: vec![],
            postures: vec![],
            installed: true,
        },
        AgentRosterWire {
            agent_id: "codex".into(),
            display: "Codex".into(),
            default_model: None,
            models: vec![],
            postures: vec![],
            installed: true,
        },
    ];
    let h = host(true, roster);
    as_agent(&h, "parent", |c| async move {
        let mut req = spawn("r1");
        req.agent_id = "gemini".into();
        let (kind, msg) = spawn_error(call(&*c, Request::SpawnAgent(req)).await);
        assert_eq!(kind, SpawnErrorKind::UnknownAgent);
        assert!(msg.contains("claude-code") && msg.contains("codex"), "{msg}");
        let Response::Whoami(me) = call(&*c, Request::Whoami).await else { panic!("Whoami") };
        assert_eq!(me.agents.len(), 2);
        assert!(me.spawn_allowed);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_outside_the_parent_is_refused() {
    let h = host(true, vec![]);
    as_agent(&h, "parent", |c| async move {
        let mut req = spawn("r1");
        req.cwd = SpawnCwdWire::Path("../../etc".into());
        let (kind, _) = spawn_error(call(&*c, Request::SpawnAgent(req)).await);
        assert_eq!(kind, SpawnErrorKind::OutsideRepo);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_lists_and_cancels_its_children_but_not_a_strangers() {
    let h = host(true, vec![]);
    as_agent(&h, "parent", |c| async move {
        let Response::AgentSpawned(child) = call(&*c, Request::SpawnAgent(spawn("r1"))).await else {
            panic!("expected AgentSpawned");
        };
        let Response::Children(kids) =
            call(&*c, Request::ListChildren { of: "parent".into(), recursive: true }).await
        else {
            panic!("expected Children");
        };
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].session_id, child.session_id);
        let Response::AgentsCancelled { ended } =
            call(&*c, Request::CancelAgent { id: child.session_id.clone(), cascade: true }).await
        else {
            panic!("expected AgentsCancelled");
        };
        assert_eq!(ended, vec![child.session_id]);
        // A run is the operator's to cancel.
        let (kind, _) =
            spawn_error(call(&*c, Request::CancelAgent { id: "team:run-1".into(), cascade: true }).await);
        assert_eq!(kind, SpawnErrorKind::NotInSubtree);
    })
    .await;
    // Another agent can reach none of it.
    as_agent(&h, "stranger", |c| async move {
        let (kind, _) = spawn_error(call(&*c, Request::ListChildren { of: "parent".into(), recursive: false }).await);
        assert_eq!(kind, SpawnErrorKind::NotInSubtree);
    })
    .await;
    assert_eq!(h.core.lineage().row("child-0").unwrap().unwrap().state, LineageState::Ended);
}

/// The Critical row "parent reads/waits/sends to its own child": the session
/// verbs reach a confined caller's subtree, and a stranger's child stays out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_reaches_its_child_through_the_session_verbs() {
    use oximux_remote_proto::messages::SendPromptReq;
    let h = host(true, vec![]);
    let child = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let child_id = child.clone();
    as_agent(&h, "parent", |c| async move {
        let Response::AgentSpawned(r) = call(&*c, Request::SpawnAgent(spawn("r1"))).await else {
            panic!("expected AgentSpawned");
        };
        *child_id.lock().unwrap() = r.session_id.clone();
        let send = Request::SendPrompt(SendPromptReq {
            session_id: r.session_id.clone(),
            text: "also check the tests".into(),
            images: vec![],
            corr_id: 1,
        });
        assert_eq!(call(&*c, send).await, Response::Ack, "a parent sends to its child");
        assert!(
            matches!(call(&*c, Request::EventsSince { session_id: r.session_id.clone(), after_seq: 0 }).await, Response::Events(_)),
            "a parent reads its child's events"
        );
        assert!(
            matches!(call(&*c, Request::GetSessionInfo { session_id: r.session_id.clone() }).await, Response::SessionInfo(_)),
            "a parent reads its child's info"
        );
    })
    .await;
    let child = child.lock().unwrap().clone();
    as_agent(&h, "stranger", |c| async move {
        let send = Request::SendPrompt(SendPromptReq { session_id: child.clone(), text: "x".into(), images: vec![], corr_id: 1 });
        assert_eq!(call(&*c, send).await, Response::Error(RpcError::Unauthorized));
        assert_eq!(
            call(&*c, Request::EventsSince { session_id: child, after_seq: 0 }).await,
            Response::Error(RpcError::Unauthorized)
        );
    })
    .await;
}

/// A cap hit is visible to the human: a notice lands in the parent's
/// transcript, not only in the agent's tool output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_hit_leaves_a_notice_in_the_parents_transcript() {
    let h = host(true, vec![]);
    h.core.set_config(HostConfig {
        caps: oximux_agents::lineage::SpawnCaps { active_per_parent: 1, ..Default::default() },
        ..HostConfig::default()
    });
    let mut events = h.registry.get("parent").unwrap().subscribe();
    as_agent(&h, "parent", |c| async move {
        assert!(matches!(call(&*c, Request::SpawnAgent(spawn("r1"))).await, Response::AgentSpawned(_)));
        let (kind, _) = spawn_error(call(&*c, Request::SpawnAgent(spawn("r2"))).await);
        assert_eq!(kind, SpawnErrorKind::FanoutExceeded);
    })
    .await;
    let mut saw = false;
    while let Ok((_, ev)) = events.try_recv() {
        if let oximux_agent_core::thread::ThreadEvent::Notice(text) = ev {
            saw = text.contains("1 of 1 active children") && text.contains("Settings");
        }
    }
    assert!(saw, "the parent's transcript names the limit and where to raise it");
}

/// A team run's roles join the lineage under `team:<run>` at depth 1, so a
/// role's own helper is depth 2 (refused at the default) and the operator's
/// `CancelAgent team:<run>` ends the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_run_roles_are_recorded_under_the_run() {
    use oximux_remote_proto::messages::{TeamRoleSpecWire, TeamRunCreateReq};
    let h = host(true, vec![]);
    let db = oximux_storage::db::open_memory().unwrap();
    let dispatcher = Dispatcher::new(h.registry.clone(), Arc::new(AuthStore::new()))
        .with_launcher(h.launcher.clone())
        .with_host_core(h.core.clone())
        .with_team_store(Arc::new(oximux_agents::team::TeamStore::new(db.conn())));
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let cwd = h._dir.path().to_string_lossy().into_owned();
    let run_id = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let seen = run_id.clone();
    join(serve, async move {
        let reply = call(
            &client,
            Request::TeamRunCreate(TeamRunCreateReq {
                name: "ship".into(),
                cwd,
                agent_id: None,
                worktree_each: false,
                roles: vec![TeamRoleSpecWire { name: "plan".into(), prompt: "go".into() }],
            }),
        )
        .await;
        let Response::TeamRun(run) = reply else { panic!("expected TeamRun, got {reply:?}") };
        *seen.lock().unwrap() = run.id.clone();
        let Response::AgentsCancelled { ended } =
            call(&client, Request::CancelAgent { id: format!("team:{}", run.id), cascade: true }).await
        else {
            panic!("expected AgentsCancelled");
        };
        assert_eq!(ended, vec!["child-0".to_string()]);
        drop(client);
    })
    .await;
    let row = h.core.lineage().row("child-0").unwrap().unwrap();
    assert_eq!(row.parent_id, format!("team:{}", run_id.lock().unwrap()));
    assert_eq!(row.depth, 1);
}
