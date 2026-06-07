//! Pure render helpers for workspace cards in the left rail.
//!
//! `build_workspace_row_plan` is the testable boundary — given a
//! `WorktreeInfo` + active flag + theme, it returns the resolved visual
//! tokens that `render_workspace_row` paints. No GPUI runtime needed.

use gpui::{Hsla, IntoElement, ParentElement, Styled, div, px, svg};
use oximux_core::{Workspace, WorktreeInfo};
use oximux_settings::{Density, Theme, Typography};

const ROW_ICON_SIZE: f32 = 14.0;
const ROW_HEIGHT_MULT: f32 = 1.4;

/// How workspace rows within a project group are ordered.
///
/// The primary row (the repo-root worktree) is always pinned first in every
/// mode — it's the project's anchor. The remaining worktree rows are ordered
/// per the active mode. The choice is persisted across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceSortMode {
    /// Attention-weighted: workspaces needing action (approval / waiting) or
    /// running float above idle / finished ones. Stable within a tier.
    #[default]
    Smart,
    /// Most recently created first.
    Recent,
    /// Insertion order as stored — no reordering.
    Manual,
}

impl WorkspaceSortMode {
    /// Short label shown on the header toggle chip.
    pub fn label(self) -> &'static str {
        match self {
            WorkspaceSortMode::Smart => "Smart",
            WorkspaceSortMode::Recent => "Recent",
            WorkspaceSortMode::Manual => "Manual",
        }
    }

    /// Stable persistence key.
    pub fn as_key(self) -> &'static str {
        match self {
            WorkspaceSortMode::Smart => "smart",
            WorkspaceSortMode::Recent => "recent",
            WorkspaceSortMode::Manual => "manual",
        }
    }

    /// Parse a persisted key; unknown / missing values fall back to default.
    pub fn from_key(raw: &str) -> WorkspaceSortMode {
        match raw.trim() {
            "recent" => WorkspaceSortMode::Recent,
            "manual" => WorkspaceSortMode::Manual,
            _ => WorkspaceSortMode::Smart,
        }
    }

    /// Next mode in the cycle: Smart → Recent → Manual → Smart.
    pub fn next(self) -> WorkspaceSortMode {
        match self {
            WorkspaceSortMode::Smart => WorkspaceSortMode::Recent,
            WorkspaceSortMode::Recent => WorkspaceSortMode::Manual,
            WorkspaceSortMode::Manual => WorkspaceSortMode::Smart,
        }
    }
}

/// Order one project group's workspace rows for display.
///
/// The primary row (`worktree_path == project_root`) is pinned first; the
/// remaining rows are sorted per `mode`. `attention_for` resolves a row's
/// attention tier (lower = higher priority) — injected so this stays pure and
/// free of the status / live-worktree maps. Sorts are stable, so equal-tier /
/// equal-timestamp rows keep their input order.
///
/// Partition contract: if no row matches `project_root`, all rows fall into
/// the sorted tail. If several rows match (not expected — the synthesized
/// primary is unique), all are pinned first in their input order.
pub fn sort_workspaces(
    workspaces: &[Workspace],
    project_root: &str,
    mode: WorkspaceSortMode,
    attention_for: impl Fn(&Workspace) -> u8,
) -> Vec<Workspace> {
    if mode == WorkspaceSortMode::Manual {
        return workspaces.to_vec();
    }

    let (mut primary, mut rest): (Vec<Workspace>, Vec<Workspace>) = workspaces
        .iter()
        .cloned()
        .partition(|ws| ws.worktree_path == project_root);

    match mode {
        // Stable sort by ascending attention tier floats action-needing rows up.
        WorkspaceSortMode::Smart => rest.sort_by_key(|ws| attention_for(ws)),
        // created_at is an RFC3339 UTC string; lexicographic desc = newest first.
        WorkspaceSortMode::Recent => rest.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
        WorkspaceSortMode::Manual => unreachable!("handled by early return"),
    }

    primary.extend(rest);
    primary
}

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

    // `created_at` literals use the `+00:00` offset suffix to match what the
    // storage layer actually writes (`Utc::now().to_rfc3339()`), so the
    // Recent-sort comparison is exercised against the real value domain.
    fn ws(id: &str, worktree_path: &str, created_at: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: "p1".to_string(),
            name: id.to_string(),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: worktree_path.to_string(),
            status: "active".to_string(),
            created_at: created_at.to_string(),
            archived_at: None,
        }
    }

    #[test]
    fn sort_mode_round_trips_through_key() {
        for mode in [
            WorkspaceSortMode::Smart,
            WorkspaceSortMode::Recent,
            WorkspaceSortMode::Manual,
        ] {
            assert_eq!(WorkspaceSortMode::from_key(mode.as_key()), mode);
        }
    }

    #[test]
    fn sort_mode_unknown_key_falls_back_to_default() {
        assert_eq!(WorkspaceSortMode::from_key("bogus"), WorkspaceSortMode::Smart);
        assert_eq!(WorkspaceSortMode::default(), WorkspaceSortMode::Smart);
    }

    #[test]
    fn sort_mode_cycle_wraps() {
        assert_eq!(WorkspaceSortMode::Smart.next(), WorkspaceSortMode::Recent);
        assert_eq!(WorkspaceSortMode::Recent.next(), WorkspaceSortMode::Manual);
        assert_eq!(WorkspaceSortMode::Manual.next(), WorkspaceSortMode::Smart);
    }

    #[test]
    fn manual_sort_preserves_input_order() {
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("b", "/tmp/p1/b", "2026-03-01T00:00:00+00:00"),
            ws("a", "/tmp/p1/a", "2026-02-01T00:00:00+00:00"),
        ];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Manual, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["primary", "b", "a"]);
    }

    #[test]
    fn smart_sort_pins_primary_then_floats_by_attention() {
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("idle", "/tmp/p1/idle", "2026-02-01T00:00:00+00:00"),
            ws("needs", "/tmp/p1/needs", "2026-03-01T00:00:00+00:00"),
        ];
        // Tier 0 for "needs", tier 2 for everything else.
        let attention = |w: &Workspace| if w.id == "needs" { 0 } else { 2 };
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Smart, attention);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        // Primary stays first; "needs" floats above "idle".
        assert_eq!(ids, ["primary", "needs", "idle"]);
    }

    #[test]
    fn recent_sort_pins_primary_then_newest_first() {
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("old", "/tmp/p1/old", "2026-02-01T00:00:00+00:00"),
            ws("new", "/tmp/p1/new", "2026-05-01T00:00:00+00:00"),
        ];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Recent, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["primary", "new", "old"]);
    }

    #[test]
    fn smart_sort_without_primary_still_orders_rest() {
        // Defensive: if no row matches the root, all are treated as rest.
        let root = "/tmp/p1";
        let list = vec![
            ws("idle", "/tmp/p1/idle", "2026-02-01T00:00:00+00:00"),
            ws("needs", "/tmp/p1/needs", "2026-03-01T00:00:00+00:00"),
        ];
        let attention = |w: &Workspace| if w.id == "needs" { 0 } else { 2 };
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Smart, attention);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["needs", "idle"]);
    }
}
