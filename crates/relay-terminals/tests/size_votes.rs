//! Two attachments to one terminal, against a real relay daemon booted
//! in-process.
//!
//! The property under test belongs to the daemon: it runs a PTY at the smallest
//! grid any attachment asks for, so every attachment holds a standing vote and a
//! resize is a change to *one* of them. That contract cannot be checked against a
//! stand-in — a fake would simply agree with whatever this crate did — so these
//! drive the daemon itself.
//!
//! What makes this worth a real daemon: one [`RelayTerminals`] is shared by every
//! paired device, so two phones watching one terminal are two attachments through
//! one of these. If the attachment were resolved from the PTY here rather than
//! carried by the caller, the second device to attach would own every later
//! resize and the first device's vote would never be recorded at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Request, Response};
use oximux_relay_terminals::RelayTerminals;
use oximux_remote_host::TerminalSource;
use oximux_shell_env::test_support::{test_cwd, test_shell};
use tempfile::TempDir;

struct TestRelay {
    socket: PathBuf,
    token: String,
    _dir: TempDir,
    _server: tokio::task::JoinHandle<()>,
}

/// Boot a daemon on a private socket and wait for it to accept connections.
async fn boot_relay() -> TestRelay {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-test.sock");
    let token_file = dir.path().join("relay-test.token");
    let token = "size-vote-test-token".to_string();
    std::fs::write(&token_file, &token).expect("write token");

    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    let server = tokio::spawn(async move {
        let _ = run_server(cfg).await;
    });
    for _ in 0..200 {
        if RelayClient::connect(&socket, &token).await.is_ok() {
            return TestRelay { socket, token, _dir: dir, _server: server };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("relay socket never came up");
}

async fn terminals(socket: &Path, token: &str) -> RelayTerminals {
    let client = RelayClient::connect(socket, token).await.expect("connect");
    RelayTerminals::new(Arc::new(client))
}

/// Spawn a shell at `cols`x`rows` and return its PTY id.
async fn spawn_pty(client: &RelayClient, cols: u16, rows: u16) -> String {
    let spawned = client
        .request(Request::Spawn {
            cwd: test_cwd().to_string_lossy().into_owned(),
            cols,
            rows,
            shell: Some(test_shell()),
            args: Vec::new(),
            env: Vec::new(),
        })
        .await
        .expect("spawn");
    match spawned {
        Response::SpawnOk { pty_id, .. } => pty_id,
        other => panic!("expected SpawnOk, got {other:?}"),
    }
}

/// The grid the daemon is actually running the PTY at — the `min` across every
/// attachment's standing vote, which is the only observable that distinguishes
/// "each attachment voted" from "one attachment was overwritten twice".
async fn effective_size(client: &RelayClient, pty_id: &str) -> (u16, u16) {
    let listed = client.request(Request::ListPtys).await.expect("list");
    match listed {
        Response::PtyList(ptys) => {
            let p = ptys.into_iter().find(|p| p.pty_id == pty_id).expect("the spawned pty");
            (p.cols, p.rows)
        }
        other => panic!("expected PtyList, got {other:?}"),
    }
}

/// Two devices on one terminal each hold their own size vote.
///
/// The discriminating step is the last one. Each attachment votes separately, so
/// after a small vote and then a large one from a *different* attachment the
/// terminal stays small — the small vote is still standing. Were both resizes
/// landing on one attachment (what resolving by PTY does, since the second attach
/// replaces the first's entry), the small vote would never have been recorded and
/// the large one would overwrite it, leaving the terminal large.
///
/// That is the user-visible bug: two paired phones on one terminal, and the one
/// that attached first cannot shrink it — or worse, silently drives the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_attachments_each_hold_their_own_size_vote() {
    let relay = boot_relay().await;
    let observer = RelayClient::connect(&relay.socket, &relay.token).await.expect("connect");
    let pty = spawn_pty(&observer, 80, 24).await;

    // ONE `RelayTerminals` for both devices — the host installs a single shared
    // instance, so every paired phone's attachment is minted on this one
    // connection. Two instances would not reproduce the condition at all.
    let host = terminals(&relay.socket, &relay.token).await;
    let (a, _a_frames) = host.attach(&pty).await.expect("phone A attaches");
    let (b, _b_frames) = host.attach(&pty).await.expect("phone B attaches");
    assert_ne!(a.attachment, b.attachment, "each attach is its own attachment");
    assert_eq!(
        effective_size(&observer, &pty).await,
        (80, 24),
        "attaching alone changes nothing — a new attachment votes the current size",
    );

    host.resize(&pty, a.attachment, 40, 12).await.expect("A shrinks");
    assert_eq!(
        effective_size(&observer, &pty).await,
        (40, 12),
        "the smallest vote wins",
    );

    host.resize(&pty, b.attachment, 100, 30).await.expect("B grows");
    assert_eq!(
        effective_size(&observer, &pty).await,
        (40, 12),
        "A's vote still stands — B changed its own, not A's",
    );
}

/// Releasing one attachment withdraws that attachment's vote and no other.
///
/// The other half of the same property: with A gone the terminal stops being
/// held at A's size, while B — untouched throughout — still holds its own vote
/// and can still be resized on its own id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_departed_device_stops_holding_the_terminal_small() {
    let relay = boot_relay().await;
    // Spawning auto-attaches the spawner, so this connection holds a standing
    // vote at the spawn size for the whole test. That is the floor every
    // assertion below is measured against, not an artifact to work around.
    let observer = RelayClient::connect(&relay.socket, &relay.token).await.expect("connect");
    let pty = spawn_pty(&observer, 80, 24).await;

    let host = terminals(&relay.socket, &relay.token).await;
    let (a, a_frames) = host.attach(&pty).await.expect("phone A attaches");
    let (b, _b_frames) = host.attach(&pty).await.expect("phone B attaches");

    host.resize(&pty, a.attachment, 40, 12).await.expect("A shrinks");
    assert_eq!(effective_size(&observer, &pty).await, (40, 12));

    // A leaves. Dropping the frame receiver is what a client disconnecting looks
    // like from here; the forwarding task notices on its next send and hands the
    // attachment back. Waking it needs one byte of output, which is why the
    // shell is asked to produce some.
    drop(a_frames);
    observer
        .request(Request::Write { pty_id: pty.clone(), bytes: b"\n".to_vec() })
        .await
        .expect("nudge the shell so the forwarding task wakes and detaches");

    let mut released = false;
    for _ in 0..200 {
        if effective_size(&observer, &pty).await == (80, 24) {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        released,
        "A's vote outlived A: the terminal is still {:?}, so the departed \
         attachment was never released",
        effective_size(&observer, &pty).await,
    );

    // B was never touched by any of that, and resizing it still lands on B.
    host.resize(&pty, b.attachment, 60, 20).await.expect("B shrinks");
    assert_eq!(
        effective_size(&observer, &pty).await,
        (60, 20),
        "B's own vote is live — A's departure did not take it along",
    );
}
