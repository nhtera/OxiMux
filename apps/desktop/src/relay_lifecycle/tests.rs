//! The lifecycle's decisions, against a real daemon booted in-process. Only the
//! paths that decide NOT to start a daemon are driven here: starting one spawns
//! the `oximux-relay` binary through the supervisor, which a unit test cannot.
//! The success path is exercised by the live drill (phase 8).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::RelayClient;
use oximux_relay_supervisor::RelaySupervisor;
use tempfile::TempDir;

use super::*;

struct Fixture {
    lifecycle: Arc<RelayLifecycle>,
    events: UnboundedReceiver<RelayLifecycleEvent>,
    _dir: TempDir,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    // On the supervisor's own endpoint, so it counts as the daemon that answers
    // there — one with no pid record, which a stop must leave alone.
    let socket = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf()).socket_path();
    let token_file = dir.path().join("relay-test.token");
    let token = "lifecycle-test-token";
    std::fs::write(&token_file, token).expect("write token");
    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    tokio::spawn(async move {
        let _ = run_server(cfg).await;
    });
    let mut client = None;
    for _ in 0..200 {
        if let Ok(c) = RelayClient::connect(&socket, token).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client = Arc::new(client.expect("relay socket never came up"));
    let repo = oximux_storage::PaneRelayIdRepo::new(oximux_storage::open_memory().expect("db"));
    let supervisor = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
    let lifecycle =
        RelayLifecycle::new(supervisor, repo, tokio::runtime::Handle::current(), client);
    let events = lifecycle.take_events().expect("fresh lifecycle has its events");
    Fixture { lifecycle, events, _dir: dir }
}

fn no_event(events: &mut UnboundedReceiver<RelayLifecycleEvent>) -> bool {
    events.try_recv().is_err()
}

// A second respawn for a death that was already handled finds the epoch moved
// on: it touches nothing and tells no one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_for_a_replaced_session_is_a_no_op() {
    let mut f = fixture().await;
    let dead = f.lifecycle.current_session();
    // What a successful respawn leaves behind: a new current session.
    *f.lifecycle.current_session.write().unwrap() = "the-replacement".into();

    let outcome = f.lifecycle.respawn(RespawnReason::Crash, dead).await;

    assert_eq!(outcome, RespawnOutcome::AlreadyRespawned);
    assert_eq!(f.lifecycle.current_session(), "the-replacement");
    assert!(no_event(&mut f.events), "a no-op respawn emits nothing");
}

// The heartbeat stands down for a death a manual restart has claimed, and only
// for that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_stands_down_for_an_expected_death() {
    let f = fixture().await;
    let session = f.lifecycle.current_session();
    assert!(f.lifecycle.claim_death(&session), "an unclaimed death is a crash");

    *f.lifecycle.expected_death.lock().unwrap() = Some(session.clone());
    assert!(!f.lifecycle.claim_death(&session), "a claimed death is not");
    assert!(f.lifecycle.claim_death("some-other-session"));
}

// A crash respawn the throttle refuses reports "keeps stopping" and leaves the
// epoch alone — the daemon stays down rather than looping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_crash_respawn_reports_keeps_stopping() {
    let mut f = fixture().await;
    {
        let mut throttle = f.lifecycle.crash_throttle.lock().unwrap();
        for _ in 0..CRASH_LIMIT {
            assert!(throttle.admit(Instant::now()));
        }
    }
    let session = f.lifecycle.current_session();

    let outcome = f.lifecycle.respawn(RespawnReason::Crash, session.clone()).await;

    assert_eq!(outcome, RespawnOutcome::Failed(RespawnFailure::KeepsStopping));
    assert_eq!(f.lifecycle.current_session(), session);
    match f.events.next().await {
        Some(RelayLifecycleEvent::RespawnFailed { failure, reason }) => {
            assert_eq!(failure, RespawnFailure::KeepsStopping);
            assert_eq!(reason, RespawnReason::Crash);
        }
        other => panic!("expected RespawnFailed, got {other:?}"),
    }
}

#[test]
fn crash_throttle_refuses_the_fourth_crash_in_a_minute() {
    let mut throttle = CrashThrottle::default();
    let start = Instant::now();
    assert!(throttle.admit(start));
    assert!(throttle.admit(start + Duration::from_secs(10)));
    assert!(throttle.admit(start + Duration::from_secs(20)));
    assert!(!throttle.admit(start + Duration::from_secs(30)), "4th within 60s");
    // The first crash ages out of the window.
    assert!(throttle.admit(start + Duration::from_secs(61)));
}

#[test]
fn a_manual_restart_resets_the_crash_window() {
    let mut throttle = CrashThrottle::default();
    let now = Instant::now();
    for _ in 0..CRASH_LIMIT {
        assert!(throttle.admit(now));
    }
    assert!(!throttle.admit(now));
    throttle.reset();
    assert!(throttle.admit(now));
}

// Two restarts asked for together are one restart: one run, one result for
// both. (The fixture's daemon answers but has no pid record, so the run fails
// fast at the stop — which is enough to count runs without starting a daemon
// binary.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_restarts_coalesce_into_one() {
    let mut f = fixture().await;
    // Held so the first run cannot finish before the second request arrives.
    let held = f.lifecycle.respawn_lock.lock().await;
    let first = f.lifecycle.restart();
    let second = f.lifecycle.restart();
    drop(held);

    let (a, b) = futures::join!(first, second);

    assert_eq!(a, b);
    assert_eq!(a, Err(RestartError::Stop(oximux_relay_supervisor::StopError::NoPidRecord)));
    assert_eq!(f.lifecycle.restart_runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(f.events.next().await, Some(RelayLifecycleEvent::RestartFailed { .. })));
    assert_eq!(*f.lifecycle.expected_death.lock().unwrap(), None, "the claim is released");

    // Finished restarts do not linger: the next request runs again.
    let _ = f.lifecycle.restart().await;
    assert_eq!(f.lifecycle.restart_runs.load(std::sync::atomic::Ordering::SeqCst), 2);
}

// A crash respawn that wins the lock first has already replaced the daemon the
// user meant to restart; the restart must not stop the fresh one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_that_waited_behind_a_respawn_stops_nothing() {
    let f = fixture().await;
    let held = f.lifecycle.respawn_lock.lock().await;
    let restart = f.lifecycle.restart();
    // Let the restart take its snapshot and park on the lock.
    tokio::time::sleep(Duration::from_millis(200)).await;
    *f.lifecycle.current_session.write().unwrap() = "respawned-meanwhile".into();
    drop(held);

    let outcome = restart.await.expect("nothing to fail");

    assert_eq!(outcome.stop_path, None);
    assert_eq!(outcome.new_session, "respawned-meanwhile");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_daemon_answers_the_probe() {
    let mut f = fixture().await;
    f.lifecycle.probe();
    match tokio::time::timeout(Duration::from_secs(5), f.events.next()).await {
        Ok(Some(RelayLifecycleEvent::Probed { responsive })) => assert!(responsive),
        other => panic!("expected a probe result, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_responding_is_reported_once_per_window() {
    let mut f = fixture().await;
    let session = f.lifecycle.current_session();
    f.lifecycle.report_probe(false, &session);
    f.lifecycle.report_probe(false, &session);
    assert!(matches!(f.events.next().await, Some(RelayLifecycleEvent::Probed { responsive: false })));
    assert!(no_event(&mut f.events), "the second report is debounced");
}

// An answer about a daemon that has since been replaced is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_of_a_replaced_daemon_is_ignored() {
    let mut f = fixture().await;
    f.lifecycle.report_probe(false, "an-older-session");
    assert!(no_event(&mut f.events));
}

#[cfg(unix)]
async fn spawn_sleeper(lifecycle: &RelayLifecycle, cwd: &std::path::Path) -> String {
    spawn_script(lifecycle, cwd, "sleep 30").await
}

#[cfg(unix)]
async fn spawn_script(lifecycle: &RelayLifecycle, cwd: &std::path::Path, script: &str) -> String {
    use oximux_relay_proto::{Request, Response};
    let spawned = lifecycle
        .client()
        .request(Request::Spawn {
            cwd: cwd.to_string_lossy().into_owned(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            args: vec!["-c".into(), script.into()],
            env: Vec::new(),
            prefill: Vec::new(),
        })
        .await
        .expect("spawn");
    match spawned {
        Response::SpawnOk { pty_id, .. } => pty_id,
        other => panic!("spawn: {other:?}"),
    }
}

// Kill all ends every session and leaves the daemon up: the same client still
// answers, nothing is reported as a death, and the list is empty.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_all_ends_every_session_and_keeps_the_daemon() {
    let mut f = fixture().await;
    for _ in 0..3 {
        spawn_sleeper(&f.lifecycle, f._dir.path()).await;
    }
    assert_eq!(f.lifecycle.list_pty_ids().await.expect("list").len(), 3);
    let session = f.lifecycle.current_session();

    let outcome = f.lifecycle.kill_all_sessions(Vec::new()).await.expect("kill all");

    assert_eq!(outcome, KillAllOutcome { before: 3, after: 0 });
    assert_eq!(f.lifecycle.list_pty_ids().await.expect("still answers"), Vec::<String>::new());
    assert_eq!(f.lifecycle.current_session(), session, "same daemon");
    assert!(no_event(&mut f.events), "kill all is not a lifecycle event");
}

// Asked for twice at once, kill all runs once; a finished one does not linger.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_kill_alls_share_one_sweep() {
    let f = fixture().await;
    spawn_sleeper(&f.lifecycle, f._dir.path()).await;
    let held = f.lifecycle.respawn_lock.lock().await;
    let first = f.lifecycle.kill_all_sessions(Vec::new());
    let second = f.lifecycle.kill_all_sessions(Vec::new());
    assert!(first.ptr_eq(&second), "the second joins the first");
    drop(held);

    let (a, b) = futures::join!(first, second);

    assert_eq!(a, Ok(KillAllOutcome { before: 1, after: 0 }));
    assert_eq!(a, b);
    let again = f.lifecycle.kill_all_sessions(Vec::new()).await;
    assert_eq!(again, Ok(KillAllOutcome { before: 0, after: 0 }));
}

// Sessions that ignore SIGTERM (as an idle interactive shell does) each wait
// out the whole grace. The daemon runs one connection's closes side by side,
// so kill all costs about one grace, not one per session — in series these
// eight would take over four seconds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closes_that_wait_out_their_grace_overlap() {
    let f = fixture().await;
    for _ in 0..8 {
        spawn_script(&f.lifecycle, f._dir.path(), "trap '' TERM; sleep 60").await;
    }
    // Until each shell has run its `trap`, SIGTERM still ends it at once.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();

    let outcome = f.lifecycle.kill_all_sessions(Vec::new()).await.expect("kill all");

    assert_eq!(outcome, KillAllOutcome { before: 8, after: 0 });
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2500), "closes ran in series: {took:?}");
}

// A kill all that waited behind a respawn finds a daemon the user was never
// shown, and leaves its sessions alone.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_all_that_waited_behind_a_respawn_kills_nothing() {
    let f = fixture().await;
    spawn_sleeper(&f.lifecycle, f._dir.path()).await;
    let held = f.lifecycle.respawn_lock.lock().await;
    let kill_all = f.lifecycle.kill_all_sessions(Vec::new());
    // Let the sweep take its snapshot and park on the lock.
    tokio::time::sleep(Duration::from_millis(200)).await;
    *f.lifecycle.current_session.write().unwrap() = "respawned-meanwhile".into();
    drop(held);

    assert_eq!(kill_all.await, Err(KillAllError::Replaced));
    assert_eq!(f.lifecycle.list_pty_ids().await.expect("list").len(), 1, "nothing killed");
}

// A tab close's own close is already under way when the sweep runs, so the
// daemon no longer lists that session; given its id, the sweep still waits
// until it has ended.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweep_waits_for_closes_already_under_way() {
    use oximux_relay_proto::{Request, Response};
    let f = fixture().await;
    let pty_id = spawn_script(&f.lifecycle, f._dir.path(), "trap '' TERM; sleep 60").await;
    // Until the shell has run its `trap`, SIGTERM still ends it at once.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = f.lifecycle.client();
    let tab_close = {
        let pty_id = pty_id.clone();
        tokio::spawn(async move {
            client.request(Request::Close { pty_id, grace_ms: 1000 }).await
        })
    };
    // The tab's close has begun: the session is off the list.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.lifecycle.list_pty_ids().await.expect("list").contains(&pty_id) {
        assert!(Instant::now() < deadline, "the tab close never began");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let started = Instant::now();

    let outcome = f.lifecycle.kill_all_sessions(vec![pty_id]).await.expect("kill all");

    assert_eq!(outcome, KillAllOutcome { before: 0, after: 0 });
    assert!(started.elapsed() >= Duration::from_millis(500), "returned before the session ended");
    assert!(matches!(tab_close.await.expect("join"), Ok(Response::Ok)));
}

/// A live child that is not us, standing in for a foreign `oximux serve`.
#[cfg(unix)]
fn foreign_process() -> std::process::Child {
    std::process::Command::new("sleep").arg("30").spawn().expect("spawn sleep")
}

/// Write the holder record the way a host does, with chosen fields.
fn write_holder_record(dir: &std::path::Path, pid: u32, started: Option<u64>, boot: Option<u64>) {
    let lock = dir.join(oximux_remote_local::HOST_LOCK_FILENAME);
    let opt = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_default();
    std::fs::write(lock.with_extension("pid"), format!("{pid}\n{}\n{}\nserve\n", opt(started), opt(boot)))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn foreign_serve_holder_accepts_a_live_holder_whose_record_matches() {
    let dir = TempDir::new().unwrap();
    let mut child = foreign_process();
    let pid = child.id();
    let started = oximux_proc_tree::start_time_of_pid(pid);
    write_holder_record(dir.path(), pid, started, oximux_proc_tree::boot_time_of_system());
    assert_eq!(foreign_serve_holder(dir.path()), Some(pid));
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn foreign_serve_holder_rejects_a_recycled_pid() {
    // The pid is alive, but its process started long after the record says,
    // which is what a pid recycled by an unrelated process looks like.
    let dir = TempDir::new().unwrap();
    let mut child = foreign_process();
    let pid = child.id();
    let started = oximux_proc_tree::start_time_of_pid(pid).map(|t| t - 600);
    write_holder_record(dir.path(), pid, started, oximux_proc_tree::boot_time_of_system());
    assert_eq!(foreign_serve_holder(dir.path()), None);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn foreign_serve_holder_rejects_a_record_from_another_boot() {
    let dir = TempDir::new().unwrap();
    let mut child = foreign_process();
    let pid = child.id();
    let started = oximux_proc_tree::start_time_of_pid(pid);
    let old_boot = oximux_proc_tree::boot_time_of_system().map(|b| b - 3600);
    write_holder_record(dir.path(), pid, started, old_boot);
    assert_eq!(foreign_serve_holder(dir.path()), None);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn foreign_serve_holder_keeps_trusting_a_live_one_line_record() {
    // An older serve wrote only its pid. It is alive, so it is still reported.
    let dir = TempDir::new().unwrap();
    let mut child = foreign_process();
    let pid = child.id();
    let lock = dir.path().join(oximux_remote_local::HOST_LOCK_FILENAME);
    std::fs::write(lock.with_extension("pid"), format!("{pid}\n")).unwrap();
    assert_eq!(foreign_serve_holder(dir.path()), Some(pid));
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn foreign_serve_holder_ignores_our_own_record_and_an_unreadable_one() {
    let dir = TempDir::new().unwrap();
    write_holder_record(
        dir.path(),
        std::process::id(),
        oximux_proc_tree::start_time_of_pid(std::process::id()),
        oximux_proc_tree::boot_time_of_system(),
    );
    assert_eq!(foreign_serve_holder(dir.path()), None, "our own record is not foreign");
    let lock = dir.path().join(oximux_remote_local::HOST_LOCK_FILENAME);
    std::fs::write(lock.with_extension("pid"), "garbage\n").unwrap();
    assert_eq!(foreign_serve_holder(dir.path()), None);
}
