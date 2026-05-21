// End-to-end: boot the real `oximux-relay` daemon in-process, then
// drive it through `RelayBackend` (sync TerminalBackend API). Proves
// the trait contract is upheld across the socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent};
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
    let _ = drain_until(&mut fx.backend, Duration::from_secs(3), |e| {
        matches!(e, TerminalEvent::Output { bytes, .. } if String::from_utf8_lossy(bytes).contains("LOCAL_GRID_OK"))
    });

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

    fx.backend
        .write(id, b"exit\n")
        .expect("write exit");
    let _ = drain_until(&mut fx.backend, Duration::from_secs(2), |e| {
        matches!(e, TerminalEvent::Exit { .. })
    });
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
    fx.backend
        .write(id, b"cat\n")
        .expect("start cat");
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
            if events.iter().any(|e| {
                matches!(e, TerminalEvent::Output { bytes, .. } if bytes.contains(&b'x'))
            }) {
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
            if events.iter().any(|e| {
                matches!(e, TerminalEvent::Output { bytes, .. } if bytes.contains(&b'x'))
            }) {
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
    let _ = drain_until(&mut fx.backend, Duration::from_secs(3), |e| {
        matches!(e, TerminalEvent::Output { bytes, .. } if String::from_utf8_lossy(bytes).contains("PERSIST_MARKER"))
    });
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

