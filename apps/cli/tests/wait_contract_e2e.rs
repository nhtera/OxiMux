//! `oximux wait`'s exit codes and error codes, pinned against the compiled
//! binary and a live host before its source of truth changes (the v37 health
//! frame). Scripts branch on these: 0 reached, 1 not idle, 4 timeout or
//! stalled (told apart by `error.code`), 8 no such session.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::{PermissionKind, StubConnection, ThreadEvent};
use oximux_agents::health::{HealthInputs, Process};
use oximux_agents::lineage::LineageStore;
use oximux_remote_host::{AuthStore, Dispatcher, HostConfig, HostCore, HostCoreDeps, LocalScope, PriorOwner};
use oximux_remote_local::{LocalClaim, LocalControlListener, generate_token};

fn bin(runtime_dir: &Path, timeout_secs: u64) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", &timeout_secs.to_string(), "--json"]);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// Exit code and `error.code` (absent on success).
fn outcome(out: &Output) -> (Option<i32>, Option<String>) {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("stdout is not JSON ({e}): {}\nstderr: {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    });
    (out.status.code(), v["error"]["code"].as_str().map(str::to_string))
}

/// A host serving one stub session, `wait-1`, until the runtime drops.
fn serve(rt: &tokio::runtime::Runtime, runtime_dir: &Path) -> Arc<SessionHandle> {
    serve_on(rt, runtime_dir, false)
}

/// [`serve`], optionally with a host core: the desktop's shape, which pushes
/// session health to a v37 subscriber.
fn serve_on(rt: &tokio::runtime::Runtime, runtime_dir: &Path, with_core: bool) -> Arc<SessionHandle> {
    let registry = Arc::new(SessionRegistry::new());
    registry.register("wait-1".into(), Arc::new(StubConnection::default()));
    let handle = registry.get("wait-1").unwrap();
    let mut dispatcher = Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()));
    if with_core {
        let db = oximux_storage::db::open_memory().unwrap();
        let core = HostCore::new(HostCoreDeps {
            rt: rt.handle().clone(),
            lineage: LineageStore::new(db.conn()),
            registry,
            config: HostConfig::default(),
        });
        core.start(PriorOwner::None);
        dispatcher = dispatcher.with_host_core(core);
    }
    let dispatcher = Arc::new(dispatcher);
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    rt.spawn(async move {
        while let Ok(pending) = listener.accept_pending().await {
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let Ok((transport, claim)) = pending.authenticate().await else { return };
                let scope = match claim {
                    LocalClaim::Operator => LocalScope::Full,
                    LocalClaim::Session(id) => LocalScope::Session(id.into()),
                    LocalClaim::Mcp => LocalScope::Mcp,
                };
                dispatcher.serve_local(transport.as_ref(), scope).await;
            });
        }
    });
    handle
}

fn prompt(handle: &SessionHandle) {
    handle.ingest(ThreadEvent::UserMessage { text: "go".into(), images: vec![] });
}

fn ask_permission(handle: &SessionHandle) {
    handle.ingest(ThreadEvent::PermissionRequested {
        request_id: "req-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
        description: "Run cargo test".into(),
        suggestions: vec![],
        kind: PermissionKind::Tool,
    });
}

fn end_turn(handle: &SessionHandle) {
    handle.ingest(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
}

#[test]
fn a_state_already_reached_exits_zero() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(&rt, dir.path());

    // Nothing has run: done and idle both hold.
    for until in ["done", "idle"] {
        let out = bin(dir.path(), 10).args(["wait", "wait-1", "--until", until]).output().unwrap();
        assert_eq!(outcome(&out), (Some(0), None), "--until {until}");
    }
    prompt(&handle);
    ask_permission(&handle);
    let out = bin(dir.path(), 10).args(["wait", "wait-1", "--until", "needs-approval"]).output().unwrap();
    assert_eq!(outcome(&out), (Some(0), None));
}

#[test]
fn a_running_turn_times_out_with_four_and_names_a_pending_decision() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(&rt, dir.path());
    prompt(&handle);

    let out = bin(dir.path(), 1).args(["wait", "wait-1", "--until", "done"]).output().unwrap();
    assert_eq!(outcome(&out), (Some(4), Some("timeout".into())));

    // Same exit and code; the message says only a decision ends it.
    ask_permission(&handle);
    let out = bin(dir.path(), 1).args(["wait", "wait-1", "--until", "done"]).output().unwrap();
    assert_eq!(outcome(&out), (Some(4), Some("timeout".into())));
    assert!(String::from_utf8_lossy(&out.stdout).contains("awaiting a decision"));
}

#[test]
fn a_silent_turn_is_stalled_not_timed_out() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(&rt, dir.path());
    prompt(&handle);

    let out = bin(dir.path(), 20)
        .args(["wait", "wait-1", "--until", "done", "--stalled-after", "1"])
        .output()
        .unwrap();
    assert_eq!(outcome(&out), (Some(4), Some("stalled".into())));
}

#[test]
fn a_turn_that_ends_on_a_pending_decision_is_not_idle() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(&rt, dir.path());
    prompt(&handle);
    ask_permission(&handle);

    let mut child = bin(dir.path(), 20)
        .args(["wait", "wait-1", "--until", "idle"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // End the turn until the waiter sees it: one landing before it subscribes
    // is already in its scan, so it would keep waiting for the next.
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(15), "wait never returned");
        end_turn(&handle);
        std::thread::sleep(Duration::from_millis(300));
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(outcome(&out), (Some(1), Some("not-idle".into())));
}

#[test]
fn a_prefix_matching_no_session_exits_eight() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let _handle = serve(&rt, dir.path());

    let out = bin(dir.path(), 10).args(["wait", "nope", "--until", "done"]).output().unwrap();
    assert_eq!(outcome(&out), (Some(8), Some("target-not-found".into())));
}

/// A wait against a host that pushes health (the desktop's shape) still ends
/// on the turn, with frames streaming throughout. A smoke check: whether a
/// push lands inside the one mid-stream call is timing, so the routing itself
/// is pinned by `client::tests::a_health_frame_is_a_push`.
#[test]
fn health_pushes_do_not_disturb_a_wait() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let handle = serve_on(&rt, dir.path(), true);
    prompt(&handle);

    // Flip what a reader sees, so every publish is pushed.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flipper = {
        let (handle, stop) = (handle.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut turn = false;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                turn = !turn;
                let now = oximux_agents::session_health::now_ms();
                handle.publish_health(HealthInputs { turn_active: turn, process: Process::Live, last_activity_at: now, ..Default::default() });
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let mut child = bin(dir.path(), 20).args(["wait", "wait-1", "--until", "idle"]).stdout(Stdio::piped()).spawn().unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(15), "wait never returned");
        if started.elapsed() > Duration::from_millis(800) {
            end_turn(&handle);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    flipper.join().unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(outcome(&out), (Some(0), None), "stderr-free success expected");
}
