//! Pure-unit tests for worktree row label + path suggestion.

use oximux_app::shell::worktree_panel::list_render::{row_label, suggest_worktree_path};
use oximux_core::WorktreeInfo;
use std::path::{Path, PathBuf};

fn wt(path: &str, branch: Option<&str>, is_main: bool, is_locked: bool) -> WorktreeInfo {
    WorktreeInfo {
        path: PathBuf::from(path),
        branch: branch.map(str::to_string),
        head: "abc1234".to_string(),
        is_main,
        is_locked,
    }
}

#[test]
fn suggest_path_uses_main_parent() {
    let suggestion = suggest_worktree_path(Path::new("/home/dev/proj"), "feat");
    // Built with `join` rather than spelled out: the suggestion uses the
    // platform separator (`\` on Windows), and that's correct — only the
    // "sibling of the main worktree, named oximux-wt-<slug>" shape is the
    // contract here.
    let expected = Path::new("/home/dev").join("oximux-wt-feat");
    assert_eq!(Path::new(&suggestion), expected.as_path());
}

#[test]
fn suggest_path_falls_back_when_no_parent() {
    let suggestion = suggest_worktree_path(Path::new("/"), "feat");
    assert_eq!(suggestion, "./oximux-wt-feat");
}

#[test]
fn suggest_path_handles_trailing_slash_input() {
    // Path::parent on "/foo" returns Some("/"), which is non-empty — so we
    // get "/oximux-wt-x". That's intentional: the user can override.
    let suggestion = suggest_worktree_path(Path::new("/foo"), "x");
    assert_eq!(suggestion, "/oximux-wt-x");
}

#[test]
fn row_label_main_worktree_has_main_marker() {
    let w = wt("/repo", Some("main"), true, false);
    assert_eq!(row_label(&w), "[main]  /repo  (main)");
}

#[test]
fn row_label_linked_worktree_omits_main_marker() {
    let w = wt("/wt-feat", Some("oximux/feat"), false, false);
    assert_eq!(row_label(&w), "[oximux/feat]  /wt-feat");
}

#[test]
fn row_label_detached_head_renders_label() {
    let w = wt("/wt-detached", None, false, false);
    assert_eq!(row_label(&w), "[detached HEAD]  /wt-detached");
}

#[test]
fn row_label_locked_appends_glyph() {
    let w = wt("/wt-locked", Some("oximux/x"), false, true);
    assert_eq!(row_label(&w), "[oximux/x]  /wt-locked  🔒");
}

#[test]
fn row_label_main_and_locked_both_appear() {
    let w = wt("/repo", Some("main"), true, true);
    assert_eq!(row_label(&w), "[main]  /repo  (main)  🔒");
}
