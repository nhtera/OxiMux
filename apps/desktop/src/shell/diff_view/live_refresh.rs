//! Deciding whether an open diff is showing something that can still change.
//!
//! A diff tab was a snapshot: it fetched once and then held whatever it got.
//! Everything that edits the files underneath it — the user in another editor,
//! an agent mid-turn, a `git stash` in a terminal — left the tab showing code
//! that had stopped existing, with no cue that it had. The SCM list beside it
//! updated (it rides a 500 ms status poller), which made the stale tab look
//! authoritative rather than old.
//!
//! Refreshing means re-running the same query the tab was opened with, so the
//! only real question is *which* tabs have a query worth re-running. A diff of
//! the working tree does. A diff of a commit does not: it is addressed by a
//! SHA, and re-fetching it every couple of seconds would spend a git process
//! to be told the same thing forever.
//!
//! That decision is here, on its own, because it is a table of claims about
//! what each diff scope is — not something to re-derive inside a timer.

use std::path::PathBuf;

use oximux_core::{CombinedDiff, CombinedDiffScope, FileDiff};

use super::DiffViewState;

/// The query that would re-fetch what a diff tab is currently showing.
///
/// Mirrors the two fetching load paths — [`super::DiffView::load`] and
/// [`super::DiffView::load_combined`] — because a refresh is exactly those
/// queries run again, never a different or cheaper one. Asking the same
/// question is what makes the answer comparable to what is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveQuery {
    Single {
        path: PathBuf,
        staged: bool,
        untracked: bool,
    },
    Combined {
        scope: CombinedDiffScope,
    },
}

/// What a refresh fetch came back with.
///
/// `Unavailable` folds every way a background fetch can fail into one
/// declined answer, on purpose: the caller's only options are "adopt this" or
/// "leave the view alone", and no error string changes which one it picks.
#[derive(Debug)]
pub enum LiveResult {
    Single(Vec<FileDiff>),
    Combined(CombinedDiff),
    /// The untracked file this view was opened on is no longer on disk.
    ///
    /// Separated from `Unavailable` because it is an answer rather than a
    /// failure to get one, and the two want opposite handling: a git error is
    /// ignored so a transient hiccup cannot wreck a readable view, while a
    /// deleted file means everything on screen has stopped being true and
    /// leaving it up would be the original bug in miniature.
    ///
    /// Only untracked files need this. A tracked file that is deleted still
    /// has a diff — the deletion — so the ordinary path already tells the
    /// truth about it.
    Gone,
    Unavailable,
}

impl LiveQuery {
    /// The query to re-run for `state`, or `None` when the state has nothing
    /// that can change out from under it.
    ///
    /// The exclusions are the interesting half:
    ///
    /// * **Commit and range views** are addressed by revision. Their content
    ///   is fixed for as long as the tab names the same revisions, so polling
    ///   them would burn a git process per tick to re-read history.
    /// * **A turn diff** never came from git at all — the agent chat handed
    ///   the bytes over, and `diff_combined` rejects the scope by design.
    ///   There is no query to re-run, only a way to get an internal error.
    /// * **Loading** states already have a fetch in flight; a second one would
    ///   race it and could apply the older answer.
    /// * **Failed** states are left alone on purpose. Retry is a button the
    ///   user presses. A background retry every couple of seconds would hammer
    ///   a repo that is mid-rebase or mid-`gc`, and a view that flapped between
    ///   an error and a diff would be worse than one that stayed honest about
    ///   having failed.
    pub fn for_state(state: &DiffViewState) -> Option<LiveQuery> {
        match state {
            DiffViewState::Ready {
                path,
                staged,
                untracked,
                ..
            } => Some(LiveQuery::Single {
                path: path.clone(),
                staged: *staged,
                untracked: *untracked,
            }),
            DiffViewState::CombinedReady { scope, .. } if scope_is_live(scope) => {
                Some(LiveQuery::Combined {
                    scope: scope.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Whether a combined scope reads the working tree (so it can change) or a
/// fixed set of revisions (so it cannot).
fn scope_is_live(scope: &CombinedDiffScope) -> bool {
    match scope {
        CombinedDiffScope::AllChanges
        | CombinedDiffScope::Unstaged
        | CombinedDiffScope::Staged
        | CombinedDiffScope::Untracked => true,
        // `base..head` is history. It moves only when the user commits, which
        // is not something a poll should be watching for.
        CombinedDiffScope::Branch { .. } => false,
        // Not a slice of the repo — bytes the caller already had.
        CombinedDiffScope::TurnDiff { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_file_diff_is_live() {
        let state = DiffViewState::Ready {
            path: PathBuf::from("src/a.rs"),
            staged: false,
            untracked: false,
            diffs: Vec::new(),
            expanded: false,
        };
        assert_eq!(
            LiveQuery::for_state(&state),
            Some(LiveQuery::Single {
                path: PathBuf::from("src/a.rs"),
                staged: false,
                untracked: false,
            })
        );
    }

    #[test]
    fn the_query_carries_the_routing_the_original_fetch_used() {
        // An untracked file is fetched through a different repo call than a
        // tracked one; a refresh that forgot which would re-fetch the wrong
        // thing, or nothing.
        let state = DiffViewState::Ready {
            path: PathBuf::from("new.txt"),
            staged: true,
            untracked: true,
            diffs: Vec::new(),
            expanded: false,
        };
        assert_eq!(
            LiveQuery::for_state(&state),
            Some(LiveQuery::Single {
                path: PathBuf::from("new.txt"),
                staged: true,
                untracked: true,
            })
        );
    }

    #[test]
    fn every_working_tree_scope_is_live() {
        for scope in [
            CombinedDiffScope::AllChanges,
            CombinedDiffScope::Unstaged,
            CombinedDiffScope::Staged,
            CombinedDiffScope::Untracked,
        ] {
            let state = DiffViewState::CombinedReady {
                scope: scope.clone(),
                diffs: Vec::new(),
                groups: Vec::new(),
                expanded: false,
            };
            assert_eq!(
                LiveQuery::for_state(&state),
                Some(LiveQuery::Combined { scope: scope.clone() }),
                "{scope:?} reads the working tree and must refresh"
            );
        }
    }

    #[test]
    fn history_is_not_polled() {
        let branch = DiffViewState::CombinedReady {
            scope: CombinedDiffScope::Branch {
                base: "main".into(),
                head: "HEAD".into(),
            },
            diffs: Vec::new(),
            groups: Vec::new(),
            expanded: false,
        };
        assert_eq!(LiveQuery::for_state(&branch), None);

        let commit = DiffViewState::CommitReady {
            sha: "abc123".into(),
            short_oid: "abc123".into(),
            subject: "a commit".into(),
            diffs: Vec::new(),
            expanded: false,
        };
        assert_eq!(LiveQuery::for_state(&commit), None);

        let range = DiffViewState::RangeReady {
            base: "main".into(),
            head: "HEAD".into(),
            path: PathBuf::from("a.rs"),
            title: "a.rs".into(),
            diffs: Vec::new(),
            expanded: false,
        };
        assert_eq!(LiveQuery::for_state(&range), None);
    }

    #[test]
    fn a_turn_diff_has_no_query_to_re_run() {
        // Its bytes came from the agent chat. Re-fetching would ask git for a
        // scope it is documented to reject.
        let state = DiffViewState::CombinedReady {
            scope: CombinedDiffScope::TurnDiff {
                key: "turn-7".into(),
            },
            diffs: Vec::new(),
            groups: Vec::new(),
            expanded: false,
        };
        assert_eq!(LiveQuery::for_state(&state), None);
    }

    #[test]
    fn a_fetch_already_in_flight_is_not_raced() {
        let state = DiffViewState::Loading {
            path: PathBuf::from("a.rs"),
            staged: false,
            untracked: false,
        };
        assert_eq!(LiveQuery::for_state(&state), None);

        let combined = DiffViewState::CombinedLoading {
            scope: CombinedDiffScope::AllChanges,
        };
        assert_eq!(LiveQuery::for_state(&combined), None);
    }

    #[test]
    fn a_failure_waits_for_the_user_to_retry() {
        let state = DiffViewState::Failed {
            path: PathBuf::from("a.rs"),
            staged: false,
            untracked: false,
            error: "git exploded".into(),
        };
        assert_eq!(
            LiveQuery::for_state(&state),
            None,
            "a background retry loop would hammer a repo that is mid-rebase"
        );
    }

    #[test]
    fn an_empty_view_has_nothing_to_refresh() {
        assert_eq!(LiveQuery::for_state(&DiffViewState::Empty), None);
    }
}
