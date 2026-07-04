//! Exclude guard: temporarily hides oversized untracked files from
//! `git add --all` by appending a marked block to `.git/info/exclude`,
//! restored byte-exact on drop.

use super::{CheckpointError, Result};
use std::path::{Path, PathBuf};

const START_MARKER: &str = "# >>> oximux checkpoint (auto, do not edit)";
const END_MARKER: &str = "# <<< oximux checkpoint";

/// Untracked files at or above this size are excluded from checkpoints
/// (and consequently not restored by rewind — accepted limitation).
pub(crate) const MAX_UNTRACKED_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) struct ExcludeGuard {
    exclude_path: PathBuf,
    original: Option<Vec<u8>>,
}

impl ExcludeGuard {
    /// `oversized` are repo-root-relative paths to hide. Always constructs a
    /// guard (even for an empty list) so stale blocks from crashed runs get
    /// stripped.
    pub(crate) fn new(git_dir: &Path, oversized: &[String]) -> Result<Self> {
        let info_dir = git_dir.join("info");
        std::fs::create_dir_all(&info_dir).map_err(CheckpointError::io)?;
        let exclude_path = info_dir.join("exclude");

        // A marked block is always ours, so both the restore target and the
        // working base are the STRIPPED content — crash residue from a prior
        // run never survives a guard cycle.
        let original = match std::fs::read(&exclude_path) {
            Ok(bytes) => Some(strip_stale_block(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(CheckpointError::io(e)),
        };

        let mut content = original.clone().unwrap_or_default();
        if !oversized.is_empty() {
            if !content.is_empty() && !content.ends_with(b"\n") {
                content.push(b'\n');
            }
            content.extend_from_slice(START_MARKER.as_bytes());
            content.push(b'\n');
            for path in oversized {
                // Anchor to repo root and escape glob metacharacters so a
                // filename like `foo[1].bin` matches literally.
                content.push(b'/');
                content.extend_from_slice(escape_pathspec(path).as_bytes());
                content.push(b'\n');
            }
            content.extend_from_slice(END_MARKER.as_bytes());
            content.push(b'\n');
        }
        std::fs::write(&exclude_path, &content).map_err(CheckpointError::io)?;

        Ok(Self {
            exclude_path,
            original,
        })
    }
}

impl Drop for ExcludeGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(bytes) => {
                let _ = std::fs::write(&self.exclude_path, bytes);
            }
            None => {
                let _ = std::fs::remove_file(&self.exclude_path);
            }
        }
    }
}

fn strip_stale_block(content: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(content);
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        if line == START_MARKER {
            inside = true;
            continue;
        }
        if line == END_MARKER {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.into_bytes()
}

fn escape_pathspec(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\' | '!' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_block_and_restores_original() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        std::fs::write(&exclude, b"target/\n").unwrap();

        {
            let _g = ExcludeGuard::new(dir.path(), &["big.bin".into()]).unwrap();
            let live = std::fs::read_to_string(&exclude).unwrap();
            assert!(live.contains("target/"), "user content kept");
            assert!(live.contains("/big.bin"), "oversized path added");
            assert!(live.contains(START_MARKER));
        }
        assert_eq!(std::fs::read(&exclude).unwrap(), b"target/\n");
    }

    #[test]
    fn removes_file_it_created_and_strips_stale() {
        let dir = tempfile::tempdir().unwrap();
        // No pre-existing exclude file. Simulate a stale block by creating one.
        {
            let _g = ExcludeGuard::new(dir.path(), &["a.bin".into()]).unwrap();
            // Simulate crash: forget the guard so drop never restores.
            std::mem::forget(_g);
        }
        let exclude = dir.path().join("info/exclude");
        assert!(std::fs::read_to_string(&exclude).unwrap().contains("a.bin"));

        {
            let _g = ExcludeGuard::new(dir.path(), &[]).unwrap();
            let live = std::fs::read_to_string(&exclude).unwrap();
            assert!(!live.contains("a.bin"), "stale block stripped");
        }
        let after = std::fs::read_to_string(&exclude).unwrap();
        assert!(!after.contains("a.bin"), "stale block gone after drop too");
    }

    #[test]
    fn escapes_glob_metacharacters() {
        assert_eq!(escape_pathspec("f[1] *?.bin"), "f\\[1\\] \\*\\?.bin");
    }
}
