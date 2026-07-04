//! Temporary-index guard: lets checkpoint commands stage the whole worktree
//! without ever touching the user's real `.git/index` (or its lock).

use super::{CheckpointError, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Copy of the real index at a private path. Pass [`Self::path`] as
/// `GIT_INDEX_FILE` to every command inside the guard's lifetime. The temp
/// file is removed on drop (best-effort; a leaked `oximux-index-*.tmp` in the
/// git dir is harmless and swept by the next guard construction).
pub(crate) struct TempIndexGuard {
    path: PathBuf,
}

impl TempIndexGuard {
    pub(crate) fn new(git_dir: &Path) -> Result<Self> {
        sweep_stale(git_dir);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = git_dir.join(format!(
            "oximux-index-{}-{}.tmp",
            std::process::id(),
            n
        ));
        let real_index = git_dir.join("index");
        if real_index.exists() {
            std::fs::copy(&real_index, &path).map_err(CheckpointError::io)?;
        }
        // Fresh repo has no index yet — commands starting from an absent
        // GIT_INDEX_FILE build an empty one, which is exactly right.
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempIndexGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Remove temp indexes left behind by crashed prior runs. Only files matching
/// our own naming scheme are touched.
fn sweep_stale(git_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(git_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("oximux-index-") && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_existing_index_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index"), b"fake-index").unwrap();
        let temp_path;
        {
            let guard = TempIndexGuard::new(dir.path()).unwrap();
            temp_path = guard.path().to_path_buf();
            assert_eq!(std::fs::read(&temp_path).unwrap(), b"fake-index");
        }
        assert!(!temp_path.exists(), "temp index removed on drop");
        assert!(dir.path().join("index").exists(), "real index untouched");
    }

    #[test]
    fn tolerates_missing_index_and_sweeps_stale() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("oximux-index-999-0.tmp");
        std::fs::write(&stale, b"stale").unwrap();
        let guard = TempIndexGuard::new(dir.path()).unwrap();
        assert!(!stale.exists(), "stale temp index swept");
        assert!(!guard.path().exists(), "no file until git writes one");
    }
}
