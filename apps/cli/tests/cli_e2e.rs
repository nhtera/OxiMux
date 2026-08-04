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
