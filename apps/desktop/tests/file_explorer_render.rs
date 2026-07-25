//! Unit tests for `row_render::build_row_plan`.

use oximux_app::shell::file_explorer::row_render::{NodeIcon, build_row_plan};
use oximux_app::shell::file_explorer::status_display::BadgeStatus;
use oximux_app::shell::file_explorer::tree_state::TreeNode;
use std::path::PathBuf;

fn file_node(name: &str, depth: usize) -> TreeNode {
    TreeNode {
        name: name.to_string(),
        path: PathBuf::from(format!("/repo/{name}")),
        relative_path: PathBuf::from(name),
        is_directory: false,
        depth,
    }
}

fn dir_node(name: &str, depth: usize) -> TreeNode {
    TreeNode {
        name: name.to_string(),
        path: PathBuf::from(format!("/repo/{name}")),
        relative_path: PathBuf::from(name),
        is_directory: true,
        depth,
    }
}

#[test]
fn file_no_badge_returns_file_icon_no_badge() {
    let node = file_node("lib.rs", 0);
    let plan = build_row_plan(&node, false, false, None, None);
    assert_eq!(plan.icon, NodeIcon::File);
    assert_eq!(plan.badge, None);
    assert!(!plan.selected);
    assert!(!plan.italic_dim);
    assert_eq!(plan.depth, 0);
    assert_eq!(plan.name, "lib.rs");
}

#[test]
fn file_with_modified_badge() {
    let node = file_node("main.rs", 1);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Modified), None);
    assert_eq!(plan.icon, NodeIcon::File);
    assert_eq!(plan.badge, Some((BadgeStatus::Modified, "M")));
    assert!(!plan.italic_dim);
}

#[test]
fn file_with_added_badge() {
    let node = file_node("new.rs", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Added), None);
    assert_eq!(plan.badge, Some((BadgeStatus::Added, "A")));
}

#[test]
fn file_with_deleted_badge() {
    let node = file_node("gone.rs", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Deleted), None);
    assert_eq!(plan.badge, Some((BadgeStatus::Deleted, "D")));
}

#[test]
fn file_ignored_sets_italic_dim_and_no_badge() {
    // M2: ignored files are italic+dim with NO badge label.
    let node = file_node("ignored.log", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Ignored), None);
    assert!(plan.italic_dim, "ignored file should be italic_dim");
    assert!(
        plan.badge.is_none(),
        "ignored file must not have a badge (M2)"
    );
}

#[test]
fn folder_closed_when_not_expanded() {
    let node = dir_node("src", 0);
    let plan = build_row_plan(&node, false, false, None, None);
    assert_eq!(plan.icon, NodeIcon::FolderClosed);
    assert_eq!(plan.badge, None);
    assert!(!plan.italic_dim);
}

#[test]
fn folder_open_when_expanded() {
    let node = dir_node("src", 0);
    let plan = build_row_plan(&node, true, false, None, Some(BadgeStatus::Modified));
    assert_eq!(plan.icon, NodeIcon::FolderOpen);
    assert_eq!(plan.badge, Some((BadgeStatus::Modified, "M")));
}

#[test]
fn folder_uses_folder_status_ignores_file_status() {
    let node = dir_node("crates", 0);
    let plan = build_row_plan(
        &node,
        false,
        false,
        Some(BadgeStatus::Deleted), // file_status — must be ignored for dirs
        Some(BadgeStatus::Added),   // folder_status — must be used
    );
    assert_eq!(plan.badge, Some((BadgeStatus::Added, "A")));
}

#[test]
fn selected_flag_propagated_to_plan() {
    let node = file_node("main.rs", 0);
    let plan = build_row_plan(&node, false, true, None, None);
    assert!(plan.selected);
}

#[test]
fn depth_propagated_from_node() {
    let node = file_node("deep.rs", 3);
    let plan = build_row_plan(&node, false, false, None, None);
    assert_eq!(plan.depth, 3);
}

#[test]
fn plan_equality_check() {
    let node = file_node("a.rs", 0);
    let p1 = build_row_plan(&node, false, false, None, None);
    let p2 = build_row_plan(&node, false, false, None, None);
    assert_eq!(p1, p2);
}

#[test]
fn renamed_badge_label_is_r() {
    let node = file_node("moved.rs", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Renamed), None);
    assert_eq!(plan.badge, Some((BadgeStatus::Renamed, "R")));
}

#[test]
fn untracked_badge_label_is_u() {
    let node = file_node("new_file.rs", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Untracked), None);
    assert_eq!(plan.badge, Some((BadgeStatus::Untracked, "U")));
}

#[test]
fn copied_badge_label_is_c() {
    let node = file_node("copy.rs", 0);
    let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Copied), None);
    assert_eq!(plan.badge, Some((BadgeStatus::Copied, "C")));
}
