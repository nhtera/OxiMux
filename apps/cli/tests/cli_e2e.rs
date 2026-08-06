//! The compiled binary against a live host: a real dispatcher behind a real
//! owner-only socket, driven exactly as a user or an agent would drive it —
//! argv in, JSON out, exit codes as the contract says.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{
    AuthStore, Dispatcher, LaunchError, LocalScope, SessionLauncher, WorktreeError,
    WorktreeService,
};
use oximux_remote_local::{
    LocalClaim, LocalControlListener, generate_token, token_path, write_token_file,
};
use oximux_remote_proto::messages::WorktreeWire;

fn bin(runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "10"]);
    // The test runner's own environment must not leak a credential in.
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// The binary as an agent child sees it: session id AND the per-session
/// secret the host minted for that agent.
fn agent_bin(runtime_dir: &Path, session_id: &str, secret: &str) -> Command {
    let mut cmd = bin(runtime_dir);
    cmd.env(oximux_remote_local::SESSION_ENV_VAR, session_id);
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

/// A launcher that registers stub-backed sessions on demand, so `run` gets a
/// real session id it can immediately drive.
struct StubLauncher {
    registry: Arc<SessionRegistry>,
    counter: AtomicU32,
}

#[async_trait::async_trait]
impl SessionLauncher for StubLauncher {
    async fn create(&self, _cwd: &str, _agent_id: Option<&str>) -> Result<String, LaunchError> {
        let id = format!("run-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        self.registry.register(id.clone(), Arc::new(StubConnection::default()));
        Ok(id)
    }
}

/// An in-memory worktree service, enough for the scope and plumbing tests.
struct StubWorktrees;

#[async_trait::async_trait]
impl WorktreeService for StubWorktrees {
    async fn create(&self, project_path: &str, slug: &str)
    -> Result<WorktreeWire, WorktreeError> {
        Ok(WorktreeWire {
            id: format!("wt-{slug}"),
            project_path: project_path.into(),
            name: slug.into(),
            slug: slug.into(),
            branch: format!("oximux/{slug}"),
            path: format!("/stub/worktrees/{slug}"),
        })
    }
    async fn list(&self, _project_path: Option<&str>) -> Result<Vec<WorktreeWire>, WorktreeError> {
        Ok(vec![])
    }
    async fn remove(&self, _id: &str) -> Result<(), WorktreeError> {
        Ok(())
    }
}

/// A host with two sessions, serving on a fresh runtime dir until dropped.
/// Returns the per-session secret minted for `sess-1`, as the host would hand
/// it to that agent's process at spawn.
fn serve_host(rt: &tokio::runtime::Runtime, runtime_dir: &Path) -> String {
    let (secret, _registry) = serve_host_with_registry(rt, runtime_dir);
    secret
}

/// Like [`serve_host`], but hands back the registry so a test can ingest
/// events and publish transcripts into the live sessions, and installs the
/// stub launcher + worktree service so `run` and `worktree` have a host side.
fn serve_host_with_registry(
    rt: &tokio::runtime::Runtime,
    runtime_dir: &Path,
) -> (String, Arc<SessionRegistry>) {
    let registry = Arc::new(SessionRegistry::new());
    for id in ["sess-1", "sess-2"] {
        registry.register(id.into(), Arc::new(StubConnection::default()));
    }
    let dispatcher = Arc::new(
        Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
            .with_launcher(Arc::new(StubLauncher {
                registry: registry.clone(),
                counter: AtomicU32::new(0),
            }))
            .with_worktrees(Arc::new(StubWorktrees)),
    );
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    let agent_secret = listener.grant_session("sess-1");
    rt.spawn(async move {
        loop {
            // Authenticate inside the per-connection task, as the desktop does:
            // a stalled handshake must not hold the accept path.
            let pending = match listener.accept_pending().await {
                Ok(pending) => pending,
                // Bail rather than retry. A `continue` here spins at full speed
                // on any error that does not clear (EMFILE, say, which this
                // suite can reach by spawning the binary repeatedly), starving
                // the very subprocesses the assertions are waiting on and
                // surfacing as an unrelated timeout.
                Err(err) => {
                    eprintln!("test host: accept failed, stopping: {err}");
                    return;
                }
            };
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
    (agent_secret, registry)
}

/// `status` and `ls --json` against a live host: exit 0, honest counts —
/// and the same invocations from an agent-scoped environment see exactly one
/// session and are refused everything session-less.
#[test]
fn status_ls_and_scope_against_a_live_host() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let agent_secret = serve_host(&rt, &runtime_dir);

    // Operator: everything visible.
    let out = bin(&runtime_dir).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["sessions"]["total"], 2);

    let out = bin(&runtime_dir).args(["--json", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json_stdout(&out);
    assert_eq!(v["data"].as_array().unwrap().len(), 2);

    // Agent-scoped: holding the per-session secret narrows the same binary…
    let out = agent_bin(&runtime_dir, "sess-1", &agent_secret)
        .args(["--json", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "a scoped caller must not enumerate other sessions");
    assert_eq!(rows[0]["session_id"], "sess-1");

    // …and is refused the session-less surface, with the denied exit code.
    let out = agent_bin(&runtime_dir, "sess-1", &agent_secret)
        .args(["--json", "projects", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "denied is exit 5");
    let v = json_stdout(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "denied");
    assert!(!v["error"]["next_steps"].as_array().unwrap().is_empty());

    // The escalation an injected agent would attempt through the protocol:
    // present its own secret while naming a session it was not given. Denied —
    // the label is bound to a secret it does not hold.
    let out = agent_bin(&runtime_dir, "sess-2", &agent_secret)
        .args(["--json", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "wrong-session credential is denied");

    // An agent whose per-session secret failed to arrive does NOT quietly fall
    // back to the operator token file: it is refused, rather than running the
    // whole session with the operator's authority.
    let out = bin(&runtime_dir)
        .args(["--json", "ls"])
        .env(oximux_remote_local::SESSION_ENV_VAR, "sess-1")
        .output()
        .unwrap();
    // Exit 5, not 3: this is an access failure, and the host is running fine.
    // Reporting it as "unreachable" told a wrapper to keep retrying something
    // no retry can fix.
    assert_eq!(out.status.code(), Some(5), "no fallback to operator scope");
    let v = json_stdout(&out);
    assert_eq!(v["error"]["code"], "denied");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains(oximux_remote_local::SESSION_TOKEN_ENV_VAR),
        "the error names the missing credential: {v}"
    );
}

/// No host at the dir → exit 3 with actionable next steps; a rotated (stale)
/// token → exit 5. The two factors fail distinguishably.
#[test]
fn unreachable_is_3_and_stale_token_is_5() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // Nothing ever served here.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = bin(&empty).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "unreachable is exit 3");
    let v = json_stdout(&out);
    assert_eq!(v["error"]["code"], "unreachable");
    assert!(!v["error"]["next_steps"].as_array().unwrap().is_empty());

    // A live host whose on-disk token we rotate out from under the CLI.
    let runtime_dir = dir.path().join("host");
    serve_host(&rt, &runtime_dir);
    write_token_file(&token_path(&runtime_dir), &generate_token()).unwrap();
    let out = bin(&runtime_dir).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(5), "token mismatch is exit 5");
    assert_eq!(json_stdout(&out)["error"]["code"], "denied");
}

/// The offline verbs cost nothing: no host anywhere, still exit 0.
#[test]
fn offline_verbs_need_no_host() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(dir.path()).arg("version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));

    let out = bin(dir.path()).arg("agent-context").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"]["name"], "oximux");

    let out = bin(dir.path()).arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));

    // A typo is a usage error: exit 2, from clap itself.
    let out = bin(dir.path()).arg("no-such-verb").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// The phase-3 scripted loop, end to end against the compiled binary:
/// `run --bg` → `wait --until needs-approval` → `permit ls`/`allow` →
/// `wait --until done` → `transcript --json`. The host side is a stub agent
/// whose events the test injects at each step, so every wait resolves from
/// state rather than timing.
#[test]
fn scripted_loop_run_wait_permit_transcript() {
    use oximux_agent_core::thread::ThreadEvent;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let (_secret, registry) = serve_host_with_registry(&rt, &runtime_dir);

    // run --bg: a new session, id returned, prompt accepted (the stub records
    // it and the handle synthesizes the user bubble).
    let out = bin(&runtime_dir)
        .args(["--json", "run", "--bg", "approve the tool, then finish"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    let session = v["data"]["session_id"].as_str().unwrap().to_string();
    assert!(session.starts_with("run-"), "launcher-minted id, got {session}");

    // The agent asks for permission.
    let handle = registry.get(&session).unwrap();
    handle.ingest(ThreadEvent::PermissionRequested {
        request_id: "req-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
        description: "Run cargo test".into(),
        suggestions: vec![],
        kind: oximux_agent_core::thread::PermissionKind::Tool,
    });

    let out = bin(&runtime_dir)
        .args(["--json", "wait", &session, "--until", "needs-approval"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // The pending request is listable, then decidable.
    let out = bin(&runtime_dir).args(["--json", "permit", "ls", &session]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json_stdout(&out);
    assert_eq!(v["data"][0]["request_id"], "req-1");
    assert_eq!(v["data"][0]["kind"], "permission");

    let out = bin(&runtime_dir).args(["--json", "permit", "allow", &session]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(json_stdout(&out)["data"]["decided"], true);

    // The turn completes; `wait --until done` resolves from the backlog.
    handle.ingest(ThreadEvent::TurnEnded {
        result: None,
        usage: None,
        is_error: false,
        turn_diff: None,
    });
    let out = bin(&runtime_dir)
        .args(["--json", "wait", &session, "--until", "done"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // The published fold comes back whole over the paginated verb.
    let entries = serde_json::json!([
        {"User": {"text": "approve the tool, then finish", "images": []}},
        {"Assistant": {"text": "Done.", "thinking": ""}},
    ]);
    handle.publish_transcript(entries.to_string(), Some("stub-model".into()));
    let out = bin(&runtime_dir).args(["--json", "transcript", &session]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["data"]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(v["data"]["model"], "stub-model");
}

/// A subscriber pushed past the retained backlog reconnects, the gap is
/// detected, state resyncs from the transcript, and the marker is printed —
/// no silent loss. The session's backlog is capped tiny so the gap is real.
#[test]
fn lagged_attach_resyncs_from_the_transcript_with_a_marker() {
    use std::io::BufRead as _;
    use oximux_agent_core::thread::ThreadEvent;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let (_secret, registry) = serve_host_with_registry(&rt, &runtime_dir);

    // A session whose replay ring holds only the last 8 events.
    let handle = registry.register_with_caps(
        "lag".into(),
        Arc::new(StubConnection::default()),
        8,
        64,
    );
    for i in 0..50 {
        handle.ingest(ThreadEvent::AssistantText(format!("line {i}")));
    }
    handle.publish_transcript(
        serde_json::json!([{ "Assistant": {"text": "the fold", "thinking": ""} }]).to_string(),
        None,
    );

    // Attach from seq 0: the ring starts far later, so the CLI must detect the
    // gap and resync from the transcript, printing the marker.
    let mut child = bin(&runtime_dir)
        .args(["--json", "attach", "lag", "--from", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut resynced: Option<serde_json::Value> = None;
    let mut streamed_after_marker = false;
    while std::time::Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(std::time::Duration::from_millis(200)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if v["resynced"] == true {
            resynced = Some(v);
        } else if resynced.is_some() && v["seq"].is_u64() {
            // Events keep flowing after the marker: the retained tail streams.
            streamed_after_marker = true;
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let resynced = resynced.expect("the resync marker must be printed — silent loss is a bug");
    let elapsed = resynced["events_elapsed"].as_u64().unwrap();
    assert!(elapsed >= 40, "the marker reports the lost span, got {elapsed}");
    assert!(streamed_after_marker, "the retained events still stream after the resync");
}

/// A transcript larger than one 16 MB transport frame arrives whole through
/// the CLI's paging loop — the exact failure the paginated verb exists for.
#[test]
fn oversize_transcript_arrives_whole_via_paging() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let (_secret, registry) = serve_host_with_registry(&rt, &runtime_dir);

    let handle = registry.get("sess-1").unwrap();
    let big: Vec<serde_json::Value> = (0..2000)
        .map(|i| serde_json::json!({ "Assistant": { "text": format!("{i}:{}", "x".repeat(10_000)), "thinking": "" } }))
        .collect();
    let entries_json = serde_json::to_string(&big).unwrap();
    assert!(entries_json.len() > 16 * 1024 * 1024, "fixture must exceed one frame");
    handle.publish_transcript(entries_json, None);

    let out = bin(&runtime_dir).args(["--json", "transcript", "sess-1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["data"]["entries"].as_array().unwrap().len(), 2000, "no entry lost");
    assert!(v["data"]["pages"].as_u64().unwrap() > 1, "the fetch really paged");
}

/// The worktree verbs honour the scope split end to end: the operator reaches
/// them, an agent-scoped invocation is denied with exit 5.
#[test]
fn worktree_verbs_follow_the_scope_split() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let (agent_secret, _registry) = serve_host_with_registry(&rt, &runtime_dir);

    // Operator: served (the stub returns an empty list).
    let out = bin(&runtime_dir).args(["--json", "worktree", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let out = bin(&runtime_dir)
        .args(["--json", "worktree", "create", "feat-x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["data"]["branch"], "oximux/feat-x");

    // Agent-scoped: every worktree verb is denied, exit 5.
    for args in [vec!["worktree", "ls"], vec!["worktree", "create", "esc"], vec!["worktree", "rm", "wt-x"]] {
        let mut argv = vec!["--json"];
        argv.extend(args.iter());
        let out = agent_bin(&runtime_dir, "sess-1", &agent_secret).args(&argv).output().unwrap();
        assert_eq!(out.status.code(), Some(5), "agent-scoped {args:?} must be denied");
        assert_eq!(json_stdout(&out)["error"]["code"], "denied");
    }
}

/// A provider listing exactly one project — enough for the client-side
/// project-root walk to have something to match against.
struct StubProjects(String);

#[async_trait::async_trait]
impl oximux_remote_host::ProjectProvider for StubProjects {
    async fn projects(&self) -> Vec<oximux_remote_proto::messages::ProjectSummaryWire> {
        vec![oximux_remote_proto::messages::ProjectSummaryWire {
            name: "proj".into(),
            path: self.0.clone(),
        }]
    }
}

/// A host that lists one project and echoes worktree creates, serving on a
/// fresh runtime dir until the runtime drops.
fn serve_worktree_host(rt: &tokio::runtime::Runtime, runtime_dir: &Path, project_root: &Path) {
    let registry = Arc::new(SessionRegistry::new());
    let dispatcher = Arc::new(
        Dispatcher::new(registry, Arc::new(AuthStore::new()))
            .with_worktrees(Arc::new(StubWorktrees))
            .with_projects(Arc::new(StubProjects(project_root.to_string_lossy().into_owned()))),
    );
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    rt.spawn(async move {
        loop {
            let pending = match listener.accept_pending().await {
                Ok(pending) => pending,
                Err(err) => {
                    eprintln!("test host: accept failed, stopping: {err}");
                    return;
                }
            };
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
}

/// Running worktree verbs from inside a project's subdirectory names the
/// project root the host actually knows — via the client-side ancestor walk
/// against `ListProjects` — while a directory outside every listed project
/// still passes through verbatim for the host's own exact-match validation
/// (the stub echoes `project_path` back, which is what makes each case
/// observable).
#[test]
fn worktree_create_resolves_the_project_from_a_subdirectory() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    // Canonicalized, as the desktop's own project rows are — macOS tempdirs
    // live behind the /var → /private/var symlink and the invoking process's
    // cwd always reads back physical.
    let project_root = {
        let p = dir.path().join("proj");
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    };
    let subdir = project_root.join("src").join("deep");
    std::fs::create_dir_all(&subdir).unwrap();
    serve_worktree_host(&rt, &runtime_dir, &project_root);

    // From the subdirectory, no --project: the walk finds the root.
    let out = bin(&runtime_dir)
        .args(["--json", "worktree", "create", "feat-sub"])
        .current_dir(&subdir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        json_stdout(&out)["data"]["project_path"],
        project_root.to_string_lossy().as_ref(),
        "the subdirectory resolved to its project root"
    );

    // The same walk serves an explicit --project pointing at a subdirectory,
    // and `run --worktree` invoked from one.
    let out = bin(&runtime_dir)
        .args(["--json", "worktree", "create", "feat-flag"])
        .args(["--project", subdir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        json_stdout(&out)["data"]["project_path"],
        project_root.to_string_lossy().as_ref()
    );

    // A directory outside every listed project passes through verbatim — the
    // host's exact-match validation stays the deciding boundary.
    let elsewhere = dir.path().join("elsewhere").canonicalize().unwrap_or_else(|_| {
        std::fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        dir.path().join("elsewhere").canonicalize().unwrap()
    });
    let out = bin(&runtime_dir)
        .args(["--json", "worktree", "create", "feat-out"])
        .args(["--project", elsewhere.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "the stub host accepts any root");
    assert_eq!(
        json_stdout(&out)["data"]["project_path"],
        elsewhere.to_string_lossy().as_ref(),
        "an unlisted directory is not rewritten"
    );
}

/// A firer that registers a stub-backed session per fire — the ticker's whole
/// fire/record path without spawning any agent CLI.
struct StubFirer {
    registry: Arc<SessionRegistry>,
    counter: AtomicU32,
}

#[async_trait::async_trait]
impl oximux_agents::schedule::ScheduleFirer for StubFirer {
    async fn fire(
        &self,
        _schedule: &oximux_agents::schedule::Schedule,
        _target: &oximux_agents::schedule::ScheduleTarget,
    ) -> oximux_agents::schedule::FireOutcome {
        let id = format!("fire-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1);
        self.registry.register(id.clone(), Arc::new(StubConnection::default()));
        oximux_agents::schedule::FireOutcome::Completed { session_id: Some(id) }
    }
}

/// A host with a real schedule store — and, when `with_runner`, a real ticker
/// behind the run-now RPC (its fires land in the shared registry as stub
/// sessions).
fn serve_schedule_host(
    rt: &tokio::runtime::Runtime,
    runtime_dir: &Path,
    with_runner: bool,
) -> oximux_agents::schedule::ScheduleStore {
    let registry = Arc::new(SessionRegistry::new());
    let db = oximux_storage::db::open_memory().unwrap();
    let store = oximux_agents::schedule::ScheduleStore::new(db.conn());
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut dispatcher = Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
        .with_schedule_store(Arc::new(store.clone()))
        .with_schedule_events(events.clone());
    if with_runner {
        let ticker = Arc::new(
            oximux_agents::schedule::Ticker::new(
                store.clone(),
                Arc::new(StubFirer { registry: registry.clone(), counter: AtomicU32::new(0) }),
            )
            .with_recorded_hook(Arc::new(move |run| {
                let _ = events.send(oximux_remote_host::schedule_run_to_wire(run));
            })),
        );
        dispatcher = dispatcher.with_schedule_runner(Arc::new(
            oximux_remote_host::TickerRunner(ticker),
        ));
    }
    let dispatcher = Arc::new(dispatcher);
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    rt.spawn(async move {
        loop {
            let pending = match listener.accept_pending().await {
                Ok(pending) => pending,
                Err(err) => {
                    eprintln!("test host: accept failed, stopping: {err}");
                    return;
                }
            };
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
    store
}

/// The whole schedule lifecycle through the compiled binary — and the run-once
/// contract: the manual fire is recorded, names its session, and leaves
/// `next_fire_at` exactly where it was.
#[test]
fn schedule_lifecycle_and_run_once_keeps_cadence() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let _store = serve_schedule_host(&rt, &runtime_dir, true);

    let out = bin(&runtime_dir)
        .args([
            "--json", "schedule", "create", "check the builds", "--name", "nightly", "--cwd",
            "/tmp", "--daily", "09:00",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let created = json_stdout(&out);
    let id = created["data"]["id"].as_str().expect("an id").to_string();
    let armed = created["data"]["next_fire_at"].as_str().expect("a next fire").to_string();

    // run-once: recorded ok, fired into a session, cadence untouched.
    let out = bin(&runtime_dir).args(["--json", "schedule", "run-once", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let run = json_stdout(&out);
    assert_eq!(run["data"]["outcome"], "ok");
    assert_eq!(run["data"]["session_id"], "fire-1");

    let out = bin(&runtime_dir).args(["--json", "schedule", "ls"]).output().unwrap();
    let rows = json_stdout(&out);
    assert_eq!(rows["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(rows["data"][0]["next_fire_at"], armed.as_str(), "run-once must not advance cadence");

    let out = bin(&runtime_dir).args(["--json", "schedule", "logs", &id]).output().unwrap();
    let logs = json_stdout(&out);
    assert_eq!(logs["data"].as_array().map(Vec::len), Some(1), "the manual run is history");
    assert_eq!(logs["data"][0]["outcome"], "ok");

    // Pause / resume round-trip.
    let out = bin(&runtime_dir).args(["--json", "schedule", "pause", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let out = bin(&runtime_dir).args(["--json", "schedule", "ls"]).output().unwrap();
    assert_eq!(json_stdout(&out)["data"][0]["enabled"], false);
    let out = bin(&runtime_dir).args(["--json", "schedule", "resume", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let out = bin(&runtime_dir).args(["--json", "schedule", "ls"]).output().unwrap();
    assert_eq!(json_stdout(&out)["data"][0]["enabled"], true);

    let out = bin(&runtime_dir).args(["--json", "schedule", "rm", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let out = bin(&runtime_dir).args(["--json", "schedule", "ls"]).output().unwrap();
    assert_eq!(json_stdout(&out)["data"].as_array().map(Vec::len), Some(0));
}

/// A host that serves schedule reads/writes but does not own the ticker
/// answers run-once with `Unsupported` — the schedules fire from the process
/// that holds the lock, and this host says so instead of racing it.
#[test]
fn run_once_without_the_ticker_is_unsupported() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let _store = serve_schedule_host(&rt, &runtime_dir, false);

    let out = bin(&runtime_dir)
        .args([
            "--json", "schedule", "create", "check the builds", "--name", "nightly", "--cwd",
            "/tmp", "--every", "30",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "creates still work here");
    let id = json_stdout(&out)["data"]["id"].as_str().unwrap().to_string();

    let out = bin(&runtime_dir).args(["--json", "schedule", "run-once", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_stdout(&out)["error"]["code"], "unsupported");
}
