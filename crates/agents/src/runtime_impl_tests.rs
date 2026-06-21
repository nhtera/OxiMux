//! Tests for `runtime_impl` — kept in a `#[path]` submodule so the runtime
//! file itself stays under the file-size cap. Still a child module of
//! `runtime_impl`, so `use super::*` reaches its private items
//! (`lock_recover`, `POLL_INTERVAL`, `SessionEntry`, …).

use super::*;
use crate::cli::CustomCommandAdapter;
use oximux_pty::TerminalEvent;
use std::path::PathBuf;

// A panic while some thread held the runtime lock must not take every
// later agent operation down with it: lock_recover hands back the
// still-consistent value instead of propagating the poison.
#[test]
fn lock_recover_survives_poisoned_mutex() {
    let m = Arc::new(Mutex::new(5i32));
    let m2 = m.clone();
    let _ = std::thread::spawn(move || {
        let _guard = m2.lock().unwrap();
        panic!("poison the lock");
    })
    .join();
    assert!(m.lock().is_err(), "mutex must be poisoned by the panic");
    assert_eq!(*lock_recover(&m, "test"), 5, "value recovered intact");
    // And a recovered lock keeps working for writes afterwards.
    *lock_recover(&m, "test") = 6;
    assert_eq!(*lock_recover(&m, "test"), 6);
}

fn echo_cfg(program: &str, args: Vec<String>) -> AgentSessionConfig {
    AgentSessionConfig {
        adapter: AgentAdapter::Custom,
        worktree_path: PathBuf::from("/"),
        prompt: None,
        model: None,
        effort: None,
        extra_args: Vec::new(),
        env: Vec::new(),
        cols: 80,
        rows: 24,
        custom_command: Some((program.to_string(), args)),
        resumption: oximux_core::SessionResumption::None,
    }
}

fn runtime_with_custom() -> CliRuntime {
    let rt = CliRuntime::new();
    rt.register_adapter(AgentAdapter::Custom, Arc::new(CustomCommandAdapter));
    rt
}

#[tokio::test(flavor = "multi_thread")]
async fn start_session_unknown_adapter_errors() {
    let rt = CliRuntime::new();
    let err = rt
        .start_session(echo_cfg("/bin/true", vec![]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no adapter"));
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_echo_runs_to_done() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/echo", vec!["hello".into()]))
        .await
        .expect("start_session");

    let mut rx = rt.subscribe_status(id).expect("subscribe");
    // Wait until status becomes terminal or 3 s elapses.
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rx.borrow().status.is_terminal() {
                return rx.borrow().status.clone();
            }
            if rx.changed().await.is_err() {
                // Sender dropped before terminal — runtime bug; surface
                // as a panic so the test fails loudly.
                panic!("status sender closed before terminal status");
            }
        }
    })
    .await;
    let final_status = result.expect("did not reach terminal status in time");
    match final_status {
        AgentStatus::Done { code } => {
            // /bin/echo exits 0
            assert_eq!(code, Some(0), "expected exit code 0, got {code:?}");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn current_status_starts_at_idle() {
    let rt = runtime_with_custom();
    // /bin/sleep gives us a few seconds of "running but not yet done"
    // to query while the session is live.
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["2".into()]))
        .await
        .expect("start_session");
    let initial = rt.current_status(id).expect("current_status");
    assert!(matches!(initial, AgentStatus::Idle | AgentStatus::Running));
    // Cleanup so the test doesn't take 2 s.
    let _ = rt.cancel(id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_terminates_session_and_removes_entry() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
        .await
        .expect("start_session");
    // Cancel should return well before the 30 s sleep finishes.
    let cancel_started = Instant::now();
    rt.cancel(id).await.expect("cancel");
    assert!(
        cancel_started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}, expected < 5 s",
        cancel_started.elapsed()
    );
    // Session is gone from the table — subscribe_status now errors.
    let err = rt.subscribe_status(id).unwrap_err();
    assert!(err.to_string().contains("unknown session"));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_message_writes_to_pty() {
    let rt = runtime_with_custom();
    // /bin/cat echoes whatever we feed it; we just verify write() does
    // not error and the session survives the write.
    let id = rt
        .start_session(echo_cfg("/bin/cat", vec![]))
        .await
        .expect("start_session");
    rt.send_message(id, "ping\n").await.expect("send_message");
    // status is still non-terminal
    let s = rt.current_status(id).expect("current_status");
    assert!(!s.is_terminal());
    let _ = rt.cancel(id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn send_message_unknown_session_errors() {
    let rt = runtime_with_custom();
    let bogus = AgentSessionId::new(999);
    let err = rt.send_message(bogus, "x").await.unwrap_err();
    assert!(err.to_string().contains("unknown session"));
}

// The approval card answers a numeric prompt with a carriage-return-terminated
// reply (`"1\r"`). This proves that contract end-to-end against a real readline:
// `read x` only returns — letting the shell reach `exit 0` — if the CR actually
// submits the line. A `\n`-or-nothing terminator would leave `read` blocked and
// the session would never go terminal, timing the test out.
#[tokio::test(flavor = "multi_thread")]
async fn cr_terminated_reply_submits_a_readline() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sh", vec!["-c".into(), "read x; exit 0".into()]))
        .await
        .expect("start_session");
    let mut rx = rt.subscribe_status(id).expect("subscribe");
    rt.send_message(id, "1\r").await.expect("send_message");
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if rx.borrow().status.is_terminal() {
                return rx.borrow().status.clone();
            }
            if rx.changed().await.is_err() {
                panic!("status sender closed before terminal status");
            }
        }
    })
    .await;
    let final_status = result.expect("readline never returned — CR did not submit the line");
    match final_status {
        AgentStatus::Done { code } => assert_eq!(code, Some(0), "expected clean exit"),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_unknown_session_errors() {
    let rt = runtime_with_custom();
    let bogus = AgentSessionId::new(999);
    let err = rt.cancel(bogus).await.unwrap_err();
    assert!(err.to_string().contains("unknown session"));
}

// M2 (review 260520-1448): explicit subscribe-then-cancel coverage of
// the user-facing contract — UI badge holds a Receiver across a cancel
// and must observe the final terminal status before the session is
// removed from the table.
#[tokio::test(flavor = "multi_thread")]
async fn subscribe_then_cancel_publishes_terminal_status() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
        .await
        .expect("start_session");
    let mut rx = rt.subscribe_status(id).expect("subscribe");
    rt.cancel(id).await.expect("cancel");
    // After cancel returns, the poll task has either exited (publishing
    // terminal status) or been aborted. In the happy path we see a
    // terminal state on the receiver.
    let final_status = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let s = rx.borrow().status.clone();
            if s.is_terminal() {
                return s;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().status.clone();
            }
        }
    })
    .await
    .expect("did not reach terminal status after cancel");
    assert!(
        final_status.is_terminal(),
        "expected terminal, got {final_status:?}"
    );
    // A user cancel must read as Interrupted ("Stopped"), never as the
    // Done/Failed exit-code mapping the kill signal would produce.
    assert_eq!(
        final_status,
        AgentStatus::Interrupted,
        "cancel must publish Interrupted, got {final_status:?}"
    );
}

// M3 (review 260520-1448): double-cancel must error on the second call
// with the typed "unknown session" message — proves the table remove
// is the source of truth, not just the OS-level kill.
#[tokio::test(flavor = "multi_thread")]
async fn double_cancel_second_call_errors() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
        .await
        .expect("start_session");
    rt.cancel(id).await.expect("first cancel");
    let err = rt.cancel(id).await.unwrap_err();
    assert!(err.to_string().contains("unknown session"));
}

// Phase 3 step 9 sub-1: the app renderer needs the same backend Arc the
// poll task holds so it can drain output and resize without going
// through `send_message`. `backend_for` hands out a clone; both
// callers compete on the same mutex.
#[tokio::test(flavor = "multi_thread")]
async fn backend_for_returns_live_handle_shared_with_poll_task() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
        .await
        .expect("start_session");
    let backend = rt.backend_for(id).expect("backend_for");
    // Arc count: one in SessionEntry, one cloned into the poll task,
    // one we just took. Asserting an exact count would tie the test
    // to the poll-task internals; instead prove the Arc is shared by
    // exercising the same mutex from both sides.
    let term_id = rt.terminal_session_id(id).expect("terminal_session_id");
    let resize_ok = tokio::task::spawn_blocking(move || {
        let mut be = backend.lock().expect("backend mutex poisoned");
        be.resize(term_id, 100, 30).is_ok()
    })
    .await
    .expect("spawn_blocking");
    assert!(resize_ok, "renderer-side resize must succeed");
    // Session is still alive and reachable through the runtime.
    let s = rt.current_status(id).expect("current_status");
    assert!(!s.is_terminal(), "session must not be killed by resize");
    let _ = rt.cancel(id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_polling_preserves_renderer_output_and_exit() {
    const MARKER: &[u8] = b"STATUS_MARKER";
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg(
            "/bin/sh",
            vec![
                "-c".into(),
                "read first; printf 'STATUS_MARKER\\n'; read second; exit 7".into(),
            ],
        ))
        .await
        .expect("start gated shell");
    let backend = rt.backend_for(id).expect("renderer backend");
    let term_id = rt.terminal_session_id(id).expect("terminal session id");
    let mut status = rt.subscribe_status(id).expect("status receiver");

    rt.send_message(id, "first\n")
        .await
        .expect("release output");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot_has_marker = {
                let be = lock_recover(&backend, "terminal backend");
                be.snapshot(term_id)
                    .expect("terminal snapshot")
                    .cells
                    .iter()
                    .flat_map(|row| row.iter().map(|cell| cell.ch))
                    .collect::<String>()
                    .contains("STATUS_MARKER")
            };
            if snapshot_has_marker {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("marker did not reach terminal state");
    // Give the poller two complete opportunities to drain. Under the
    // former shared-queue design this removed the marker before the
    // renderer assertion below.
    tokio::time::sleep(POLL_INTERVAL * 2).await;
    assert!(matches!(&status.borrow().status, AgentStatus::Running));

    let first_renderer_events = {
        let mut be = lock_recover(&backend, "terminal backend");
        be.drain_events_for(term_id)
    };
    let first_bytes = first_renderer_events
        .into_iter()
        .filter_map(|event| match event {
            TerminalEvent::Output { bytes, .. } => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        first_bytes
            .windows(MARKER.len())
            .any(|bytes| bytes == MARKER),
        "status poller stole renderer marker"
    );

    rt.send_message(id, "second\n").await.expect("release exit");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !status.borrow().status.is_terminal() {
            status.changed().await.expect("status task remains live");
        }
    })
    .await
    .expect("status did not observe exit");
    let final_renderer_events = {
        let mut be = lock_recover(&backend, "terminal backend");
        be.drain_events_for(term_id)
    };
    assert!(
        final_renderer_events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Exit { code: Some(7), .. }))
    );
    rt.cancel(id).await.expect("remove completed session");
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_for_unknown_session_errors() {
    let rt = runtime_with_custom();
    let bogus = AgentSessionId::new(999);
    // `SharedBackend` wraps a trait object so `Result<SharedBackend>`
    // doesn't impl `Debug`; pattern-match the Err arm instead of
    // `unwrap_err()`.
    match rt.backend_for(bogus) {
        Ok(_) => panic!("expected unknown-session error"),
        Err(e) => assert!(e.to_string().contains("unknown session")),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_session_id_unknown_session_errors() {
    let rt = runtime_with_custom();
    let bogus = AgentSessionId::new(999);
    let err = rt.terminal_session_id(bogus).unwrap_err();
    assert!(err.to_string().contains("unknown session"));
}

// After cancel removes the SessionEntry, backend_for must surface the
// typed error — proves the session table (not the OS-level handle) is
// the source of truth.
#[tokio::test(flavor = "multi_thread")]
async fn backend_for_after_cancel_returns_unknown_session() {
    let rt = runtime_with_custom();
    let id = rt
        .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
        .await
        .expect("start_session");
    rt.cancel(id).await.expect("cancel");
    match rt.backend_for(id) {
        Ok(_) => panic!("expected unknown-session error after cancel"),
        Err(e) => assert!(e.to_string().contains("unknown session")),
    }
}
