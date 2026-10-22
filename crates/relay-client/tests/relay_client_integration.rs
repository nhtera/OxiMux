// End-to-end: boot the real `oximux-relay` daemon in-process, then
// drive it through `RelayBackend` (sync TerminalBackend API). Proves
// the trait contract is upheld across the socket.

use std::sync::Arc;
use std::time::Duration;

use oximux_pty::{SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId};
// Only the in-process latency probe uses this, and that probe is unix-only.
#[cfg(unix)]
use oximux_pty::PortablePtyBackend;
use oximux_shell_env::test_support::{echo_two_vars, lines, test_cwd, test_shell};
use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::{RelayBackend, RelayClient};
use tempfile::TempDir;
use tokio::runtime::Runtime;

struct Fixture {
    backend: RelayBackend,
    _runtime: Arc<Runtime>,
    _dir: TempDir,
}





fn boot_fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-v1.sock");
    let token_file = dir.path().join("relay-v1.token");
    let token = "test-token-abc".to_string();
    std::fs::write(&token_file, &token).expect("write token");

    let runtime = Arc::new(Runtime::new().expect("runtime"));

    let socket_for_server = socket.clone();
    let token_file_for_server = token_file.clone();
    runtime.spawn(async move {
        let _ = run_server(ServerConfig::idle_disabled(
            socket_for_server,
            token_file_for_server,
        ))
        .await;
    });

    // Retry the real connect until the daemon is serving, and keep the client
    // that succeeds. A separate readiness probe would open a connection only to
    // throw it away, and would have to name a transport this test does not care
    // about — `RelayClient` already dials whichever one the platform uses.
    let client = runtime.block_on(async {
        for _ in 0..200 {
            if let Ok(client) = RelayClient::connect(&socket, &token).await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("relay never came up");
    });
    let backend = RelayBackend::new(Arc::new(client), runtime.handle().clone());

    Fixture {
        backend,
        _runtime: runtime,
        _dir: dir,
    }
}

fn drain_until<F: Fn(&TerminalEvent) -> bool>(
    backend: &mut RelayBackend,
    overall: Duration,
    pred: F,
) -> Vec<TerminalEvent> {
    let deadline = std::time::Instant::now() + overall;
    let mut events = Vec::new();
    while std::time::Instant::now() < deadline {
        events.extend(backend.drain_events());
        if events.iter().any(&pred) {
            return events;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    events
}

#[test]
fn spawn_write_observe_output_then_exit() {
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(id, &lines(&["echo CLIENT_HELLO", "exit"]))
        .expect("write");

    let events = drain_until(&mut fx.backend, Duration::from_secs(5), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });

    let mut combined = Vec::new();
    let mut saw_exit = false;
    for ev in events {
        match ev {
            TerminalEvent::Output { bytes, .. } => combined.extend(bytes),
            TerminalEvent::Exit { .. } => saw_exit = true,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&combined);
    assert!(text.contains("CLIENT_HELLO"), "missing marker in: {text:?}");
    assert!(saw_exit, "no Exit event");

    fx.backend.close(id).expect("close idempotent");
}

#[test]
fn status_consumer_cannot_steal_relay_renderer_events() {
    const MARKER: &[u8] = b"STATUS_MARKER";
    let mut fx = boot_fixture();
    let original_id = fx
        .backend
        .spawn(SpawnConfig {
            shell: test_shell(),
            cwd: test_cwd(),
            ..SpawnConfig::default()
        })
        .expect("spawn");
    let external_id = fx
        .backend
        .external_id_of(original_id)
        .expect("relay PTY id");
    fx.backend
        .detach(original_id)
        .expect("detach before restore");
    let id = fx
        .backend
        .attach_existing(&external_id)
        .expect("restore existing PTY");
    fx.backend
        .write(id, &lines(&["echo STATUS_MARKER", "exit 7"]))
        .expect("write restored command");

    let output_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < output_deadline {
        let snapshot = fx.backend.snapshot(id).expect("restored snapshot");
        let text = snapshot
            .cells
            .iter()
            .flat_map(|row| row.iter().map(|cell| cell.ch))
            .collect::<String>();
        if text.contains("STATUS_MARKER") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    fx.backend
        .subscribe_status_events(id)
        .expect("late restored status registration");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut status = Vec::new();
    let mut status_exit = None;
    while std::time::Instant::now() < deadline && status_exit.is_none() {
        for event in fx.backend.drain_status_events_for(id) {
            match event {
                TerminalEvent::Output { bytes, .. } => status.extend(bytes),
                TerminalEvent::Exit { code, .. } => status_exit = Some(code),
                _ => panic!("status stream must contain only Output/Exit"),
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let renderer = fx.backend.drain_events_for(id);
    let mut renderer_bytes = Vec::new();
    let mut renderer_exit = None;
    for event in renderer {
        match event {
            TerminalEvent::Output { bytes, .. } => renderer_bytes.extend(bytes),
            TerminalEvent::Exit { code, .. } => renderer_exit = Some(code),
            _ => {}
        }
    }

    assert!(status.windows(MARKER.len()).any(|bytes| bytes == MARKER));
    assert_eq!(status_exit, Some(Some(7)));
    assert!(
        renderer_bytes
            .windows(MARKER.len())
            .any(|bytes| bytes == MARKER),
        "status consumer stole relay renderer output"
    );
    assert_eq!(renderer_exit, Some(Some(7)));
    fx.backend.unsubscribe_status_events(id);
    fx.backend.close(id).expect("close idempotent");
}

// Context-env round-trip through the FULL client path: `SpawnConfig.env`
// → wire → daemon → child environment. The sibling daemon-protocol test
// proves the daemon honors the env; this proves the env survives
// `SpawnConfig` serialization through `RelayBackend` (the exact API the
// app drives when it injects the per-pane identity vars). Same reasoning
// as that test: the markers can only appear in the expanded echo output,
// never in the typed command (which references the variable NAMES), so a
// substring match is unambiguous.
#[test]
fn spawn_config_env_reaches_child_via_backend() {
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        env: vec![
            ("OXIMUX_WORKSPACE_ID".into(), "WS_CLIENT_MARKER_5".into()),
            ("OXIMUX_TAB_ID".into(), "TAB_CLIENT_MARKER_9".into()),
        ],
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(
            id,
            &lines(&[&echo_two_vars("OXIMUX_WORKSPACE_ID", "OXIMUX_TAB_ID"), "exit"]),
        )
        .expect("write");

    let events = drain_until(&mut fx.backend, Duration::from_secs(5), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });
    let mut combined = Vec::new();
    for ev in events {
        if let TerminalEvent::Output { bytes, .. } = ev {
            combined.extend(bytes);
        }
    }
    let text = String::from_utf8_lossy(&combined);
    assert!(
        text.contains("WS_CLIENT_MARKER_5|TAB_CLIENT_MARKER_9"),
        "SpawnConfig.env didn't reach child; got {text:?}"
    );

    fx.backend.close(id).expect("close idempotent");
}

#[test]
fn detach_keeps_pty_alive_for_reattach() {
    // Tear-off invariant (Phase 3 Slice C): detaching ONE attachment must not
    // kill the daemon PTY — a second attachment (the destination window) keeps
    // driving the same live shell. Contrast with `close`, which kills it.
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    // Window A spawns + auto-attaches.
    let id_a = fx.backend.spawn(cfg).expect("spawn");
    let external_id = fx
        .backend
        .external_id_of(id_a)
        .expect("relay pty id for spawned session");

    // Real tear-off order: the tab leaves window A FIRST (detach, not close —
    // releases A's attachment but keeps the daemon PTY alive), THEN the
    // destination attaches. The two are never attached on the same client at
    // once, because the client multiplexes one output subscription per pty_id.
    fx.backend.detach(id_a).expect("detach");

    // `close` on the just-detached session is a no-op (session already gone),
    // which is exactly why the source view's Drop → close doesn't kill the PTY.
    fx.backend
        .close(id_a)
        .expect("close after detach is a no-op");

    // Window B attaches to the SAME daemon PTY (the tear-off destination).
    let id_b = fx
        .backend
        .attach_existing(&external_id)
        .expect("attach_existing after detach — PTY must still be alive");

    // B drives the live shell → proves the PTY survived the detach.
    fx.backend
        .write(id_b, &lines(&["echo SURVIVED_DETACH", "exit"]))
        .expect("write via B");
    let events = drain_until(&mut fx.backend, Duration::from_secs(5), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });
    let mut combined = Vec::new();
    for ev in events {
        if let TerminalEvent::Output { bytes, .. } = ev {
            combined.extend(bytes);
        }
    }
    let text = String::from_utf8_lossy(&combined);
    assert!(
        text.contains("SURVIVED_DETACH"),
        "PTY died after detach; B saw: {text:?}"
    );

    fx.backend.close(id_b).expect("close B idempotent");
}

#[test]
fn snapshot_mirrors_remote_state() {
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(id, &lines(&["echo LOCAL_GRID_OK"]))
        .expect("write");

    // Wait for the bytes to flow back; drain_events advances state
    // as a side-effect of the pump task.
    let _ = drain_until(
        &mut fx.backend,
        Duration::from_secs(3),
        |e| matches!(e, TerminalEvent::Output { bytes, .. } if String::from_utf8_lossy(bytes).contains("LOCAL_GRID_OK")),
    );

    let snap = fx.backend.snapshot(id).expect("snapshot");
    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        rendered.contains("LOCAL_GRID_OK"),
        "grid missing marker; got {rendered:?}"
    );

    fx.backend.write(id, &lines(&["exit"])).expect("write exit");
    let _ = drain_until(&mut fx.backend, Duration::from_secs(2), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });
}

// Regression guard: `resize` must apply to the local grid SYNCHRONOUSLY
// and return immediately, without blocking the caller on a daemon ack.
// The render thread calls this every time pane bounds change; a
// re-introduced `block_on(request(Resize))` would park the UI (and, on
// macOS, can wedge the main thread in the App-Nap path). We assert the
// snapshot reflects the new dims on the very next line — proving the
// local state update happens before (not after) the daemon round-trip.
#[test]
fn resize_applies_to_local_grid_synchronously() {
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");

    let before = fx.backend.snapshot(id).expect("snapshot");
    assert_eq!((before.cols, before.rows), (80, 24), "initial dims");

    // Resize must return Ok immediately and not surface a daemon error
    // synchronously (the ack is fire-and-forget on the relay runtime).
    fx.backend.resize(id, 120, 40).expect("resize returns Ok");

    // Read the snapshot on the next line — no drain/sleep. If this is
    // already 120x40, the local grid was resized synchronously.
    let after = fx.backend.snapshot(id).expect("snapshot");
    assert_eq!(
        (after.cols, after.rows),
        (120, 40),
        "resize did not update the local grid synchronously"
    );

    // A Resize event is emitted synchronously for the view to observe.
    let saw_resize = fx.backend.drain_events().iter().any(|e| {
        matches!(
            e,
            TerminalEvent::Resize {
                cols: 120,
                rows: 40,
                ..
            }
        )
    });
    assert!(saw_resize, "no synchronous Resize event");

    fx.backend.write(id, &lines(&["exit"])).ok();
}

// End-to-end keystroke latency probe. Measures write→echo round-trip
// for N single-byte writes against a real /bin/sh through the live
// daemon. Run with:
//   cargo test --release -p oximux-relay-client \
//       --test relay_client_integration -- --ignored --nocapture \
//       keystroke_echo_latency
// Ignored by default so the normal `cargo test` cycle stays under 1s.
// Driven by `cat` echoing stdin line-by-line and ended with a raw Ctrl+C,
// which are tty line-discipline behaviors; cmd.exe has no equivalent that
// echoes each write back immediately. Porting needs a different probe, not
// a different command.
#[cfg(unix)]
#[test]
#[ignore]
fn keystroke_echo_latency() {
    use std::time::Instant;

    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    // Wait for the prompt so the shell is in "ready to echo" mode.
    let _ = drain_until(&mut fx.backend, Duration::from_secs(2), |_| false);

    // Disable line-editor echo confusion: tell the shell to echo a
    // unique marker on each input by reading one char in a loop. Use
    // `cat` so every byte we write comes back verbatim, no buffering.
    fx.backend.write(id, b"cat\n").expect("start cat");
    std::thread::sleep(Duration::from_millis(200));
    let _ = fx.backend.drain_events();

    const N: usize = 50;
    let mut samples_us: Vec<u128> = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        fx.backend.write(id, b"x").expect("write");
        // Spin-drain until we see at least one Output containing 'x'.
        loop {
            let events = fx.backend.drain_events();
            if events
                .iter()
                .any(|e| matches!(e, TerminalEvent::Output { bytes, .. } if bytes.contains(&b'x')))
            {
                samples_us.push(t0.elapsed().as_micros());
                break;
            }
            if t0.elapsed() > Duration::from_millis(500) {
                panic!("no echo within 500ms — daemon hung?");
            }
            std::thread::yield_now();
        }
    }

    samples_us.sort_unstable();
    let min = samples_us[0];
    let p50 = samples_us[N / 2];
    let p95 = samples_us[(N * 95) / 100];
    let max = *samples_us.last().unwrap();
    eprintln!(
        "keystroke_echo_latency over {N} samples (microseconds): \
         min={min} p50={p50} p95={p95} max={max}"
    );

    // Hard upper bound: if p50 > 30ms the daemon path is the bottleneck;
    // < 30ms means render-side investigation needed.
    assert!(
        p50 < 30_000,
        "p50={p50}µs exceeds 30ms budget — see relay path"
    );

    fx.backend.write(id, b"\x03").ok(); // SIGINT to cat
    fx.backend.write(id, b"exit\n").ok();
    let _ = drain_until(&mut fx.backend, Duration::from_secs(2), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });
}

// A/B counterpart: same latency probe against the in-process backend,
// so the relay numbers can be compared against the "no daemon" baseline.
// Same invocation pattern (--ignored --nocapture).
// Driven by `cat` echoing stdin line-by-line and ended with a raw Ctrl+C,
// which are tty line-discipline behaviors; cmd.exe has no equivalent that
// echoes each write back immediately. Porting needs a different probe, not
// a different command.
#[cfg(unix)]
#[test]
#[ignore]
fn keystroke_echo_latency_in_process() {
    use std::time::Instant;

    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = backend.spawn(cfg).expect("spawn");
    std::thread::sleep(Duration::from_millis(200));
    let _ = backend.drain_events();
    backend.write(id, b"cat\n").expect("start cat");
    std::thread::sleep(Duration::from_millis(200));
    let _ = backend.drain_events();

    const N: usize = 50;
    let mut samples_us: Vec<u128> = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        backend.write(id, b"x").expect("write");
        loop {
            let events = backend.drain_events();
            if events
                .iter()
                .any(|e| matches!(e, TerminalEvent::Output { bytes, .. } if bytes.contains(&b'x')))
            {
                samples_us.push(t0.elapsed().as_micros());
                break;
            }
            if t0.elapsed() > Duration::from_millis(500) {
                panic!("no echo within 500ms — pty hung?");
            }
            std::thread::yield_now();
        }
    }

    samples_us.sort_unstable();
    eprintln!(
        "keystroke_echo_latency_in_process over {N} samples (microseconds): \
         min={} p50={} p95={} max={}",
        samples_us[0],
        samples_us[N / 2],
        samples_us[(N * 95) / 100],
        samples_us.last().unwrap()
    );

    backend.write(id, b"\x03").ok();
    backend.write(id, b"exit\n").ok();
}

#[test]
fn attach_existing_replays_into_local_state() {
    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let original_id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(original_id, &lines(&["echo PERSIST_MARKER"]))
        .expect("write");
    // Wait for the byte to land in the daemon's ring buffer.
    let _ = drain_until(
        &mut fx.backend,
        Duration::from_secs(3),
        |e| matches!(e, TerminalEvent::Output { bytes, .. } if String::from_utf8_lossy(bytes).contains("PERSIST_MARKER")),
    );
    let client = Arc::clone(fx.backend.client());
    let relay_pty_id = {
        let resp = fx
            ._runtime
            .block_on(client.request(oximux_relay_proto::Request::ListPtys))
            .expect("list");
        match resp {
            oximux_relay_proto::Response::PtyList(v) => v[0].pty_id.clone(),
            other => panic!("{other:?}"),
        }
    };

    // Simulate "second client connects later": drop the spawning
    // session but keep the daemon alive, then call attach_existing.
    // RelayBackend doesn't expose drop-per-session yet, but attach
    // happens against a new local session id regardless.
    let attached_id = fx
        .backend
        .attach_relay_pty(&relay_pty_id)
        .expect("attach existing");
    assert_ne!(attached_id, original_id);

    let snap = fx.backend.snapshot(attached_id).expect("snapshot");
    let rendered: String = snap
        .cells
        .iter()
        .flat_map(|row| row.iter().map(|c| c.ch))
        .collect();
    assert!(
        rendered.contains("PERSIST_MARKER"),
        "replay didn't reach the local grid; got {rendered:?}"
    );

    fx.backend.close(attached_id).expect("close");
    fx.backend.close(original_id).expect("close");
}

// Daemon-replacement swap contract. A fresh backend seeded with a dead
// predecessor's sessions must:
// (a) report each as lost to its daemon — `DaemonLost`, never an `Exit`, which
//     would read as the program ending on its own — exactly once, on the
//     renderer and the status drain alike;
// (b) keep that one-shot signal for its own view: the unfiltered drain (which
//     an agent cancel runs on the backend every view shares) must not take it;
// (c) still answer `external_id_of` with the lost session's daemon id, and
//     carry undrained losses into a further replacement;
// (d) never mint a session id a mounted view still holds.
#[test]
fn seeded_daemon_losses_fire_once_and_floor_fresh_ids() {
    let mut fx = boot_fixture();
    fx.backend.seed_daemon_losses(vec![
        (TerminalSessionId(3), Some("pty-three".into())),
        (TerminalSessionId(7), Some("pty-seven".into())),
    ]);

    // (b) the shared, unfiltered drain leaves the losses alone…
    let unfiltered = fx.backend.drain_events();
    assert!(
        !unfiltered.iter().any(|e| matches!(e, TerminalEvent::DaemonLost { .. })),
        "the unfiltered drain must not take a view's loss, got {unfiltered:?}"
    );
    // (a) …so each view still gets its one DaemonLost on its own drain…
    let ev = fx.backend.drain_events_for(TerminalSessionId(3));
    assert!(
        matches!(
            ev.as_slice(),
            [TerminalEvent::DaemonLost { id }] if *id == TerminalSessionId(3)
        ),
        "expected one DaemonLost for id 3, got {ev:?}"
    );
    // …and only once.
    assert!(
        fx.backend.drain_events_for(TerminalSessionId(3)).is_empty(),
        "DaemonLost must not repeat"
    );
    // The status stream (agent pollers) hears it too, once.
    let status = fx.backend.drain_status_events_for(TerminalSessionId(3));
    assert!(
        matches!(status.as_slice(), [TerminalEvent::DaemonLost { .. }]),
        "status drain must report the loss, got {status:?}"
    );

    // (c) the lost ids still resolve to their daemon ids…
    assert_eq!(fx.backend.external_id_of(TerminalSessionId(7)).as_deref(), Some("pty-seven"));
    // …and a further replacement takes over the one not yet reported.
    let carried = fx.backend.sessions_to_carry();
    assert!(
        carried.contains(&(TerminalSessionId(7), Some("pty-seven".into()))),
        "an undrained loss is carried forward, got {carried:?}"
    );
    assert!(
        !carried.iter().any(|(id, _)| *id == TerminalSessionId(3)),
        "a reported loss is not reported twice, got {carried:?}"
    );
    // Its view letting go forgets it.
    fx.backend.close(TerminalSessionId(7)).expect("close");
    assert_eq!(fx.backend.external_id_of(TerminalSessionId(7)), None);
    assert!(fx.backend.drain_events_for(TerminalSessionId(7)).is_empty());

    // (d) fresh spawns clear the inherited floor.
    let cfg = SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    assert!(
        id.0 > 7,
        "fresh id {} must clear the inherited floor of 7",
        id.0
    );
    let _ = fx.backend.close(id);
}

// A replaced backend's views stay mounted holding their ids — for good, if
// bringing them back fails — and a replacement may be seeded with nothing (no
// live sessions left). Its fresh ids must still never collide with them.
#[test]
fn a_replacement_backend_never_re_mints_an_earlier_id() {
    let mut fx = boot_fixture();
    let cfg = || SpawnConfig {
        shell: test_shell(),
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let earlier = fx.backend.spawn(cfg()).expect("spawn");
    let mut replacement =
        RelayBackend::new(Arc::clone(fx.backend.client()), fx._runtime.handle().clone());
    replacement.seed_daemon_losses(Vec::new());

    let fresh = replacement.spawn(cfg()).expect("spawn on the replacement");

    assert!(fresh.0 > earlier.0, "fresh id {} collides with held id {}", fresh.0, earlier.0);
    let _ = replacement.close(fresh);
    let _ = fx.backend.close(earlier);
}

// --- OSC 7501 program status over the relay (unix: POSIX shell scripts) ---

/// The reply, as `od -An -tx1` prints it.
#[cfg(unix)]
const PROGRAM_STATUS_REPLY_HEX: &str = "1b 5d 37 35 30 31 3b 3f 1b 5c";

/// Ask once, report, then print every input byte received within a second of
/// quiet as hex, then `DONE`, then idle (so the session outlives the drain).
#[cfg(unix)]
fn program_status_script(report: &str) -> SpawnConfig {
    SpawnConfig {
        shell: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "stty raw -echo min 0 time 10; printf '\\033]7501;?\\007\\033]7501;{report}\\007'; \
                 dd bs=1 count=64 2>/dev/null | od -An -tx1 | tr -d '\\n'; printf '\\r\\nDONE\\r\\n'; \
                 while :; do sleep 1; done"
            ),
        ],
        cwd: test_cwd(),
        cols: 200,
        rows: 24,
        ..SpawnConfig::default()
    }
}

#[cfg(unix)]
fn output_text(events: &[TerminalEvent]) -> String {
    let mut bytes = Vec::new();
    for ev in events {
        if let TerminalEvent::Output { bytes: b, .. } = ev {
            bytes.extend_from_slice(b);
        }
    }
    String::from_utf8_lossy(&bytes).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(unix)]
fn reported_states(events: &[TerminalEvent]) -> Vec<(Option<oximux_pty::ProgramState>, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            TerminalEvent::ProgramStatus { root, handshake, .. } => {
                Some((root.as_ref().map(|r| r.state), *handshake))
            }
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
fn until_done(backend: &mut RelayBackend) -> Vec<TerminalEvent> {
    drain_until(backend, Duration::from_secs(10), |e| {
        matches!(e, TerminalEvent::Output { bytes, .. } if String::from_utf8_lossy(bytes).contains("DONE"))
    })
}

/// A current daemon answers; the client parses the reports but never answers
/// itself, so the program hears exactly one reply.
#[cfg(unix)]
#[test]
fn program_status_a_current_daemon_answers_once_and_reports_reach_the_client() {
    let mut fx = boot_fixture();
    let id = fx.backend.spawn(program_status_script("state=working:app=t")).expect("spawn");
    let events = until_done(&mut fx.backend);
    let text = output_text(&events);
    assert!(text.contains("DONE"), "{text:?}");
    assert_eq!(text.matches(PROGRAM_STATUS_REPLY_HEX).count(), 1, "{text:?}");
    assert_eq!(reported_states(&events), vec![(Some(oximux_pty::ProgramState::Working), true)]);
    let _ = fx.backend.close(id);
}

/// A second window attaching later starts from the program's last report,
/// recovered from the replay ring, without answering the replayed query.
#[cfg(unix)]
#[test]
fn program_status_an_attach_restores_the_reported_root_from_the_replay() {
    let mut fx = boot_fixture();
    let id = fx
        .backend
        .spawn(program_status_script("state=blocked:kind=permission"))
        .expect("spawn");
    let text = output_text(&until_done(&mut fx.backend));
    assert_eq!(text.matches(PROGRAM_STATUS_REPLY_HEX).count(), 1, "{text:?}");
    let pty = fx.backend.external_id_of(id).expect("relay id");

    let mut second =
        RelayBackend::new(Arc::clone(fx.backend.client()), fx._runtime.handle().clone());
    let attached = second.attach_existing(&pty).expect("attach");
    let events = second.drain_events_for(attached);
    assert_eq!(
        reported_states(&events),
        vec![(Some(oximux_pty::ProgramState::Blocked), true)],
        "the attach must seed the replayed root"
    );
    let _ = second.detach(attached);
    let _ = fx.backend.close(id);
}

/// For a daemon that predates program-status replies, the client answers
/// instead. Production enables this only for such a daemon (see
/// `daemon_answers_program_status`); forcing it on against this current daemon
/// makes two replies — the daemon's and the client's — which is how this test
/// sees that the client's reply path works at all.
#[cfg(unix)]
#[test]
fn program_status_a_client_answering_for_an_older_daemon_sends_its_reply() {
    let fx = boot_fixture();
    let mut backend = RelayBackend::new(Arc::clone(fx.backend.client()), fx._runtime.handle().clone())
        .with_program_status_replies(true);
    let id = backend.spawn(program_status_script("state=idle")).expect("spawn");
    let text = output_text(&until_done(&mut backend));
    assert_eq!(text.matches(PROGRAM_STATUS_REPLY_HEX).count(), 2, "{text:?}");
    let _ = backend.close(id);
}
