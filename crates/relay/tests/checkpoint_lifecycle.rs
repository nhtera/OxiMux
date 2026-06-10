// Disk-checkpoint lifecycle against a real PTY registry: a live shell
// gets a checkpoint dir at spawn and scrollback on the tick; every
// clean end (deliberate close, natural exit) removes it. Whatever
// remains on disk is therefore an unclean death — the exact contract
// the app's cold-restore reader depends on.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_relay::checkpoint::CheckpointStore;
use oximux_relay::registry::{PtyRegistry, SpawnArgs};
use oximux_relay_proto::Notification;
use tempfile::TempDir;
use tokio::time::timeout;

fn spawn_args() -> SpawnArgs {
    SpawnArgs {
        cwd: PathBuf::from("/"),
        cols: 80,
        rows: 24,
        shell: Some("/bin/sh".into()),
        env: Vec::new(),
    }
}

async fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    for _ in 0..200 {
        if probe() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn checkpoint_written_on_tick_and_removed_on_close() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("checkpoints");
    let store = Arc::new(CheckpointStore::new(base.clone()));
    let registry = PtyRegistry::with_checkpoints(Some(Arc::clone(&store)));

    let pty_id = registry.spawn(spawn_args()).expect("spawn");
    let pty_dir = base.join(&pty_id);
    assert!(
        pty_dir.join("meta.json").exists(),
        "meta seeded at spawn so even a pre-tick crash is identifiable"
    );

    // The shell prints a prompt to the PTY; once bytes land in the ring
    // a checkpoint pass must persist them.
    wait_until("scrollback checkpoint to appear", || {
        registry.checkpoint_all();
        std::fs::read(pty_dir.join("scrollback.bin"))
            .map(|b| !b.is_empty())
            .unwrap_or(false)
    })
    .await;

    // A second pass with no new output is a no-op (skip-if-unchanged):
    // observe via mtime stability.
    let before = std::fs::metadata(pty_dir.join("scrollback.bin"))
        .and_then(|m| m.modified())
        .expect("mtime");
    registry.checkpoint_all();
    let after = std::fs::metadata(pty_dir.join("scrollback.bin"))
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(before, after, "unchanged PTY must not rewrite its checkpoint");

    registry
        .close(&pty_id, Duration::from_millis(500))
        .await
        .expect("close");
    wait_until("checkpoint dir removal after close", || !pty_dir.exists()).await;
}

#[tokio::test]
async fn natural_shell_exit_removes_checkpoint() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("checkpoints");
    let store = Arc::new(CheckpointStore::new(base.clone()));
    let registry = PtyRegistry::with_checkpoints(Some(store));

    let pty_id = registry.spawn(spawn_args()).expect("spawn");
    let pty_dir = base.join(&pty_id);
    assert!(pty_dir.exists());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Notification>(64);
    registry.attach(&pty_id, tx).expect("attach");
    registry.write(&pty_id, b"exit\n").expect("write exit");

    timeout(Duration::from_secs(5), async {
        while let Some(n) = rx.recv().await {
            if matches!(n, Notification::Exit { .. }) {
                break;
            }
        }
    })
    .await
    .expect("shell exit notification");

    wait_until("checkpoint dir removal after natural exit", || {
        !pty_dir.exists()
    })
    .await;
}
