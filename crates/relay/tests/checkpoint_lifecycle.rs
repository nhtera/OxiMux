// Disk-checkpoint lifecycle against a real PTY registry: a live shell
// gets a checkpoint dir at spawn and scrollback on the tick; every
// clean end (deliberate close, natural exit) removes it. Whatever
// remains on disk is therefore an unclean death — the exact contract
// the app's cold-restore reader depends on.

use std::sync::Arc;
use std::time::Duration;

use oximux_shell_env::test_support::{lines, test_cwd, test_shell};
use oximux_relay::checkpoint::CheckpointStore;
use oximux_relay::registry::{PtyRegistry, SpawnArgs};
use oximux_relay_proto::Notification;
use tempfile::TempDir;
use tokio::time::timeout;

fn spawn_args() -> SpawnArgs {
    SpawnArgs {
        cwd: test_cwd(),
        cols: 80,
        rows: 24,
        shell: Some(test_shell()),
        args: Vec::new(),
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

    // The spawn-seeded meta must carry the live shell child's pid (the
    // app resolves split-inherit cwd from it kernel-side). Gated: every
    // Unix target reports a child pid, but a target without process ids
    // legitimately stores None and must not hard-fail the suite.
    // Parsed on every platform so a malformed meta.json fails the suite
    // anywhere; only the pid assertions below are Unix-shaped.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let meta: oximux_relay::checkpoint::CheckpointMeta =
        serde_json::from_slice(&std::fs::read(pty_dir.join("meta.json")).expect("read meta"))
            .expect("parse meta");
    #[cfg(unix)]
    assert!(meta.pid.is_some(), "child pid recorded at spawn");
    #[cfg(target_os = "macos")]
    assert!(
        meta.pid.and_then(oximux_proc_cwd::cwd_of_pid).is_some(),
        "recorded pid must be a live process with a resolvable cwd"
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

    // A pass with no new output is a no-op (skip-if-unchanged): observe via
    // mtime stability. Polled until stable rather than asserted on the very
    // next pass, because "no new output" is a state the PTY settles into, not
    // one the first checkpoint already guarantees — a Windows shell through
    // ConPTY keeps trickling startup output (banner, prompt repaints) after
    // its first bytes land, and each trickle makes the following rewrite
    // legitimate. The contract under test is that an idle PTY STOPS being
    // rewritten, and that is what the stability probe asserts.
    let mtime = || {
        std::fs::metadata(pty_dir.join("scrollback.bin"))
            .and_then(|m| m.modified())
            .expect("mtime")
    };
    let mut settled = false;
    for _ in 0..200 {
        let before = mtime();
        registry.checkpoint_all();
        if mtime() == before {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        settled,
        "unchanged PTY must stop rewriting its checkpoint once output settles"
    );

    registry
        .close(&pty_id, Duration::from_millis(500))
        .await
        .expect("close");
    wait_until("checkpoint dir removal after close", || !pty_dir.exists()).await;
}

/// The checkpoint meta must track the shell child's LIVE working
/// directory (kernel-resolved each tick), not the spawn-time cwd —
/// that's what lets a cold restore revive the user where they actually
/// were. macOS-only: the cwd resolver returns None elsewhere.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn checkpoint_meta_tracks_live_shell_cwd() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("checkpoints");
    let store = Arc::new(CheckpointStore::new(base.clone()));
    let registry = PtyRegistry::with_checkpoints(Some(store));

    let target = TempDir::new().expect("target dir");
    // The kernel reports the canonical path (tempdirs sit behind a
    // /var → /private/var symlink), so compare canonicalized.
    let want = target
        .path()
        .canonicalize()
        .expect("canonicalize target")
        .to_string_lossy()
        .into_owned();

    let pty_id = registry.spawn(spawn_args()).expect("spawn");
    let meta_path = base.join(&pty_id).join("meta.json");
    registry
        .write(&pty_id, format!("cd '{}'\n", target.path().display()).as_bytes())
        .expect("write cd");

    wait_until("checkpoint meta to pick up the live cwd", || {
        registry.checkpoint_all();
        std::fs::read(&meta_path)
            .ok()
            .and_then(|raw| {
                serde_json::from_slice::<oximux_relay::checkpoint::CheckpointMeta>(&raw).ok()
            })
            .map(|meta| meta.cwd == want)
            .unwrap_or(false)
    })
    .await;

    registry
        .close(&pty_id, Duration::from_millis(500))
        .await
        .expect("close");
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
    registry.write(&pty_id, &lines(&["exit"])).expect("write exit");

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

#[tokio::test]
async fn attach_after_exit_replays_exit_to_new_subscriber() {
    // The daemon outlives the app; a re-launched app attaching to a session
    // whose child already died must be told it is dead (replayed `Exit`),
    // not left adopting the frozen ring as a live, input-less pane.
    let registry = PtyRegistry::with_checkpoints(None);
    let pty_id = registry.spawn(spawn_args()).expect("spawn");

    // Subscriber A drives the shell to a clean exit and observes it.
    let (tx_a, mut rx_a) = tokio::sync::mpsc::channel::<Notification>(64);
    registry.attach(&pty_id, tx_a).expect("attach A");
    registry.write(&pty_id, &lines(&["exit"])).expect("write exit");
    let code_a = timeout(Duration::from_secs(5), async {
        loop {
            match rx_a.recv().await {
                Some(Notification::Exit { code, .. }) => break code,
                Some(_) => continue,
                None => panic!("channel closed before Exit"),
            }
        }
    })
    .await
    .expect("first subscriber sees Exit");
    assert_eq!(code_a, Some(0), "a clean `exit` carries status 0");

    // Subscriber B reconnects AFTER the child is gone — it must still get Exit.
    let (tx_b, mut rx_b) = tokio::sync::mpsc::channel::<Notification>(64);
    registry.attach(&pty_id, tx_b).expect("attach B");
    let code_b = timeout(Duration::from_secs(2), async {
        loop {
            match rx_b.recv().await {
                Some(Notification::Exit { code, .. }) => break code,
                Some(_) => continue,
                None => panic!("channel closed before replayed Exit"),
            }
        }
    })
    .await
    .expect("reconnecting subscriber is replayed Exit");
    assert_eq!(code_b, Some(0), "replayed Exit carries the real status");
}

#[tokio::test]
async fn exited_pty_is_excluded_from_list() {
    // `list()` backs the restore liveness gate (`live_external_ids`). A PTY
    // whose child has exited is retained for replay/cold-restore, but it must
    // NOT be reported as live — otherwise restore warm-re-attaches to the
    // corpse, shows its frozen scrollback, and silently swallows every
    // keystroke (writes land on a PTY with no reader). A dead session must
    // fall through to a fresh respawn instead.
    let registry = PtyRegistry::with_checkpoints(None);
    let pty_id = registry.spawn(spawn_args()).expect("spawn");

    // While the child is alive, the session is a valid re-attach target.
    assert!(
        registry.list().iter().any(|d| d.pty_id == pty_id),
        "a live PTY must be listed"
    );

    // Drive the shell to a clean exit and wait until the registry observes it.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Notification>(64);
    registry.attach(&pty_id, tx).expect("attach");
    registry.write(&pty_id, &lines(&["exit"])).expect("write exit");
    timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(Notification::Exit { .. }) => break,
                Some(_) => continue,
                None => panic!("channel closed before Exit"),
            }
        }
    })
    .await
    .expect("shell exits");

    // The corpse is gone from the live list, so a restore gate keyed on it
    // respawns rather than attaching a dead, input-less pane.
    assert!(
        !registry.list().iter().any(|d| d.pty_id == pty_id),
        "an exited PTY must be excluded from list()"
    );
}
