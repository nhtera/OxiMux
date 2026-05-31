//! Pure render helpers for workspace cards in the left rail.
//!
//! `build_workspace_row_plan` is the testable boundary — given a
//! `WorktreeInfo` + active flag + theme, it returns the resolved visual
//! tokens that `render_workspace_row` paints. No GPUI runtime needed.

use gpui::{Hsla, IntoElement, ParentElement, Styled, div, px, svg};
use oximux_core::WorktreeInfo;
use oximux_settings::{Density, Theme, Typography};

const ROW_ICON_SIZE: f32 = 14.0;
const ROW_HEIGHT_MULT: f32 = 1.4;

/// Visual tokens for one workspace row. Pure, testable.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRowPlan {
    /// Folder icon tint.
    pub folder_icon_color: Hsla,
    /// Display name (typically the path basename).
    pub name: String,
    /// Branch / HEAD label (`(detached)` when no branch).
    pub branch_label: String,
    /// Whether to show the "primary" badge next to the branch.
    pub show_primary_badge: bool,
    /// Whether to show the remove (×) button on the row.
    pub show_remove_button: bool,
    /// Row background.
    pub bg: Hsla,
    /// Row foreground.
    pub fg: Hsla,
}

/// Strip a leading `refs/heads/` prefix from branch labels.
pub fn strip_refs_prefix(branch: &str) -> &str {
    branch.strip_prefix("refs/heads/").unwrap_or(branch)
}

/// Compute the visual plan for one workspace row.
pub fn build_workspace_row_plan(
    info: &WorktreeInfo,
    is_active: bool,
    theme: Theme,
) -> WorkspaceRowPlan {
    let name = info
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| info.path.display().to_string());

    let branch_label = info
        .branch
        .as_deref()
        .map(|b| strip_refs_prefix(b).to_string())
        .unwrap_or_else(|| "(detached)".to_string());

    let (bg, fg, icon) = if is_active {
        (theme.bg_panel_alt, theme.fg_base, theme.fg_base)
    } else {
        (theme.bg_panel, theme.fg_base, theme.fg_muted)
    };

    WorkspaceRowPlan {
        folder_icon_color: icon,
        name,
        branch_label,
        show_primary_badge: info.is_main,
        show_remove_button: !info.is_main,
        bg,
        fg,
    }
}

/// Render one workspace row from its computed plan.
pub fn render_workspace_row(
    plan: WorkspaceRowPlan,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let primary_badge: Option<gpui::AnyElement> = plan.show_primary_badge.then(|| {
        div()
            .px(px(4.))
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child("primary")
            .into_any_element()
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row * ROW_HEIGHT_MULT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .bg(plan.bg)
        .child(
            svg()
                .path("icons/folder.svg")
                .size(px(ROW_ICON_SIZE))
                .text_color(plan.folder_icon_color),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(plan.fg)
                        .child(plan.name),
                )
                .child({
                    let mut branch_row = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(density.gap_inline))
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(plan.branch_label);
                    if let Some(badge) = primary_badge {
                        branch_row = branch_row.child("•").child(badge);
                    }
                    branch_row
                }),
        )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn info(path: &str, branch: Option<&str>, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            branch: branch.map(|s| s.to_string()),
            head: "abc123".to_string(),
            is_main,
            is_locked: false,
        }
    }

    #[test]
    fn build_plan_main_worktree_shows_primary_badge() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(&info("/tmp/oximux", Some("main"), true), false, t);
        assert!(plan.show_primary_badge);
        assert!(!plan.show_remove_button);
    }

    #[test]
    fn build_plan_non_main_shows_remove_button() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(
            &info("/tmp/oximux-feature", Some("feature/x"), false),
            false,
            t,
        );
        assert!(!plan.show_primary_badge);
        assert!(plan.show_remove_button);
    }

    #[test]
    fn build_plan_active_uses_bg_panel_alt() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(&info("/tmp/x", Some("main"), true), true, t);
        assert_eq!(plan.bg, t.bg_panel_alt);
        assert_eq!(plan.folder_icon_color, t.fg_base);
    }

    #[test]
    fn build_plan_inactive_uses_bg_panel() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(&info("/tmp/x", Some("main"), true), false, t);
        assert_eq!(plan.bg, t.bg_panel);
        assert_eq!(plan.folder_icon_color, t.fg_muted);
    }

    #[test]
    fn build_plan_detached_head_label() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(&info("/tmp/x", None, false), false, t);
        assert_eq!(plan.branch_label, "(detached)");
    }

    #[test]
    fn build_plan_name_uses_path_basename() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(
            &info("/Users/alice/Code/oximux", Some("main"), true),
            false,
            t,
        );
        assert_eq!(plan.name, "oximux");
    }

    #[test]
    fn build_plan_branch_label_strips_refs_prefix() {
        let t = Theme::charcoal();
        let plan = build_workspace_row_plan(
            &info("/tmp/x", Some("refs/heads/feat/polish"), false),
            false,
            t,
        );
        assert_eq!(plan.branch_label, "feat/polish");
    }

    #[test]
    fn strip_refs_prefix_handles_plain_branch() {
        assert_eq!(strip_refs_prefix("main"), "main");
    }

    #[test]
    fn strip_refs_prefix_removes_full_ref() {
        assert_eq!(strip_refs_prefix("refs/heads/main"), "main");
    }

    #[test]
    fn build_plan_handles_trailing_slash_path() {
        let t = Theme::charcoal();
        let mut info = info("/tmp/oximux", Some("main"), true);
        info.path = std::path::PathBuf::from("/tmp/oximux/");
        let plan = build_workspace_row_plan(&info, false, t);
        assert_eq!(plan.name, "oximux");
    }
}
