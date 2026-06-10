// Disk scrollback checkpoints — crash insurance for daemon-owned PTYs.
//
// Every few seconds the daemon snapshots each active PTY's replay ring
// to disk. On a clean end (deliberate Close, natural child exit) the
// PTY's checkpoint directory is removed, so whatever REMAINS on disk is
// by construction an unclean death (daemon crash, SIGKILL, host
// reboot). The app's restore path reads these leftovers straight off
// disk — no wire-protocol involvement — and prefills the revived pane
// with the recovered scrollback, visibly marked as restored history.
//
// Layout: `<base>/<pty_id>/meta.json` + `<base>/<pty_id>/scrollback.bin`.
// All writes are atomic via tmp + rename: reading a slightly stale
// checkpoint is fine, reading a torn one is not.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Sidecar metadata for one PTY's checkpoint. `cols`/`rows` record the
/// grid the scrollback bytes were produced for — the consumer must
/// replay into an emulator of compatible width or absolute-position
/// sequences land in the wrong cells. `ended_at_epoch_secs` stays
/// `None` for as long as the session lives; a clean end removes the
/// whole directory instead of setting it, so the field doubles as a
/// safety check for readers (`Some` = not restorable).
///
/// `pid` is the shell child's OS pid. The daemon and its children run
/// on the same host as the app, so the app reads it to resolve the
/// LIVE working directory kernel-side (split-pane cwd inheritance for
/// daemon-owned panes) without any wire-protocol involvement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub started_at_epoch_secs: u64,
    pub ended_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pub pid: Option<u32>,
}

pub struct CheckpointStore {
    base: PathBuf,
}

impl CheckpointStore {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    pub fn dir_for(&self, pty_id: &str) -> PathBuf {
        self.base.join(pty_id)
    }

    /// Create the PTY's checkpoint dir and seed its meta. Called at
    /// spawn so a crash before the first scrollback tick still leaves
    /// an identifiable (if empty) session on disk.
    pub fn open(
        &self,
        pty_id: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
        pid: Option<u32>,
    ) -> Result<()> {
        let dir = self.dir_for(pty_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let meta = CheckpointMeta {
            cwd: cwd.to_string_lossy().into_owned(),
            cols,
            rows,
            started_at_epoch_secs: epoch_secs(),
            ended_at_epoch_secs: None,
            pid,
        };
        write_atomic(&dir.join("meta.json"), &serde_json::to_vec(&meta)?)
    }

    /// Persist one ring snapshot. Refreshes the meta's grid dims to the
    /// PTY's current effective size so the consumer replays at the
    /// geometry the bytes were produced for, and — when the caller could
    /// resolve it — the shell child's LIVE working directory, so a cold
    /// restore revives the replacement shell where the user actually
    /// was, not where the session started.
    pub fn write_scrollback(
        &self,
        pty_id: &str,
        bytes: &[u8],
        cols: u16,
        rows: u16,
        live_cwd: Option<&Path>,
    ) -> Result<()> {
        let dir = self.dir_for(pty_id);
        let meta_path = dir.join("meta.json");
        let mut meta: CheckpointMeta = std::fs::read(&meta_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or(CheckpointMeta {
                cwd: String::new(),
                cols,
                rows,
                started_at_epoch_secs: epoch_secs(),
                ended_at_epoch_secs: None,
                pid: None,
            });
        meta.cols = cols;
        meta.rows = rows;
        if let Some(cwd) = live_cwd {
            meta.cwd = cwd.to_string_lossy().into_owned();
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        write_atomic(&dir.join("scrollback.bin"), bytes)?;
        write_atomic(&meta_path, &serde_json::to_vec(&meta)?)
    }

    /// Drop the PTY's checkpoint entirely — a cleanly-ended session has
    /// nothing to restore. Idempotent.
    pub fn remove(&self, pty_id: &str) -> Result<()> {
        let dir = self.dir_for(pty_id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
        }
    }

    /// Boot-time sweep: remove checkpoint dirs whose meta hasn't been
    /// touched in `max_age` — orphans whose pane was dropped from every
    /// layout while no daemon was around to clean up. Recent dirs are
    /// deliberately left alone: a freshly-respawned daemon boots BEFORE
    /// the app's restore reconcile reads its cold-restore data.
    pub fn gc_older_than(&self, max_age: Duration) {
        let entries = match std::fs::read_dir(&self.base) {
            Ok(e) => e,
            Err(_) => return,
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Age by the meta's mtime (refreshed every checkpoint tick),
            // falling back to the dir's own mtime for metaless leftovers.
            let probe = path.join("meta.json");
            let mtime = std::fs::metadata(&probe)
                .or_else(|_| std::fs::metadata(&path))
                .and_then(|m| m.modified());
            let Ok(mtime) = mtime else { continue };
            let Ok(age) = now.duration_since(mtime) else {
                continue;
            };
            if age > max_age {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Atomic write: tmp file in the same directory, then rename over the
// target. rename(2) is atomic on the same filesystem, so readers see
// either the old complete file or the new complete file, never a torn
// one — even if the daemon dies mid-write.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, CheckpointStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CheckpointStore::new(dir.path().join("checkpoints"));
        (dir, store)
    }

    #[test]
    fn open_write_read_roundtrip() {
        let (_guard, store) = store();
        store
            .open("pty-a", Path::new("/tmp/work"), 80, 24, Some(4242))
            .expect("open");
        store
            .write_scrollback("pty-a", b"hello scrollback", 120, 40, None)
            .expect("write");

        let dir = store.dir_for("pty-a");
        let meta: CheckpointMeta =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).expect("meta"))
                .expect("parse meta");
        // No live cwd resolved → the spawn-time cwd is retained.
        assert_eq!(meta.cwd, "/tmp/work");
        // write_scrollback refreshed the dims to the latest effective size.
        assert_eq!((meta.cols, meta.rows), (120, 40));
        assert_eq!(meta.ended_at_epoch_secs, None);
        // The spawn-time child pid survives scrollback rewrites.
        assert_eq!(meta.pid, Some(4242));
        assert_eq!(
            std::fs::read(dir.join("scrollback.bin")).expect("scrollback"),
            b"hello scrollback"
        );

        // A resolved live cwd supersedes the spawn-time cwd: the user
        // cd'd somewhere and a cold restore should revive them there.
        store
            .write_scrollback("pty-a", b"more", 120, 40, Some(Path::new("/tmp/elsewhere")))
            .expect("write with live cwd");
        let meta: CheckpointMeta =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).expect("meta"))
                .expect("parse meta");
        assert_eq!(meta.cwd, "/tmp/elsewhere");
    }

    #[test]
    fn write_without_open_seeds_meta() {
        let (_guard, store) = store();
        store
            .write_scrollback("pty-b", b"orphan bytes", 100, 30, None)
            .expect("write");
        let dir = store.dir_for("pty-b");
        let meta: CheckpointMeta =
            serde_json::from_slice(&std::fs::read(dir.join("meta.json")).expect("meta"))
                .expect("parse meta");
        assert_eq!((meta.cols, meta.rows), (100, 30));
    }

    #[test]
    fn remove_is_idempotent() {
        let (_guard, store) = store();
        store
            .open("pty-c", Path::new("/"), 80, 24, Some(4242))
            .expect("open");
        store.remove("pty-c").expect("first remove");
        store.remove("pty-c").expect("second remove is a no-op");
        assert!(!store.dir_for("pty-c").exists());
    }

    #[test]
    fn gc_removes_only_old_dirs() {
        let (_guard, store) = store();
        store
            .open("pty-fresh", Path::new("/"), 80, 24, Some(4242))
            .expect("open");
        store
            .open("pty-stale", Path::new("/"), 80, 24, Some(4242))
            .expect("open");
        // Backdate the stale dir's meta mtime well past the cutoff.
        let stale_meta = store.dir_for("pty-stale").join("meta.json");
        let old = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30);
        let file = std::fs::File::options()
            .write(true)
            .open(&stale_meta)
            .expect("open meta");
        file.set_modified(old).expect("backdate mtime");
        drop(file);

        store.gc_older_than(Duration::from_secs(60 * 60 * 24 * 7));
        assert!(store.dir_for("pty-fresh").exists(), "fresh dir survives");
        assert!(!store.dir_for("pty-stale").exists(), "stale dir reaped");
    }
}
