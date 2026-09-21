//! Partial stash — turn a file selection into a `git stash push -- <paths>`.
//!
//! The panel does not run the stash itself. It works out *what* would have to
//! happen, emits [`StashSelectedRequested`], and the host mounts the push
//! dialog and drives `StashPanel`. Same split as `discard_ops`: the panel owns
//! the semantics, the host owns the modal.
//!
//! # Three things the selection alone does not tell you
//!
//! **1. An untracked path in the pathspec REQUIRES `-u`.** Not as a nicety —
//! measured on git 2.55.0, naming an untracked path without `-u` makes git
//! refuse the whole pathspec:
//!
//! ```text
//! error: pathspec ':(literal,prefix:0)fresh.txt' did not match any file(s) known to git
//! ```
//!
//! and exit 1 with **nothing** stashed, so the tracked files selected
//! alongside it do not move either. (The plan predicted a silent skip; the
//! real behaviour is a loud refusal, which is better, but it means the flag
//! is a correctness requirement and not a preference.) So the planner sets
//! `needs_untracked` and the dialog forces the flag on rather than offering
//! it.
//!
//! **2. A staged rename cannot be pathspec'd.** After `git mv a b`, `a` exists
//! in neither the index nor the worktree — only in HEAD — so naming it in a
//! pathspec fails outright:
//!
//! ```text
//! $ git stash push -m x -- ':(literal)a.txt' ':(literal)b.txt'
//! error: pathspec ':(literal,prefix:0)a.txt' did not match any file(s) known to git
//! ```
//!
//! and `git stash push` then exits 1 with **nothing** stashed — including the
//! unrelated files elsewhere in the selection. Naming only `b` is worse: it
//! leaves `D  a.txt` staged, and popping compounds that into `D a.txt` plus an
//! unmerged `DU b.txt`.
//!
//! The sequence that works is to unstage the pair first, which turns the
//! rename into a worktree deletion (`D a.txt`) plus an untracked file
//! (`?? b.txt`) — two things a pathspec *can* match. That is what
//! `rename_pairs` is for, and it is why a rename also forces `-u`: `b` is
//! untracked by the time the push runs.
//!
//! The cost is that the rename's *staging* does not survive: it comes back as
//! an unstaged deletion plus an untracked file. No content is lost and git
//! re-detects the rename on the next `git add`, but the dialog says so rather
//! than letting the user find out.
//!
//! **3. A big selection cannot be chunked.** A stash is one commit, so
//! splitting the argv would split the stash — `stash_push` has no way to
//! page. Past roughly 1 MB of argv the kernel refuses the exec outright and
//! the user gets `failed to spawn git: ArgumentListTooLong`, which explains
//! nothing. [`ARGV_BUDGET_BYTES`] refuses earlier, in words.

use crate::shell::chrome::toast::ToastKind;
use crate::shell::git_panel::GitPanel;
use gpui::{Context, EventEmitter};
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::path::PathBuf;

/// Accumulated pathspec bytes above which a partial stash is refused.
///
/// The real ceiling is the kernel's `ARG_MAX` (1_048_576 on macOS): 12_000
/// paths — 756 KB of argv — spawns fine, 20_000 (1.26 MB) is refused with
/// `E2BIG` before git runs. That failure is clean (nothing stashed, worktree
/// untouched) but its message is an exec error. Refusing at a quarter of the
/// ceiling leaves room for git's own argv (`stash push -u -m <message> --`,
/// plus the environment, which counts against the same limit) and produces a
/// sentence instead.
pub const ARGV_BUDGET_BYTES: usize = 256 * 1024;

/// Per-path argv overhead of the `:(literal)` prefix `stash_push` wraps every
/// pathspec in, plus the NUL the kernel counts between arguments.
const PATHSPEC_OVERHEAD: usize = ":(literal)".len() + 1;

/// A selection resolved into everything the stash needs to run correctly.
///
/// The host subscribes, shows the push dialog scoped to `paths`, and on
/// confirm hands the whole thing to `StashPanel::push_paths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashSelectedRequested {
    /// Every path to name in the pathspec, sorted and deduped. Includes the
    /// original side of each rename in `rename_pairs`.
    pub paths: Vec<PathBuf>,
    /// `-u` is mandatory: the selection contains an untracked path, or a
    /// rename whose target becomes untracked once the rename is unstaged.
    pub needs_untracked: bool,
    /// `(original, current)` for each staged rename, to `unstage_paths`
    /// before pushing and to `stage_paths` again if the push fails. Empty in
    /// the common case.
    pub rename_pairs: Vec<(PathBuf, PathBuf)>,
    /// How many selected paths are untracked *before* any unstaging — what
    /// the dialog's checkbox counts. Renames are excluded: their target is
    /// untracked only as a side effect of the sequence, and counting it would
    /// tell the user about a file they never saw in the untracked section.
    pub untracked_count: usize,
}

impl EventEmitter<StashSelectedRequested> for GitPanel {}

/// Why a selection cannot be stashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashPlanError {
    /// Nothing in the selection is known to the current `GitState` — a stale
    /// selection against a worktree that moved on.
    NothingToStash,
    /// The pathspec would exceed [`ARGV_BUDGET_BYTES`]. Carries the measured
    /// size so the message can name it.
    TooManyPaths { bytes: usize, paths: usize },
}

impl StashPlanError {
    /// One sentence for a toast. Says what to do, not just what failed.
    pub fn message(&self) -> String {
        match self {
            Self::NothingToStash => {
                "Nothing to stash — none of the selected files still have changes.".into()
            }
            Self::TooManyPaths { bytes, paths } => format!(
                "Too many files to stash at once: {paths} paths, {} KB of arguments (limit {} KB). \
                 A stash is a single commit, so this cannot be split — select fewer files, or \
                 stash everything from the panel header instead.",
                bytes / 1024,
                ARGV_BUDGET_BYTES / 1024,
            ),
        }
    }
}

/// Resolve `selected` against `files` into a runnable plan.
///
/// Pure — no `Context`, no repo — so the rename and untracked rules are unit
/// testable without a GPUI app. Paths absent from `files` are dropped rather
/// than passed through: the poller is the only thing that knows what git
/// currently sees, and a pathspec for a path git has never heard of fails the
/// whole push.
pub fn plan_stash_selection(
    files: &[FileStatus],
    selected: &[PathBuf],
) -> Result<StashSelectedRequested, StashPlanError> {
    let mut paths: Vec<PathBuf> = Vec::with_capacity(selected.len());
    let mut rename_pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut untracked_count = 0usize;

    for path in selected {
        let Some(file) = files.iter().find(|f| &f.path == path) else {
            continue;
        };
        paths.push(file.path.clone());

        if is_untracked(file) {
            untracked_count += 1;
        }
        // A rename is only a problem while it is STAGED — that is the state in
        // which the original side exists nowhere a pathspec can reach. An
        // unstaged rename is already a deletion plus an untracked file, which
        // is exactly what the unstage produces, so it needs no help.
        //
        // A COPY is excluded even though it also carries a `rename` field with
        // an `orig_path`. Its source is an ordinary tracked file that still
        // exists in both the index and the worktree, so the destination is
        // pathspec-reachable on its own (verified: `git stash push -- copy.txt`
        // succeeds and leaves the source alone). Treating it like a rename
        // would drag a file the user never selected into the pathspec — and
        // unstage it first — so a staged edit to the source would silently
        // leave their worktree. `C` records only appear when the user has
        // `status.renames=copies` configured, which is exactly the kind of
        // rare setup a bug like this would hide in.
        if let Some(rename) = &file.rename
            && matches!(file.index, IndexStatus::Renamed)
        {
            paths.push(rename.orig_path.clone());
            rename_pairs.push((rename.orig_path.clone(), file.path.clone()));
        }
    }

    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Err(StashPlanError::NothingToStash);
    }

    let bytes: usize = paths
        .iter()
        .map(|p| p.as_os_str().len() + PATHSPEC_OVERHEAD)
        .sum();
    if bytes > ARGV_BUDGET_BYTES {
        return Err(StashPlanError::TooManyPaths {
            bytes,
            paths: paths.len(),
        });
    }

    Ok(StashSelectedRequested {
        // A rename's target is untracked once the pair is unstaged, so the
        // flag is forced by either condition — see the module doc.
        needs_untracked: untracked_count > 0 || !rename_pairs.is_empty(),
        paths,
        rename_pairs,
        untracked_count,
    })
}

/// True when git has never recorded this path. Both columns are checked: a
/// `??` record reports `Untracked` on each, but the index column is the one
/// that decides whether `git stash push` can see the file without `-u`.
fn is_untracked(file: &FileStatus) -> bool {
    matches!(file.index, IndexStatus::Untracked)
        || matches!(file.worktree, WorktreeStatus::Untracked)
}

impl GitPanel {
    /// Ask the host to stash `paths`. Emits nothing (and toasts instead) when
    /// the plan is not runnable — a refusal the user can read beats a dialog
    /// that confirms into an exec error.
    pub fn request_stash_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let files: &[FileStatus] = match self.git_state.as_ref() {
            Some(state) => &state.files,
            None => &[],
        };
        match plan_stash_selection(files, &paths) {
            Ok(request) => cx.emit(request),
            Err(err) => crate::shell::toast::toast(cx, ToastKind::Error, err.message()),
        }
    }

    /// Ask the host to stash the current multi-select. No-op on an empty
    /// selection so the bulk bar's button cannot fire into nothing.
    pub fn request_stash_selected(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = self.selected.iter().cloned().collect();
        self.request_stash_paths(paths, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{RenameInfo, RenameKind};

    fn modified(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )
    }

    fn untracked(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Untracked,
            WorktreeStatus::Untracked,
        )
    }

    fn staged_rename(orig: &str, path: &str) -> FileStatus {
        let mut f = FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Renamed,
            WorktreeStatus::Unmodified,
        );
        f.rename = Some(RenameInfo {
            orig_path: PathBuf::from(orig),
            kind: RenameKind::Rename,
            score: 100,
        });
        f
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn plain_selection_needs_no_untracked_flag() {
        let files = vec![modified("a.txt"), modified("b.txt"), modified("keep.txt")];
        let plan = plan_stash_selection(&files, &[p("b.txt"), p("a.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("a.txt"), p("b.txt")]);
        assert!(!plan.needs_untracked);
        assert!(plan.rename_pairs.is_empty());
        assert_eq!(plan.untracked_count, 0);
    }

    #[test]
    fn an_untracked_path_forces_the_untracked_flag() {
        let files = vec![modified("a.txt"), untracked("new[1].txt")];
        let plan = plan_stash_selection(&files, &[p("a.txt"), p("new[1].txt")]).expect("plan");
        assert!(plan.needs_untracked);
        assert_eq!(plan.untracked_count, 1);
    }

    #[test]
    fn a_staged_rename_contributes_its_original_path_and_forces_the_flag() {
        let files = vec![staged_rename("a.txt", "b.txt"), modified("c.txt")];
        let plan = plan_stash_selection(&files, &[p("b.txt"), p("c.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("a.txt"), p("b.txt"), p("c.txt")]);
        assert_eq!(plan.rename_pairs, vec![(p("a.txt"), p("b.txt"))]);
        // Forced by the rename, but the user is shown no untracked count —
        // `b.txt` is untracked only as a side effect of the unstage.
        assert!(plan.needs_untracked);
        assert_eq!(plan.untracked_count, 0);
    }

    #[test]
    fn a_staged_copy_contributes_only_its_destination() {
        // `C` carries an `orig_path` just like `R` does, but the source is
        // still a normal tracked file — pulling it in would stash a file the
        // user never selected.
        let mut copy = FileStatus::with_status(
            p("copy.txt"),
            IndexStatus::Copied,
            WorktreeStatus::Unmodified,
        );
        copy.rename = Some(RenameInfo {
            orig_path: p("src.txt"),
            kind: RenameKind::Copy,
            score: 100,
        });
        let files = vec![copy, modified("src.txt")];
        let plan = plan_stash_selection(&files, &[p("copy.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("copy.txt")]);
        assert!(plan.rename_pairs.is_empty(), "a copy needs no unstage dance");
        assert!(!plan.needs_untracked);
    }

    #[test]
    fn an_unstaged_rename_needs_no_pair() {
        // ` R` — the rename is only in the worktree, so both sides are already
        // pathspec-reachable and unstaging would be a no-op.
        let mut f = FileStatus::with_status(
            p("b.txt"),
            IndexStatus::Unmodified,
            WorktreeStatus::Renamed,
        );
        f.rename = Some(RenameInfo {
            orig_path: p("a.txt"),
            kind: RenameKind::Rename,
            score: 100,
        });
        let plan = plan_stash_selection(&[f], &[p("b.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("b.txt")]);
        assert!(plan.rename_pairs.is_empty());
        assert!(!plan.needs_untracked);
    }

    #[test]
    fn paths_absent_from_git_state_are_dropped() {
        let files = vec![modified("a.txt")];
        let plan = plan_stash_selection(&files, &[p("a.txt"), p("ghost.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("a.txt")]);
    }

    #[test]
    fn a_wholly_stale_selection_is_refused() {
        let err = plan_stash_selection(&[modified("a.txt")], &[p("ghost.txt")]).unwrap_err();
        assert_eq!(err, StashPlanError::NothingToStash);
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let err = plan_stash_selection(&[modified("a.txt")], &[]).unwrap_err();
        assert_eq!(err, StashPlanError::NothingToStash);
    }

    #[test]
    fn a_selection_over_the_argv_budget_is_refused_with_a_readable_message() {
        // ~120 bytes of path each; enough of them to clear the budget.
        let files: Vec<FileStatus> = (0..3000)
            .map(|i| modified(&format!("{}/file-{i:06}.rs", "nested/".repeat(12))))
            .collect();
        let selected: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let err = plan_stash_selection(&files, &selected).unwrap_err();
        let StashPlanError::TooManyPaths { bytes, paths } = err else {
            panic!("expected TooManyPaths, got {err:?}");
        };
        assert_eq!(paths, 3000);
        assert!(bytes > ARGV_BUDGET_BYTES);
        let msg = StashPlanError::TooManyPaths { bytes, paths }.message();
        assert!(msg.contains("3000 paths"), "{msg}");
        assert!(msg.contains("cannot be split"), "{msg}");
    }

    #[test]
    fn a_selection_just_under_the_budget_is_allowed() {
        let files: Vec<FileStatus> = (0..1000)
            .map(|i| modified(&format!("src/file-{i:06}.rs")))
            .collect();
        let selected: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let plan = plan_stash_selection(&files, &selected).expect("under budget");
        assert_eq!(plan.paths.len(), 1000);
    }

    #[test]
    fn duplicate_selection_entries_collapse() {
        let files = vec![modified("a.txt")];
        let plan = plan_stash_selection(&files, &[p("a.txt"), p("a.txt")]).expect("plan");
        assert_eq!(plan.paths, vec![p("a.txt")]);
    }
}
