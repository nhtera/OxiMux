//! Stage / unstage operations — file-level and hunk-level.
//!
//! File ops wrap `git add` / `git restore --staged` / `git restore`.
//! Hunk ops rebuild a minimal unified-diff patch from a `FileDiff` plus a
//! selection of hunk indices and feed it to `git apply --cached -` (forward
//! to stage from worktree, `--reverse` to unstage from the index).
//!
//! Hunk staging is what makes interactive review viable. The patch builder
//! is the load-bearing piece — see `build_patch` for the contract.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::{DiffLineKind, DiffStatus, FileDiff};
use std::path::Path;

impl Repository {
    /// Stage entire files (equivalent to `git add -- <paths>`).
    pub async fn stage_paths(&self, paths: &[&Path]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["add", "--"]);
        for p in paths {
            cmd = cmd.arg(p.as_os_str());
        }
        cmd.run().await?;
        Ok(())
    }

    /// Unstage entire files (equivalent to `git restore --staged -- <paths>`).
    pub async fn unstage_paths(&self, paths: &[&Path]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["restore", "--staged", "--"]);
        for p in paths {
            cmd = cmd.arg(p.as_os_str());
        }
        cmd.run().await?;
        Ok(())
    }

    /// Discard worktree changes — DESTRUCTIVE. Equivalent to
    /// `git restore -- <paths>`. Caller is responsible for the type-to-confirm
    /// UX; this method has no guard.
    ///
    /// **Does NOT delete untracked files.** `git restore` operates on
    /// the index, so a pathspec for an untracked path errors with
    /// "pathspec did not match any file(s) known to git". Use
    /// [`delete_untracked_paths`] for the Delete-an-untracked-file
    /// flavour of "discard".
    ///
    /// [`delete_untracked_paths`]: Self::delete_untracked_paths
    pub async fn discard_paths(&self, paths: &[&Path]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["restore", "--"]);
        for p in paths {
            cmd = cmd.arg(p.as_os_str());
        }
        cmd.run().await?;
        Ok(())
    }

    /// Permanently delete untracked files from the worktree —
    /// DESTRUCTIVE. Equivalent to `git clean -f -- <paths>`. Caller
    /// is responsible for the type-to-confirm UX.
    ///
    /// Uses `git clean` instead of `std::fs::remove_file` so:
    /// - `clean.requireForce` config still gates accidental wipes
    ///   (mode `-f` is explicit here; users with `requireForce=true`
    ///   already opted in for everything else)
    /// - symlinks are unlinked, not followed
    /// - the same `GIT_CONFIG_NOSYSTEM` + tracing wrappers `GitCmd`
    ///   sets apply
    ///
    /// Paths in the index are silently ignored by `git clean`; the
    /// caller (`GitPanel::confirmed_discard_area(Untracked, …)` and
    /// `confirmed_discard_path` for pure-untracked rows) is expected
    /// to pass only paths whose `FileStatus.index == Untracked` AND
    /// `worktree == Untracked`. Staged-added rows (`is_delete` per
    /// `discard_confirm`) should NOT route here — they go through
    /// `discard_paths` instead since their index entry needs to clear
    /// first.
    pub async fn delete_untracked_paths(&self, paths: &[&Path]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut cmd = GitCmd::new(self.workdir()).args(["clean", "-f", "--"]);
        for p in paths {
            cmd = cmd.arg(p.as_os_str());
        }
        cmd.run().await?;
        Ok(())
    }

    /// Stage selected hunks from an unstaged diff.
    ///
    /// `file` must come from `diff_unstaged()` or `diff_for_path(path, false)`.
    /// `indices` are 0-based into `file.hunks`. Returns `InvalidInput` when
    /// the selection is empty, any index is out of range, the file is
    /// `Binary`, or `file.hunks` is empty (pure rename / mode-change-only —
    /// use `stage_paths` instead).
    ///
    /// **Note on `ModeChanged` files with hunks**: only the content hunks
    /// are staged; the mode-bit change itself is not propagated (no
    /// `old mode` / `new mode` extended header is emitted). Use
    /// `stage_paths` to stage a file's full mode change.
    pub async fn stage_hunks(&self, file: &FileDiff, indices: &[usize]) -> Result<()> {
        let patch = build_patch(file, indices)?;
        GitCmd::new(self.workdir())
            .args(["apply", "--cached", "-"])
            .stdin(patch)
            .run()
            .await?;
        Ok(())
    }

    /// Unstage selected hunks from a staged diff.
    ///
    /// `file` must come from `diff_staged()` or `diff_for_path(path, true)`.
    /// Symmetric to `stage_hunks` but `--reverse` so the patch removes the
    /// change from the index. The rename itself (if `file.status` is
    /// `Renamed`) is **not** reversed — only the content hunks are.
    pub async fn unstage_hunks(&self, file: &FileDiff, indices: &[usize]) -> Result<()> {
        let patch = build_patch(file, indices)?;
        GitCmd::new(self.workdir())
            .args(["apply", "--cached", "--reverse", "-"])
            .stdin(patch)
            .run()
            .await?;
        Ok(())
    }
}

/// Reconstruct a minimal unified-diff patch from a `FileDiff` and the
/// indices of the hunks to include. Output is acceptable to
/// `git apply --cached -` (and `--reverse` for the unstage direction).
///
/// Rejects:
/// - Empty `indices`
/// - Any index `>= file.hunks.len()`
/// - `DiffStatus::Binary` (no hunk body to reconstruct)
///
/// Pure (no I/O); separated from `stage_hunks` for fast unit testing.
pub(crate) fn build_patch(file: &FileDiff, indices: &[usize]) -> Result<Vec<u8>> {
    if indices.is_empty() {
        return Err(GitError::invalid_input("no hunk indices selected"));
    }
    if matches!(file.status, DiffStatus::Binary) {
        return Err(GitError::invalid_input(
            "cannot hunk-stage a binary diff — use stage_paths instead",
        ));
    }
    if file.hunks.is_empty() {
        return Err(GitError::invalid_input(
            "file has no hunks to stage (pure rename / mode change?)",
        ));
    }
    for &i in indices {
        if i >= file.hunks.len() {
            return Err(GitError::invalid_input(format!(
                "hunk index {i} out of range (have {})",
                file.hunks.len()
            )));
        }
    }

    let path_str = file.path.to_string_lossy();
    let (old_label, new_label, header_old_path) = match &file.status {
        DiffStatus::Added => ("/dev/null".to_string(), format!("b/{path_str}"), None),
        DiffStatus::Deleted => (format!("a/{path_str}"), "/dev/null".to_string(), None),
        DiffStatus::Renamed { from, .. } | DiffStatus::Copied { from, .. } => {
            let from_str = from.to_string_lossy().into_owned();
            (
                format!("a/{from_str}"),
                format!("b/{path_str}"),
                Some(from_str),
            )
        }
        _ => (format!("a/{path_str}"), format!("b/{path_str}"), None),
    };

    let mut out: Vec<u8> = Vec::new();
    // `diff --git` line: for renames the old path is the source; for adds the
    // old path is still the new path (git's own format quirk — it's the
    // `--- /dev/null` line that signals the add).
    let diff_old = header_old_path.as_deref().unwrap_or(&path_str);
    writeln_to_vec(
        &mut out,
        format_args!("diff --git a/{diff_old} b/{path_str}"),
    );
    // Extended headers — REQUIRED by `git apply` for delete / add to be
    // interpreted as file-mode-change-to-absent rather than a literal
    // `dev/null` path being created. Mode defaults to 100644 because
    // `FileDiff` doesn't (yet) carry the actual file mode; if the source was
    // 100755 the worst case is the staged deletion has a slightly wrong mode
    // recorded in the patch — git's deletion logic doesn't actually use it.
    // FIXME(v1.1): plumb actual file mode through `DiffStatus::Added` /
    // `DiffStatus::Deleted` so executables round-trip correctly.
    match &file.status {
        DiffStatus::Added => {
            writeln_to_vec(&mut out, format_args!("new file mode 100644"));
        }
        DiffStatus::Deleted => {
            writeln_to_vec(&mut out, format_args!("deleted file mode 100644"));
        }
        DiffStatus::Renamed { from, similarity } | DiffStatus::Copied { from, similarity } => {
            // Without these, `git apply` treats the patch as a plain modify
            // of the new path and leaves the old path on disk.
            writeln_to_vec(&mut out, format_args!("similarity index {similarity}%"));
            let from_str = from.to_string_lossy();
            let verb = if matches!(file.status, DiffStatus::Copied { .. }) {
                "copy"
            } else {
                "rename"
            };
            writeln_to_vec(&mut out, format_args!("{verb} from {from_str}"));
            writeln_to_vec(&mut out, format_args!("{verb} to {path_str}"));
        }
        _ => {}
    }
    writeln_to_vec(&mut out, format_args!("--- {old_label}"));
    writeln_to_vec(&mut out, format_args!("+++ {new_label}"));

    for &i in indices {
        let h = &file.hunks[i];
        // Reconstruct the hunk header. Use `,n` form even when n == 1 so the
        // shape is uniform — `git apply` accepts both.
        let suffix = if h.header_suffix.is_empty() {
            String::new()
        } else {
            format!(" {}", h.header_suffix)
        };
        writeln_to_vec(
            &mut out,
            format_args!(
                "@@ -{},{} +{},{} @@{}",
                h.old_start, h.old_lines, h.new_start, h.new_lines, suffix
            ),
        );
        for line in &h.lines {
            match line.kind {
                DiffLineKind::Context => {
                    out.push(b' ');
                    out.extend_from_slice(line.content.as_bytes());
                    out.push(b'\n');
                }
                DiffLineKind::Added => {
                    out.push(b'+');
                    out.extend_from_slice(line.content.as_bytes());
                    out.push(b'\n');
                }
                DiffLineKind::Removed => {
                    out.push(b'-');
                    out.extend_from_slice(line.content.as_bytes());
                    out.push(b'\n');
                }
                DiffLineKind::NoNewlineHint => {
                    // Parser strips the leading `\ ` and stores
                    // `"No newline at end of file"`. Re-emit verbatim.
                    out.extend_from_slice(b"\\ ");
                    out.extend_from_slice(line.content.as_bytes());
                    out.push(b'\n');
                }
            }
        }
    }

    Ok(out)
}

/// Tiny helper — same as `writeln!(&mut Vec<u8>, ...)` but without the result
/// noise. Writing to a `Vec<u8>` never fails, so we discard the io::Result.
fn writeln_to_vec(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(out, "{args}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{DiffHunk, DiffLine, DiffStatus, FileDiff};
    use std::path::PathBuf;

    fn modified(path: &str, hunks: Vec<DiffHunk>) -> FileDiff {
        FileDiff {
            path: PathBuf::from(path),
            status: DiffStatus::Modified,
            hunks,
            large: false,
        }
    }

    fn hunk(old_start: u32, new_start: u32, lines: Vec<DiffLine>) -> DiffHunk {
        let old_lines = lines
            .iter()
            .filter(|l| matches!(l.kind, DiffLineKind::Context | DiffLineKind::Removed))
            .count() as u32;
        let new_lines = lines
            .iter()
            .filter(|l| matches!(l.kind, DiffLineKind::Context | DiffLineKind::Added))
            .count() as u32;
        DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            header_suffix: String::new(),
            lines,
        }
    }

    fn line(kind: DiffLineKind, content: &str) -> DiffLine {
        DiffLine {
            kind,
            content: content.to_string(),
        }
    }

    #[test]
    fn rejects_empty_selection() {
        let f = modified("x.rs", vec![hunk(1, 1, vec![])]);
        let err = build_patch(&f, &[]).unwrap_err();
        assert!(matches!(err, GitError::InvalidInput { .. }));
    }

    #[test]
    fn rejects_out_of_range_index() {
        let f = modified("x.rs", vec![hunk(1, 1, vec![])]);
        let err = build_patch(&f, &[1]).unwrap_err();
        assert!(matches!(err, GitError::InvalidInput { .. }));
    }

    #[test]
    fn rejects_binary() {
        let f = FileDiff {
            path: PathBuf::from("logo.png"),
            status: DiffStatus::Binary,
            hunks: vec![],
            large: false,
        };
        let err = build_patch(&f, &[0]).unwrap_err();
        assert!(matches!(err, GitError::InvalidInput { .. }));
    }

    #[test]
    fn rejects_no_hunks() {
        let f = FileDiff {
            path: PathBuf::from("renamed.rs"),
            status: DiffStatus::Renamed {
                from: PathBuf::from("old.rs"),
                similarity: 100,
            },
            hunks: vec![],
            large: false,
        };
        let err = build_patch(&f, &[0]).unwrap_err();
        assert!(matches!(err, GitError::InvalidInput { .. }));
    }

    #[test]
    fn modified_one_hunk_roundtrip_shape() {
        let f = modified(
            "src/a.rs",
            vec![hunk(
                1,
                1,
                vec![
                    line(DiffLineKind::Context, "fn main() {"),
                    line(DiffLineKind::Removed, "    old();"),
                    line(DiffLineKind::Added, "    new();"),
                    line(DiffLineKind::Context, "}"),
                ],
            )],
        );
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        let expected = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
";
        assert_eq!(s, expected);
    }

    #[test]
    fn deleted_emits_devnull_target() {
        let f = FileDiff {
            path: PathBuf::from("dead.rs"),
            status: DiffStatus::Deleted,
            hunks: vec![hunk(
                1,
                0,
                vec![line(DiffLineKind::Removed, "fn dead() {}")],
            )],
            large: false,
        };
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("--- a/dead.rs\n+++ /dev/null\n"), "got: {s}");
    }

    #[test]
    fn added_emits_devnull_source() {
        let f = FileDiff {
            path: PathBuf::from("new.rs"),
            status: DiffStatus::Added,
            hunks: vec![hunk(0, 1, vec![line(DiffLineKind::Added, "fn new() {}")])],
            large: false,
        };
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("--- /dev/null\n+++ b/new.rs\n"), "got: {s}");
    }

    #[test]
    fn renamed_with_hunks_uses_from_path_on_minus_side() {
        let f = FileDiff {
            path: PathBuf::from("new_name.rs"),
            status: DiffStatus::Renamed {
                from: PathBuf::from("old_name.rs"),
                similarity: 80,
            },
            hunks: vec![hunk(
                1,
                1,
                vec![
                    line(DiffLineKind::Removed, "old"),
                    line(DiffLineKind::Added, "new"),
                ],
            )],
            large: false,
        };
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("diff --git a/old_name.rs b/new_name.rs\n"));
        assert!(s.contains("--- a/old_name.rs\n+++ b/new_name.rs\n"));
    }

    #[test]
    fn no_newline_eof_marker_round_trips() {
        let f = modified(
            "tail.txt",
            vec![hunk(
                1,
                1,
                vec![
                    line(DiffLineKind::Removed, "old line"),
                    line(DiffLineKind::NoNewlineHint, "No newline at end of file"),
                    line(DiffLineKind::Added, "new line"),
                    line(DiffLineKind::NoNewlineHint, "No newline at end of file"),
                ],
            )],
        );
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Two markers, exactly as git emits.
        assert_eq!(
            s.matches("\\ No newline at end of file").count(),
            2,
            "got: {s}"
        );
    }

    #[test]
    fn header_suffix_round_trips() {
        let mut h = hunk(
            10,
            10,
            vec![
                line(DiffLineKind::Context, " ctx"),
                line(DiffLineKind::Added, " new"),
            ],
        );
        h.header_suffix = "fn second() {".to_string();
        let f = modified("src/x.rs", vec![h]);
        let bytes = build_patch(&f, &[0]).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("@@ -10,1 +10,2 @@ fn second() {\n"), "got: {s}");
    }
}
