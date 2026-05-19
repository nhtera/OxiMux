//! Row plan builder for the file explorer.
//!
//! Pure module — no GPUI imports. `build_row_plan` maps a `TreeNode` + status
//! context into a `RowPlan` value that the GPUI render layer can paint without
//! further logic.

use crate::shell::file_explorer::status_display::{BadgeStatus, label_for};
use crate::shell::file_explorer::tree_state::TreeNode;
use std::path::PathBuf;

/// Which icon glyph to display for this row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeIcon {
    FolderClosed,
    FolderOpen,
    File,
}

/// All data the renderer needs to paint one explorer row. Pure value — no
/// references into entity state so it can be cheaply cloned into closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPlan {
    pub depth: usize,
    pub icon: NodeIcon,
    pub name: String,
    pub path: PathBuf,
    /// Badge to display at the right edge: `(status, single-letter label)`.
    pub badge: Option<(BadgeStatus, &'static str)>,
    pub selected: bool,
    /// `true` for `Ignored` entries — render italic + dim.
    pub italic_dim: bool,
}

/// Build the display plan for one tree row.
///
/// `file_status` and `folder_status` are looked up from the status maps before
/// calling here so this function stays pure and side-effect-free.
pub fn build_row_plan(
    node: &TreeNode,
    expanded: bool,
    selected: bool,
    file_status: Option<BadgeStatus>,
    folder_status: Option<BadgeStatus>,
) -> RowPlan {
    let icon = if node.is_directory {
        if expanded {
            NodeIcon::FolderOpen
        } else {
            NodeIcon::FolderClosed
        }
    } else {
        NodeIcon::File
    };

    // Pick the relevant status: folders use folder_status, files use file_status.
    let status = if node.is_directory {
        folder_status
    } else {
        file_status
    };

    // M2: Ignored entries show italic+dim but NO badge label.
    let italic_dim = matches!(status, Some(BadgeStatus::Ignored));
    let badge = match status {
        Some(s) if s != BadgeStatus::Ignored => Some((s, label_for(s))),
        _ => None,
    };

    RowPlan {
        depth: node.depth,
        icon,
        name: node.name.clone(),
        path: node.path.clone(),
        badge,
        selected,
        italic_dim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::file_explorer::tree_state::TreeNode;
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
    fn file_no_badge() {
        let node = file_node("lib.rs", 0);
        let plan = build_row_plan(&node, false, false, None, None);
        assert_eq!(plan.icon, NodeIcon::File);
        assert_eq!(plan.badge, None);
        assert!(!plan.selected);
        assert!(!plan.italic_dim);
        assert_eq!(plan.depth, 0);
    }

    #[test]
    fn file_with_modified_badge() {
        let node = file_node("main.rs", 1);
        let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Modified), None);
        assert_eq!(plan.badge, Some((BadgeStatus::Modified, "M")));
        assert!(!plan.italic_dim);
    }

    #[test]
    fn file_ignored_sets_italic_dim_and_no_badge() {
        let node = file_node("ignored.log", 0);
        let plan = build_row_plan(&node, false, false, Some(BadgeStatus::Ignored), None);
        assert!(plan.italic_dim, "ignored file must be italic_dim");
        assert!(plan.badge.is_none(), "ignored file must have no badge (M2)");
    }

    #[test]
    fn folder_closed_when_not_expanded() {
        let node = dir_node("src", 0);
        let plan = build_row_plan(&node, false, false, None, None);
        assert_eq!(plan.icon, NodeIcon::FolderClosed);
        assert_eq!(plan.badge, None);
    }

    #[test]
    fn folder_open_when_expanded() {
        let node = dir_node("src", 0);
        let plan = build_row_plan(&node, true, false, None, Some(BadgeStatus::Modified));
        assert_eq!(plan.icon, NodeIcon::FolderOpen);
        assert_eq!(plan.badge, Some((BadgeStatus::Modified, "M")));
    }

    #[test]
    fn selected_flag_propagated() {
        let node = file_node("main.rs", 0);
        let plan = build_row_plan(&node, false, true, None, None);
        assert!(plan.selected);
    }

    #[test]
    fn folder_uses_folder_status_not_file_status() {
        // file_status should be ignored for directory nodes
        let node = dir_node("crates", 0);
        let plan = build_row_plan(
            &node,
            false,
            false,
            Some(BadgeStatus::Deleted), // file_status — should be ignored
            Some(BadgeStatus::Added),   // folder_status — should be used
        );
        assert_eq!(plan.badge, Some((BadgeStatus::Added, "A")));
    }
}
