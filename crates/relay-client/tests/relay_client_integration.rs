// End-to-end: boot the real `oximux-relay` daemon in-process, then
// drive it through `RelayBackend` (sync TerminalBackend API). Proves
// the trait contract is upheld across the socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_pty::{SpawnConfig, TerminalBackend, TerminalEvent};
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
        let _ = run_server(ServerConfig {
            socket_path: socket_for_server,
            token_file: token_file_for_server,
        })
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

