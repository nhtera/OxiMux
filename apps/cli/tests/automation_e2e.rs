//! The v18 automation verbs, compiled binary against a live host: heartbeats an
//! agent arms on itself, a two-role team run that converges as its roles
//! report, the run surviving a host restart, and the blackboard's
//! optimistic-concurrency exit code.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};

use oximux_agents::coord::CoordStore;
use oximux_agents::schedule::ScheduleStore;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::team::TeamStore;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{
    AuthStore, Dispatcher, LaunchError, LocalScope, SessionLauncher,
};
use oximux_remote_local::{
    LocalClaim, LocalControlListener, generate_token,
};

fn bin(runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "10"]);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// The binary as a confined agent child sees it.
fn agent_bin(runtime_dir: &Path, label: &str, secret: &str) -> Command {
    let mut cmd = bin(runtime_dir);
    cmd.env(oximux_remote_local::SESSION_ENV_VAR, label);
    cmd.env(oximux_remote_local::SESSION_TOKEN_ENV_VAR, secret);
    cmd
}

fn json_stdout(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// What each launch was asked for: `(agent_id, model)`, in launch order.
type LaunchedWith = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

struct StubLauncher {
    registry: Arc<SessionRegistry>,
    counter: AtomicU32,
    /// The agent and model each launch was asked for, in order — the only place
    /// a test can see that `--role-agent`/`--role-model` reached the thing that
    /// starts processes, rather than only the board that echoes them back.
    agents: LaunchedWith,
}

#[async_trait::async_trait]
impl SessionLauncher for StubLauncher {
    async fn create(
        &self,
        _cwd: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, LaunchError> {
        let id = format!("role-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        self.registry.register(id.clone(), Arc::new(StubConnection::default()));
        self.agents
            .lock()
            .unwrap()
            .push((agent_id.map(str::to_string), model.map(str::to_string)));
        Ok(id)
    }
}

/// What a booted host hands back to the test.
struct Host {
    /// The credential minted for `sess-1`, as the host would hand it to that
    /// agent at spawn.
    agent_secret: String,
    teams: TeamStore,
    /// What the launcher was asked for, per launch: `(agent_id, model)`.
    launched_agents: LaunchedWith,
}

/// Boot a host on `runtime_dir` against the database at `db_path`, so a second
/// boot over the same path is a genuine restart.
fn boot(rt: &tokio::runtime::Runtime, runtime_dir: &Path, db_path: &Path) -> Host {
    let db = oximux_storage::open(db_path).expect("open db");
    let launched_agents: LaunchedWith = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    let teams = TeamStore::new(db.conn());
    let dispatcher = Arc::new(
        Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
            .with_launcher(Arc::new(StubLauncher {
                registry: registry.clone(),
                counter: AtomicU32::new(0),
                agents: launched_agents.clone(),
            }))
            .with_schedule_store(Arc::new(ScheduleStore::new(db.conn())))
            .with_team_store(Arc::new(teams.clone()))
            .with_coord_store(Arc::new(CoordStore::new(db.conn()))),
    );
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    let agent_secret = listener.grant_session("sess-1");
    rt.spawn(async move {
        loop {
            let Ok(pending) = listener.accept_pending().await else { return };
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let Ok((transport, claim)) = pending.authenticate().await else {
                    return;
                };
                let scope = match claim {
                    LocalClaim::Operator => LocalScope::Full,
                    LocalClaim::Session(id) => LocalScope::Session(id.into()),
                };
                dispatcher.serve_local(transport.as_ref(), scope).await;
            });
        }
    });
    Host { agent_secret, teams, launched_agents }
}

/// An agent arms a wake-up on itself with no `--session`, sees it listed, and
/// disarms it. The whole point of the verb: the host resolves the target from
/// the credential the agent proved.
#[test]
fn an_agent_arms_lists_and_disarms_its_own_heartbeat() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let armed = agent_bin(&runtime_dir, "sess-1", &host.agent_secret)
        .args([
            "--json",
            "heartbeat",
            "create",
            "check on the build",
            "--name",
            "ci",
            "--cron",
            "*/15 * * * *",
        ])
        .output()
        .expect("run");
    assert!(armed.status.success(), "stderr: {}", String::from_utf8_lossy(&armed.stderr));
    let data = json_stdout(&armed);
    assert_eq!(data["data"]["session_id"], "sess-1", "resolved from the credential");
    let id = data["data"]["id"].as_str().expect("an id").to_string();

    let listed = agent_bin(&runtime_dir, "sess-1", &host.agent_secret)
        .args(["--json", "heartbeat", "ls"])
        .output()
        .expect("run");
    assert!(listed.status.success());
    let rows = json_stdout(&listed);
    assert_eq!(rows["data"].as_array().expect("an array").len(), 1);

    let removed = agent_bin(&runtime_dir, "sess-1", &host.agent_secret)
        .args(["--json", "heartbeat", "rm", &id])
        .output()
        .expect("run");
    assert!(removed.status.success());
    let after = agent_bin(&runtime_dir, "sess-1", &host.agent_secret)
        .args(["--json", "heartbeat", "ls"])
        .output()
        .expect("run");
    assert!(json_stdout(&after)["data"].as_array().expect("an array").is_empty());
}

/// An unmappable cron expression is a usage error (exit 2) that names the
/// supported shapes — never a cadence silently rounded to something near it.
#[test]
fn an_unmappable_cron_is_a_usage_error_that_teaches_the_supported_set() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let out = agent_bin(&runtime_dir, "sess-1", &host.agent_secret)
        .args(["--json", "heartbeat", "create", "x", "--name", "n", "--cron", "0 9 1 * *"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "usage");
    let err = json_stdout(&out);
    assert_eq!(err["error"]["code"], "usage");
    let steps = err["error"]["next_steps"].as_array().expect("steps");
    assert!(
        steps.iter().any(|s| s.as_str().unwrap_or_default().contains("supported:")),
        "the refusal teaches the set: {steps:?}"
    );
}

/// The phase's integration criterion: a two-role run, both roles report, and
/// `team status` converges to closed.
#[test]
fn a_two_role_run_converges_as_its_roles_report() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let _host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let opened = bin(&runtime_dir)
        .args([
            "--json",
            "team",
            "run",
            "--name",
            "ship",
            "--role",
            "backend=port the API",
            "--role",
            "frontend=update the client",
            "--cwd",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("run");
    assert!(opened.status.success(), "stderr: {}", String::from_utf8_lossy(&opened.stderr));
    let run = json_stdout(&opened);
    let run_id = run["data"]["id"].as_str().expect("a run id").to_string();
    assert_eq!(run["data"]["roles"].as_array().expect("roles").len(), 2);
    assert_eq!(run["data"]["closed"], false, "nothing has reported yet");

    for (role, status) in [("backend", "done"), ("frontend", "failed")] {
        let reported = bin(&runtime_dir)
            .args([
                "--json", "team", "report", "--run", &run_id, "--role", role, "--status", status,
                "--summary", "did the thing",
            ])
            .output()
            .expect("run");
        assert!(
            reported.status.success(),
            "{role}: {}",
            String::from_utf8_lossy(&reported.stderr)
        );
    }

    let board = bin(&runtime_dir)
        .args(["--json", "team", "status", "--run", &run_id])
        .output()
        .expect("run");
    let board = json_stdout(&board);
    assert_eq!(board["data"]["closed"], true, "every role reported");
    let roles = board["data"]["roles"].as_array().expect("roles");
    assert_eq!(roles[0]["status"], "done");
    assert_eq!(roles[1]["status"], "failed", "a failed role still closes the run");
}

/// Restart survival — the differentiator. A run stays open across a host
/// restart: the role whose session survived keeps running, and only the one
/// whose session is gone is settled.
#[test]
fn a_team_run_survives_a_host_restart() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("oximux.db");
    let run_id = {
        let runtime_dir = dir.path().join("host-1");
        let _host = boot(&rt, &runtime_dir, &db_path);
        let opened = bin(&runtime_dir)
            .args([
                "--json", "team", "run", "--name", "ship", "--role", "backend=go", "--role",
                "frontend=go", "--cwd", dir.path().to_str().unwrap(),
            ])
            .output()
            .expect("run");
        assert!(opened.status.success(), "stderr: {}", String::from_utf8_lossy(&opened.stderr));
        json_stdout(&opened)["data"]["id"].as_str().expect("a run id").to_string()
    };

    // The restart: a second host over the same database, running the boot pass
    // serve does. `role-1` survived; `role-2` did not.
    let runtime_dir = dir.path().join("host-2");
    let host = boot(&rt, &runtime_dir, &db_path);
    let settled = host
        .teams
        .recover_open_roles(chrono::Local::now(), |id| id == "role-1")
        .expect("recover");
    assert_eq!(settled.len(), 1, "only the orphaned role is settled");

    let board = bin(&runtime_dir)
        .args(["--json", "team", "status", "--run", &run_id])
        .output()
        .expect("run");
    let board = json_stdout(&board);
    assert_eq!(board["data"]["closed"], false, "the surviving role holds the run open");
    let roles = board["data"]["roles"].as_array().expect("roles");
    assert_eq!(roles[0]["status"], "running", "re-associated, not lost");
    assert_eq!(roles[1]["status"], "failed");
    assert!(
        roles[1]["summary"].as_str().unwrap_or_default().contains("restarted"),
        "and it says why: {:?}",
        roles[1]["summary"]
    );
}

/// The phase's headline, driven through the actual binary: two roles, two
/// different agents, and a board that says which one worked each.
///
/// The launcher assertion is the load-bearing half — a board echoing what the
/// CLI sent would pass while every role was still being started on the
/// run-level default.
#[test]
fn per_role_agents_reach_the_launcher_and_show_on_the_board() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let opened = bin(&runtime_dir)
        .args([
            "--json",
            "team",
            "run",
            "--name",
            "sweep",
            "--role",
            "plan=survey the lexer",
            "--role",
            "impl=build it",
            "--role",
            "review=check it",
            "--role-agent",
            "plan=claude",
            "--role-agent",
            "impl=codex",
            "--role-model",
            "plan=opus",
            "--agent",
            "gemini",
            "--cwd",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("run");
    assert!(opened.status.success(), "stderr: {}", String::from_utf8_lossy(&opened.stderr));

    assert_eq!(
        host.launched_agents.lock().unwrap().clone(),
        vec![
            (Some("claude".to_string()), Some("opus".to_string())),
            (Some("codex".to_string()), None),
            // Named by no `--role-agent`, so it takes the run-level `--agent`.
            (Some("gemini".to_string()), None),
        ],
        "the launcher was asked for three different agents, and one model"
    );

    let run_id = json_stdout(&opened)["data"]["id"].as_str().expect("a run id").to_string();
    let board = bin(&runtime_dir)
        .args(["--json", "team", "status", "--run", &run_id])
        .output()
        .expect("run");
    let roles = json_stdout(&board)["data"]["roles"].as_array().expect("roles").clone();
    assert_eq!(roles[0]["agent_id"], "claude");
    assert_eq!(roles[1]["agent_id"], "codex");
    assert_eq!(roles[2]["agent_id"], "gemini");
    assert_eq!(roles[0]["model"], "opus");
    assert!(roles[1]["model"].is_null(), "and a role that asked for none has none");

    // And the human board says it too, since that is what a person reads.
    let human = bin(&runtime_dir)
        .args(["team", "status", "--run", &run_id])
        .output()
        .expect("run");
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("agent claude"), "{human}");
    assert!(human.contains("agent codex"), "{human}");
}

/// A `--role-agent` naming a role that does not exist is refused before
/// anything starts — exit 2, and nothing launched.
#[test]
fn a_role_agent_for_an_unknown_role_starts_nothing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let out = bin(&runtime_dir)
        .args([
            "team",
            "run",
            "--name",
            "sweep",
            "--role",
            "plan=survey",
            "--role-agent",
            "typo=claude",
            "--cwd",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "a usage error, not a host error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("typo"), "it names the offender: {stderr}");
    assert!(stderr.contains("plan"), "and the role that does exist: {stderr}");
    assert!(
        host.launched_agents.lock().unwrap().is_empty(),
        "and the run never started — the check is client-side, before the RPC"
    );
}

/// The blackboard's contract as a script sees it: a create-only write wins
/// once, and the loser exits 5 with the current value to merge from.
#[test]
fn a_lost_conditional_write_exits_denied_with_the_current_value() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let _host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    // Absent reads as unset, at exit 0 — "nobody has claimed this" is an answer.
    let empty = bin(&runtime_dir).args(["--json", "state", "get", "claim"]).output().expect("run");
    assert!(empty.status.success());
    assert!(json_stdout(&empty)["data"]["value"].is_null());

    let won = bin(&runtime_dir)
        .args(["--json", "state", "set", "claim", "\"backend\"", "--if-version", "0"])
        .output()
        .expect("run");
    assert!(won.status.success(), "stderr: {}", String::from_utf8_lossy(&won.stderr));
    assert_eq!(json_stdout(&won)["data"]["version"], 1);

    let lost = bin(&runtime_dir)
        .args(["--json", "state", "set", "claim", "\"frontend\"", "--if-version", "0"])
        .output()
        .expect("run");
    assert_eq!(lost.status.code(), Some(5), "a lost race is denied, not a generic error");
    let err = json_stdout(&lost);
    assert_eq!(err["error"]["code"], "version-conflict");
    let steps = err["error"]["next_steps"].as_array().expect("steps");
    assert!(
        steps.iter().any(|s| s.as_str().unwrap_or_default().contains("backend")),
        "the current value rides along: {steps:?}"
    );

    // The winner's value stands.
    let read = bin(&runtime_dir).args(["--json", "state", "get", "claim"]).output().expect("run");
    assert_eq!(json_stdout(&read)["data"]["value"], "backend");
}

/// A value that is not JSON never leaves the client: a typo costs a usage
/// error, not a round trip.
#[test]
fn a_non_json_state_value_is_refused_locally() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let _host = boot(&rt, &runtime_dir, &dir.path().join("oximux.db"));

    let out = bin(&runtime_dir)
        .args(["--json", "state", "set", "k", "claimed"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "usage");
    assert_eq!(json_stdout(&out)["error"]["code"], "usage");
}
