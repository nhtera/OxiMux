// End-to-end: boot the real `oximux-relay` daemon in-process, then
// drive it through `RelayBackend` (sync TerminalBackend API). Proves
// the trait contract is upheld across the socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_pty::{
    PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId,
};
use oximux_relay::server::{ServerConfig, run_server};
use oximux_relay_client::{RelayBackend, RelayClient};
use tempfile::TempDir;
use tokio::net::UnixStream;
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

    // Wait for the socket to accept.
    runtime.block_on(async {
        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("relay socket never came up");
    });

    let client = runtime
        .block_on(async { RelayClient::connect(&socket, &token).await })
        .expect("client connect");
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(id, b"echo CLIENT_HELLO\nexit\n")
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
        .write(id, b"echo \"$OXIMUX_WORKSPACE_ID|$OXIMUX_TAB_ID\"\nexit\n")
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
        .write(id_b, b"echo SURVIVED_DETACH\nexit\n")
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(id, b"printf 'LOCAL_GRID_OK\\n'\n")
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

    fx.backend.write(id, b"exit\n").expect("write exit");
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
    let saw_resize = fx
        .backend
        .drain_events()
        .iter()
        .any(|e| matches!(e, TerminalEvent::Resize { cols: 120, rows: 40, .. }));
    assert!(saw_resize, "no synchronous Resize event");

    fx.backend.write(id, b"exit\n").ok();
}

// End-to-end keystroke latency probe. Measures write→echo round-trip
// for N single-byte writes against a real /bin/sh through the live
// daemon. Run with:
//   cargo test --release -p oximux-relay-client \
//       --test relay_client_integration -- --ignored --nocapture \
//       keystroke_echo_latency
// Ignored by default so the normal `cargo test` cycle stays under 1s.
#[test]
#[ignore]
fn keystroke_echo_latency() {
    use std::time::Instant;

    let mut fx = boot_fixture();
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
#[test]
#[ignore]
fn keystroke_echo_latency_in_process() {
    use std::time::Instant;

    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
        cols: 80,
        rows: 24,
        ..SpawnConfig::default()
    };
    let original_id = fx.backend.spawn(cfg).expect("spawn");
    fx.backend
        .write(original_id, b"printf 'PERSIST_MARKER\\n'\n")
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

// Crash-recovery swap contract: a fresh backend seeded with a dead
// predecessor's session ids must (a) emit exactly one synthetic Exit per
// inherited id, (b) emit nothing for them afterwards, and (c) never mint
// a fresh session id at or below the inherited floor — a live
// TerminalView still holds the old id, and aliasing it would cross-wire
// two panes' event queues.
#[test]
fn seeded_synthetic_exits_fire_once_and_floor_fresh_ids() {
    let mut fx = boot_fixture();
    fx.backend
        .seed_synthetic_exits(vec![TerminalSessionId(3), TerminalSessionId(7)]);

    // (a) one synthetic Exit per inherited id, on the per-session drain…
    let ev = fx.backend.drain_events_for(TerminalSessionId(3));
    assert!(
        matches!(
            ev.as_slice(),
            [TerminalEvent::Exit { id, code: None }] if *id == TerminalSessionId(3)
        ),
        "expected one synthetic Exit for id 3, got {ev:?}"
    );
    // (b) …and only once.
    assert!(
        fx.backend.drain_events_for(TerminalSessionId(3)).is_empty(),
        "synthetic Exit must not repeat"
    );
    // The unfiltered drain flushes the remaining inherited id.
    let rest = fx.backend.drain_events();
    assert!(
        rest.iter().any(|e| matches!(
            e,
            TerminalEvent::Exit { id, code: None } if *id == TerminalSessionId(7)
        )),
        "unfiltered drain must flush remaining inherited ids, got {rest:?}"
    );

    // (c) fresh spawns clear the inherited floor.
    let cfg = SpawnConfig {
        shell: "/bin/sh".into(),
        cwd: PathBuf::from("/tmp"),
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
