//! `Shutdown { kill_sessions: true }` — the daemon side of a user restart.
//!
//! The contract the app's pane recovery depends on: every session ends, none of
//! them is reported as exited, and each keeps the checkpoint it is restored
//! from. Unix-only: the children are `sh` scripts, and SIGHUP/SIGTERM handling
//! is what one of them tests.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use oximux_relay::checkpoint::CheckpointStore;
use oximux_relay::registry::{PtyRegistry, SpawnArgs};
use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Notification, Request, Response};
use oximux_shell_env::test_support::test_cwd;
use tempfile::TempDir;

fn sh(script: &str) -> SpawnArgs {
    SpawnArgs {
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        prefill: Vec::new(),
    }
}

fn scrollback(base: &Path, pty_id: &str) -> String {
    String::from_utf8_lossy(&oximux_relay_proto::checkpoint_codec::decode(
        &std::fs::read(base.join(pty_id).join("scrollback.bin")).unwrap_or_default(),
    ))
        .into_owned()
}

fn alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

fn child_pid(base: &Path, pty_id: &str) -> u32 {
    let meta: oximux_relay::checkpoint::CheckpointMeta =
        serde_json::from_slice(&std::fs::read(base.join(pty_id).join("meta.json")).expect("meta"))
            .expect("parse meta");
    meta.pid.expect("child pid recorded at spawn")
}

/// Wait for `needle` in a session's output, counting what `attach` replayed:
/// a fast child can print before the attach, and those bytes arrive only in
/// the replay.
async fn wait_for_output(
    replay: &[u8],
    rx: &mut tokio::sync::mpsc::Receiver<Notification>,
    needle: &str,
) {
    let mut seen = replay.to_vec();
    if String::from_utf8_lossy(&seen).contains(needle) {
        return;
    }
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(n) = rx.recv().await {
            if let Notification::Output { bytes, .. } = n {
                seen.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert_eq!(found, Ok(true), "never saw {needle:?}");
}

/// Many sessions at once, so a reader thread that races the flag has every
/// chance to delete a checkpoint or raise an `Exit`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminate_all_keeps_every_checkpoint_and_raises_no_exit() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("checkpoints");
    let registry = Arc::new(PtyRegistry::with_checkpoints(Some(Arc::new(CheckpointStore::new(
        base.clone(),
    )))));

    let mut sessions = Vec::new();
    for i in 0..20 {
        let pty_id = registry.spawn(sh(&format!("echo hello-{i}; sleep 1000"))).expect("spawn");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Notification>(256);
        let (replay, ..) = registry.attach(&pty_id, tx).expect("attach");
        wait_for_output(&replay, &mut rx, &format!("hello-{i}")).await;
        sessions.push((pty_id, rx));
    }

    registry.terminate_all(Duration::from_secs(2)).await;

    assert!(registry.list().is_empty(), "every session ended");
    for (i, (pty_id, rx)) in sessions.iter_mut().enumerate() {
        assert!(!alive(child_pid(&base, pty_id)), "session {i}'s child is dead");
        assert!(
            scrollback(&base, pty_id).contains(&format!("hello-{i}")),
            "session {i} kept a checkpoint with its output"
        );
        while let Ok(n) = rx.try_recv() {
            assert!(!matches!(n, Notification::Exit { .. }), "session {i} raised Exit: {n:?}");
        }
    }
}

/// A child in its own session only gets SIGHUP when the daemon goes away, so
/// one that ignores it — and SIGTERM — must still be killed explicitly, or it
/// outlives the daemon and the app resumes a second copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_all_kills_a_child_that_ignores_hup_and_term() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("checkpoints");
    let registry = Arc::new(PtyRegistry::with_checkpoints(Some(Arc::new(CheckpointStore::new(
        base.clone(),
    )))));
    let pty_id = registry
        .spawn(sh("trap '' HUP TERM; echo stubborn; while :; do sleep 1; done"))
        .expect("spawn");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Notification>(64);
    let (replay, ..) = registry.attach(&pty_id, tx).expect("attach");
    wait_for_output(&replay, &mut rx, "stubborn").await;
    let pid = child_pid(&base, &pty_id);

    registry.terminate_all(Duration::from_millis(300)).await;

    for _ in 0..100 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("a child ignoring HUP and TERM survived terminate_all");
}

/// Over the wire: the reply arrives, the daemon exits, the checkpoint stays,
/// and the subscribed client never sees an `Exit`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kill_sessions_keeps_checkpoints_and_sends_no_exit() {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-test.sock");
    let token_file = dir.path().join("relay-test.token");
    let base = dir.path().join("checkpoints");
    std::fs::write(&token_file, "restart-test-token").expect("token");
    let mut cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    cfg.checkpoint_dir = Some(base.clone());
    let server = tokio::spawn(async move { run_server(cfg).await });
    let mut client = None;
    for _ in 0..200 {
        if let Ok(c) = RelayClient::connect(&socket, "restart-test-token").await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client = client.expect("relay never came up");

    let spawned = client
        .request(Request::Spawn {
            cwd: test_cwd().to_string_lossy().into_owned(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            // The delay lets the subscription below exist before the output.
            args: vec!["-c".into(), "sleep 0.3; echo hello; sleep 1000".into()],
            env: Vec::new(),
            prefill: Vec::new(),
        })
        .await
        .expect("spawn");
    let Response::SpawnOk { pty_id, .. } = spawned else { panic!("spawn: {spawned:?}") };
    let (_sub, mut notes) = client.subscribe_pty(&pty_id);
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(n) = notes.recv().await {
            if let Notification::Output { bytes, .. } = n {
                seen.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&seen).contains("hello") {
                    return;
                }
            }
        }
    })
    .await
    .expect("never saw hello");
    let pid = child_pid(&base, &pty_id);

    let reply = client.request(Request::Shutdown { kill_sessions: true }).await.expect("shutdown");
    assert_eq!(reply, Response::Ok);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("daemon did not exit after Shutdown{kill_sessions}")
        .expect("server task panicked")
        .expect("server returned an error");

    assert!(!alive(pid), "the session's child is dead");
    assert!(scrollback(&base, &pty_id).contains("hello"), "checkpoint kept with its output");
    while let Ok(n) = notes.try_recv() {
        assert!(!matches!(n, Notification::Exit { .. }), "client saw Exit: {n:?}");
    }
}

/// An interactive shell ignores SIGTERM and honours SIGHUP by hanging up its
/// jobs — which job control puts in process groups of their own, out of reach
/// of a group signal. Returns the session and its background job's pid.
async fn shell_with_background_job(
    registry: &PtyRegistry,
) -> (String, tokio::sync::mpsc::Receiver<Notification>, u32) {
    let pty_id = registry
        .spawn(SpawnArgs {
            cwd: test_cwd(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/bash".into()),
            args: vec!["--norc".into(), "--noprofile".into(), "-i".into()],
            env: Vec::new(),
            prefill: Vec::new(),
        })
        .expect("spawn");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Notification>(256);
    let (replay, ..) = registry.attach(&pty_id, tx).expect("attach");
    registry.write(&pty_id, b"sleep 4242 & echo JOB=$!\n").expect("write");
    let mut seen = String::from_utf8_lossy(&replay).into_owned();
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(n) = rx.recv().await {
            if let Notification::Output { bytes, .. } = n {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                // The echoed command line holds `JOB=$!`; the output holds digits.
                if let Some(pid) = seen.split("JOB=").filter_map(|rest| {
                    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                    digits.parse::<u32>().ok()
                }).next() {
                    return pid;
                }
            }
        }
        panic!("session ended before reporting its job");
    })
    .await
    .expect("never saw the job pid");
    (pty_id, rx, pid)
}

async fn wait_dead(pid: u32) -> bool {
    for _ in 0..150 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

// Closing a tab must end what the shell was running in the background, as a
// hangup does — not SIGKILL the shell and leave its jobs orphaned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_hangs_up_a_shell_so_its_jobs_end_too() {
    let registry = PtyRegistry::new();
    let (pty_id, _rx, job) = shell_with_background_job(&registry).await;
    registry.close(&pty_id, Duration::from_millis(300)).await.expect("close");
    assert!(wait_dead(job).await, "the shell's background job outlived the close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_all_hangs_up_a_shell_so_its_jobs_end_too() {
    let registry = Arc::new(PtyRegistry::new());
    let (_pty_id, _rx, job) = shell_with_background_job(&registry).await;
    registry.terminate_all(Duration::from_millis(300)).await;
    assert!(wait_dead(job).await, "the shell's background job outlived the restart");
}

// A session started once the restart has begun would miss its snapshot and
// never be ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_new_session_starts_once_a_restart_has_begun() {
    let registry = Arc::new(PtyRegistry::new());
    registry.terminate_all(Duration::from_millis(100)).await;
    assert!(registry.spawn(sh("echo late")).is_err());
}
