//! Pure-unit tests for the status bar's git-zone label.

use oximux_app::shell::status_bar::git_zone_label;
use oximux_core::{FileStatus, GitState, IndexStatus, WorktreeStatus};
use oximux_git::{GitError, PollState};

// `GitError::parse` is crate-private; reach the same variant via the
// public Parse fields.
fn parse_err(msg: &str) -> GitError {
    GitError::Parse {
        reason: msg.to_string(),
    }
}
use std::path::PathBuf;

fn fs(index: IndexStatus, worktree: WorktreeStatus) -> FileStatus {
    FileStatus {
        path: PathBuf::from("a"),
        index,
        worktree,
        rename: None,
    }
}

#[test]
fn no_repo_returns_empty_string() {
    assert_eq!(git_zone_label(None), "");
}

#[test]
fn loading_state_returns_loading_label() {
    assert_eq!(git_zone_label(Some(&PollState::Loading)), "loading git…");
}

#[test]
fn failed_state_surfaces_error_prefix() {
    let err = parse_err("boom");
    let label = git_zone_label(Some(&PollState::Failed(err)));
    assert!(label.starts_with("git: "), "got {label:?}");
}

#[test]
fn ready_with_branch_and_no_changes() {
    let state = GitState {
        branch: Some("main".into()),
        ..GitState::default()
    };
    assert_eq!(
        git_zone_label(Some(&PollState::Ready(state))),
        "main  •  0 changed"
    );
}

#[test]
fn ready_detached_head_label() {
    let state = GitState {
        branch: None,
        ..GitState::default()
    };
    let label = git_zone_label(Some(&PollState::Ready(state)));
    assert!(label.starts_with("(detached)"), "got {label:?}");
}

#[test]
fn ready_shows_ahead_behind_when_nonzero() {
    let state = GitState {
        branch: Some("main".into()),
        ahead: 2,
        behind: 1,
        ..GitState::default()
    };
    let label = git_zone_label(Some(&PollState::Ready(state)));
    assert_eq!(label, "main  •  ↑2 ↓1  •  0 changed");
}

#[test]
fn ready_counts_dirty_files_excluding_ignored() {
    let state = GitState {
        branch: Some("main".into()),
        files: vec![
            fs(IndexStatus::Modified, WorktreeStatus::Unmodified),
            fs(IndexStatus::Unmodified, WorktreeStatus::Modified),
            fs(IndexStatus::Ignored, WorktreeStatus::Unmodified),
            fs(IndexStatus::Unmodified, WorktreeStatus::Ignored),
        ],
        ..GitState::default()
    };
    let label = git_zone_label(Some(&PollState::Ready(state)));
    assert!(label.ends_with("2 changed"), "got {label:?}");
}
