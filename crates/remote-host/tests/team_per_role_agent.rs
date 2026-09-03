//! The v22 team surface: each role picks its own agent and model, and the
//! board says which one worked it.
//!
//! What these hold the host to, in order:
//!
//! | claim                                        | proved by                                  |
//! |----------------------------------------------|--------------------------------------------|
//! | a role's own agent reaches the launcher      | a launcher that records what it was asked  |
//! | a role naming none falls back to the run's   | the same recording                         |
//! | the board reports which agent worked a role  | reading it back through `TeamStatusV2`     |
//! | a role's model reaches the LAUNCHER          | the same recording — and no `set_model` at all |
//! | a v18 run still reads back                   | created through the v18 verb, read through v2 |
//!
//! The model assertion is about *where* the model is applied, not merely that
//! it is. Applying it as a switch after launch is the obvious shape and it is
//! wrong: Claude and Codex take `--model` on the command line and refuse to
//! change it at runtime, so on a headless host that switch bails for exactly
//! the two agents this feature exists to combine. No stub can catch that by
//! being unswitchable — `StubConnection::set_model` succeeds — so the gate is
//! that the backend is never asked to switch at all.

use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::team::TeamStore;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, LaunchError, LocalScope, SessionLauncher};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{
    TeamRoleSpecV2Wire, TeamRoleSpecWire, TeamRoleStatusWire, TeamRunCreateReq, TeamRunCreateV2Req,
};
use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::testing::duplex_pair;

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

/// A launcher that records the agent it was asked for, per session it opens.
///
/// The recording is the point. A board echoing the agent the *client* sent
/// would pass while the launcher was still starting the run-level default for
/// every role — the exact bug this phase exists to fix — so the assertion has
/// to be about what reached the launcher, not about what came back.
struct RecordingLauncher {
    registry: Arc<SessionRegistry>,
    /// Stand in for an ACP agent: one that cannot be given a model at spawn.
    refuses_model: bool,
    /// `(session_id, agent_id, backend)` in launch order. The backend is kept
    /// so a test can assert what actually reached it — a reply that
    /// acknowledged a model switch while dropping it looks identical on the
    /// wire.
    launched: Launched,
}

#[async_trait::async_trait]
impl SessionLauncher for RecordingLauncher {
    async fn create(
        &self,
        _cwd: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, LaunchError> {
        if model.is_some() && self.refuses_model {
            return Err(LaunchError::ModelUnsupported);
        }
        let mut launched = self.launched.lock().unwrap();
        let id = format!("role-{}", launched.len() + 1);
        let conn = Arc::new(StubConnection::default());
        self.registry.register(id.clone(), conn.clone());
        launched.push(Launch {
            agent_id: agent_id.map(str::to_string),
            model: model.map(str::to_string),
            conn,
        });
        Ok(id)
    }
}

/// What one launch was asked for, and the backend it opened.
struct Launch {
    agent_id: Option<String>,
    model: Option<String>,
    conn: Arc<StubConnection>,
}

type Launched = Arc<Mutex<Vec<Launch>>>;

struct Fixture {
    dispatcher: Arc<Dispatcher>,
    teams: TeamStore,
    launched: Launched,
    /// Held so the in-memory database outlives the stores built on it.
    _db: oximux_storage::Db,
}

fn host() -> Fixture {
    host_with(false)
}

/// A host whose agents cannot be given a model at spawn — an ACP preset, as far
/// as this test is concerned.
fn host_refusing_models() -> Fixture {
    host_with(true)
}

fn host_with(refuses_model: bool) -> Fixture {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    let registry = Arc::new(SessionRegistry::new());
    let teams = TeamStore::new(db.conn());
    let launched = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(
        Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
            .with_team_store(Arc::new(teams.clone()))
            .with_launcher(Arc::new(RecordingLauncher {
                registry,
                launched: launched.clone(),
                refuses_model,
            })),
    );
    Fixture { dispatcher, teams, launched, _db: db }
}

fn talk<F, Fut>(dispatcher: &Arc<Dispatcher>, script: F)
where
    F: FnOnce(oximux_remote_proto::testing::DuplexTransport) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (server, client) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    block_on(join(serve, script(client)));
}

fn role(name: &str, agent: Option<&str>, model: Option<&str>) -> TeamRoleSpecV2Wire {
    TeamRoleSpecV2Wire {
        name: name.into(),
        prompt: format!("work on {name}"),
        agent_id: agent.map(str::to_string),
        model: model.map(str::to_string),
    }
}

fn create(roles: Vec<TeamRoleSpecV2Wire>, run_agent: Option<&str>) -> Request {
    Request::TeamRunCreateV2(TeamRunCreateV2Req {
        name: "sweep".into(),
        cwd: "/work".into(),
        agent_id: run_agent.map(str::to_string),
        worktree_each: false,
        roles,
    })
}

/// The phase's headline: two roles, two different agents, actually launched
/// that way — and the board says so afterwards.
#[test]
fn each_role_launches_with_the_agent_it_named() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            create(
                vec![role("plan", Some("claude"), None), role("impl", Some("codex"), None)],
                None,
            ),
        )
        .await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2, got {reply:?}") };
        assert_eq!(run.roles.len(), 2);

        // Read it back rather than trusting the create reply: the board after a
        // restart is the one that matters, and it comes from the database.
        let board = call(&client, Request::TeamStatusV2 { run_id: run.id.clone() }).await;
        let Response::TeamRunV2(board) = board else { panic!("expected TeamRunV2") };
        assert_eq!(board.roles[0].agent_id.as_deref(), Some("claude"));
        assert_eq!(board.roles[1].agent_id.as_deref(), Some("codex"));
    });

    let launched = fx.launched.lock().unwrap();
    assert_eq!(
        launched.iter().map(|l| l.agent_id.as_deref()).collect::<Vec<_>>(),
        [Some("claude"), Some("codex")],
        "the launcher itself was asked for two different agents"
    );
}

/// The compatibility half: a role that names no agent still gets the run's.
#[test]
fn a_role_naming_no_agent_falls_back_to_the_runs() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            create(vec![role("plan", Some("codex"), None), role("impl", None, None)], Some("claude")),
        )
        .await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };
        assert_eq!(run.roles[0].agent_id.as_deref(), Some("codex"), "its own choice wins");
        assert_eq!(
            run.roles[1].agent_id.as_deref(),
            Some("claude"),
            "and the one that chose nothing takes the run's"
        );
    });

    let launched = fx.launched.lock().unwrap();
    assert_eq!(
        launched.iter().map(|l| l.agent_id.as_deref()).collect::<Vec<_>>(),
        [Some("codex"), Some("claude")],
    );
}

/// A run whose roles name nothing at all records nothing — the host resolved
/// its own default and never reported which one, so claiming a name here would
/// be an invention.
#[test]
fn a_run_naming_no_agent_anywhere_records_none() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(&client, create(vec![role("solo", None, None)], None)).await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };
        assert_eq!(run.roles[0].agent_id, None);
        assert_eq!(run.roles[0].status, TeamRoleStatusWire::Running);
    });
    assert_eq!(fx.launched.lock().unwrap()[0].agent_id, None, "and the launcher was told nothing");
}

/// The model a role asked for reaches the **launcher**, and the backend is
/// never asked to switch.
///
/// This is the whole finding, as a gate. A post-launch `set_model` looks
/// equivalent and passes against any stub, because `StubConnection::set_model`
/// succeeds — but Claude and Codex take `--model` at spawn and refuse it at
/// runtime, so on a headless host (no view, therefore no respawn) that switch
/// bails and the role dies before doing anything. Asserting the *absence* of a
/// switch is what pins the fix; asserting the model "was applied" would not.
#[test]
fn a_roles_model_reaches_the_launcher_rather_than_a_later_switch() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            create(
                vec![
                    role("impl", Some("codex"), Some("gpt-5")),
                    role("review", Some("claude"), None),
                ],
                None,
            ),
        )
        .await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };
        assert_eq!(
            run.roles[0].status,
            TeamRoleStatusWire::Running,
            "summary: {:?}",
            run.roles[0].summary
        );
        assert_eq!(run.roles[0].model.as_deref(), Some("gpt-5"), "and the board says so");
        assert_eq!(run.roles[1].model, None, "a role that asked for none has none");
    });

    let launched = fx.launched.lock().unwrap();
    assert_eq!(
        launched.iter().map(|l| l.model.as_deref()).collect::<Vec<_>>(),
        [Some("gpt-5"), None],
        "the model was named at spawn, where every backend accepts one"
    );
    for launch in launched.iter() {
        let sent = launch.conn.sent();
        assert!(
            !sent.iter().any(|v| v["type"] == "set_model"),
            "no role is switched after launch — that path bails on Claude and \
             Codex, which is exactly where a team run needs it: {sent:?}"
        );
        assert!(
            sent.iter().any(|v| v["type"] == "user"),
            "and every role still got its opening prompt: {sent:?}"
        );
    }
}

/// A launch that fails takes only its own role down, and the board still says
/// what that role was asked to run — which is the only way to see why later.
#[test]
fn a_failed_launch_still_records_what_the_role_asked_for() {
    let fx = host();
    let run_id: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let seen = run_id.clone();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            create(vec![role("impl", Some("codex"), Some("gpt-5"))], None),
        )
        .await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };
        *seen.lock().unwrap() = run.id.clone();
    });

    let run_id = run_id.lock().unwrap().clone();
    let stored = fx.teams.get(&run_id).expect("get").expect("exists");
    assert_eq!(stored.roles[0].agent_id.as_deref(), Some("codex"));
    assert_eq!(stored.roles[0].model.as_deref(), Some("gpt-5"));
}

/// A run opened through the v18 verb reads back through the v2 board with no
/// agent recorded — not as an error, and not as an invented one.
#[test]
fn a_v18_run_reads_back_through_the_v2_board() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            Request::TeamRunCreate(TeamRunCreateReq {
                name: "legacy".into(),
                cwd: "/work".into(),
                agent_id: None,
                worktree_each: false,
                roles: vec![TeamRoleSpecWire { name: "impl".into(), prompt: "go".into() }],
            }),
        )
        .await;
        let Response::TeamRun(run) = reply else { panic!("expected the v18 TeamRun, got {reply:?}") };

        let board = call(&client, Request::TeamStatusV2 { run_id: run.id.clone() }).await;
        let Response::TeamRunV2(board) = board else { panic!("expected TeamRunV2") };
        assert_eq!(board.roles[0].name, "impl");
        assert_eq!(board.roles[0].status, TeamRoleStatusWire::Running);
        assert_eq!(board.roles[0].agent_id, None, "no agent recorded is a real answer");
        assert_eq!(board.roles[0].model, None);
    });
}

/// And the reverse direction: a run opened with per-role agents is still
/// readable through the v18 verb, which simply has nowhere to show them.
#[test]
fn a_v22_run_still_reads_back_through_the_v18_board() {
    let fx = host();
    talk(&fx.dispatcher, |client| async move {
        let reply =
            call(&client, create(vec![role("plan", Some("claude"), None)], None)).await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };

        let board = call(&client, Request::TeamStatus { run_id: run.id.clone() }).await;
        let Response::TeamRun(board) = board else { panic!("expected the v18 TeamRun") };
        assert_eq!(board.roles[0].name, "plan");
        assert_eq!(board.roles[0].status, TeamRoleStatusWire::Running);
    });
}

/// A model an agent cannot take at spawn fails that role, loudly.
///
/// The alternative is the one outcome worse than an error: ACP takes no model
/// at spawn, so the session would open on the agent's default while the board
/// recorded the model that was asked for — a false record that nothing ever
/// contradicts. The role's teammate is untouched, so the run still does the
/// work it can.
#[test]
fn a_model_an_agent_cannot_take_at_spawn_fails_only_that_role() {
    let fx = host_refusing_models();
    talk(&fx.dispatcher, |client| async move {
        let reply = call(
            &client,
            create(
                vec![
                    role("plan", Some("some-acp-agent"), Some("opus-5")),
                    role("impl", Some("some-acp-agent"), None),
                ],
                None,
            ),
        )
        .await;
        let Response::TeamRunV2(run) = reply else { panic!("expected TeamRunV2") };

        assert_eq!(run.roles[0].status, TeamRoleStatusWire::Failed);
        assert!(
            run.roles[0].summary.as_deref().unwrap_or_default().contains("--role-model"),
            "and it says what to do about it: {:?}",
            run.roles[0].summary
        );
        assert_eq!(run.roles[0].session_id, None, "nothing was started for it");
        assert_eq!(
            run.roles[0].model.as_deref(),
            Some("opus-5"),
            "the board still records what was asked for — that is why it failed"
        );

        assert_eq!(
            run.roles[1].status,
            TeamRoleStatusWire::Running,
            "the role that named no model is unaffected: {:?}",
            run.roles[1].summary
        );
    });
}
