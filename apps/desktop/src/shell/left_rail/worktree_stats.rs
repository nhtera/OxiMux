//! Per-worktree git numbers the rail shows on a row, and the pure label
//! helpers that turn them into chips.
//!
//! One value per worktree path, produced by the single batched refresher
//! (`WorkspaceRoot::run_diff_refresh_round`) and read by the row painter.
//! Absence is meaningful: a path with no entry has not been measured yet, or
//! could not be, and the card shows nothing for it. That is why every
//! "unknown" here is an `Option` and never a zero — `↑0 ↓0` on a row whose
//! base could not even be resolved would be a confident lie.

use oximux_git::AheadBehind;

use super::workspace_row::DiffCounts;

/// Everything one refresh round learns about a worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeStats {
    /// Line totals of the working tree against HEAD.
    pub diff: DiffCounts,
    /// Tracked files with a diff against HEAD — the row count of the same
    /// numstat the line totals are summed from, so it costs nothing extra.
    /// Untracked files are not in it: numstat against HEAD does not see them.
    pub dirty_files: u32,
    /// HEAD against the worktree's base, or `None` when no base resolved.
    /// See `oximux_git::ahead_behind` for the resolution order.
    pub ahead_behind: Option<AheadBehind>,
}

/// The `↑N ↓M` chip text. Each arrow is dropped at zero so a branch that is
/// only ahead reads `↑2`, and a branch level with its base yields `None` —
/// nothing to say, so no chip, the same rule the clean-tree diff chip uses.
pub fn ahead_behind_label(ab: &AheadBehind) -> Option<String> {
    match (ab.ahead, ab.behind) {
        (0, 0) => None,
        (a, 0) => Some(format!("↑{a}")),
        (0, b) => Some(format!("↓{b}")),
        (a, b) => Some(format!("↑{a} ↓{b}")),
    }
}

/// The hover text that names the base, so the chip is never ambiguous:
/// `2 ahead, 5 behind origin/main`.
pub fn ahead_behind_tooltip(ab: &AheadBehind) -> String {
    let mut parts = Vec::with_capacity(2);
    if ab.ahead > 0 {
        parts.push(format!("{} ahead", ab.ahead));
    }
    if ab.behind > 0 {
        parts.push(format!("{} behind", ab.behind));
    }
    if parts.is_empty() {
        format!("level with {}", ab.base)
    } else {
        format!("{} {}", parts.join(", "), ab.base)
    }
}

/// The `~N` changed-files chip text; `None` for a clean tree.
pub fn dirty_files_label(files: u32) -> Option<String> {
    (files > 0).then(|| format!("~{files}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ab(ahead: u32, behind: u32) -> AheadBehind {
        AheadBehind {
            base: "origin/main".into(),
            ahead,
            behind,
        }
    }

    #[test]
    fn each_arrow_is_dropped_at_zero_and_both_at_level() {
        assert_eq!(ahead_behind_label(&ab(2, 5)).as_deref(), Some("↑2 ↓5"));
        assert_eq!(ahead_behind_label(&ab(2, 0)).as_deref(), Some("↑2"));
        assert_eq!(ahead_behind_label(&ab(0, 5)).as_deref(), Some("↓5"));
        assert_eq!(ahead_behind_label(&ab(0, 0)), None);
    }

    #[test]
    fn the_tooltip_always_names_the_base() {
        assert_eq!(ahead_behind_tooltip(&ab(2, 5)), "2 ahead, 5 behind origin/main");
        assert_eq!(ahead_behind_tooltip(&ab(0, 1)), "1 behind origin/main");
        assert_eq!(ahead_behind_tooltip(&ab(0, 0)), "level with origin/main");
    }

    #[test]
    fn a_clean_tree_has_no_files_chip() {
        assert_eq!(dirty_files_label(0), None);
        assert_eq!(dirty_files_label(3).as_deref(), Some("~3"));
    }
}
