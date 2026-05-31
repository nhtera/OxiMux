//! Pure render helpers + GPUI row painter for one persisted Workspace.
//!
//! Two-layer split:
//! 1. `build_workspace_row_plan` + `status_dot_color` are pure functions —
//!    no GPUI runtime, no IO. Unit-tested without a window.
//! 2. `render_workspace_row` consumes a `WorkspaceRowPlan` and paints it.
//!
//! The status dot is a solid colored circle whose hue is derived from the
//! workspace's latest agent session. Empty session list → muted dot.

use gpui::{
    Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, div, px, svg,
};
use oximux_core::{AgentStatus, Workspace};
use oximux_settings::{Density, Theme, Typography};

/// Side length of the colored status circle.
const STATUS_DOT_SIZE: f32 = 8.0;
/// Folder icon size (matches the workspace header icon).
const FOLDER_ICON_SIZE: f32 = 14.0;
/// Row vertical multiplier — slightly taller than nav rows so the
/// two-line slug subtext fits without clipping.
const ROW_HEIGHT_MULT: f32 = 1.6;
/// Ellipsis trailing-button width.
const TRAILING_BTN_SIZE: f32 = 18.0;

/// Visual tokens for one Workspace row. Pure, testable.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRowPlan {
    pub name: String,
    pub slug: String,
    pub dot_color: Hsla,
    pub bg: Hsla,
    pub fg: Hsla,
    pub fg_sub: Hsla,
}

/// Resolve the status-dot color for a workspace given its latest agent
/// session status (or `None` when no sessions have ever started).
///
/// Semantics:
/// - No session or `Idle`  → `fg_subtle` (quiet grey, blends with row text)
/// - `Running`             → `status_info` (active work in flight)
/// - `WaitingForInput` or `NeedsApproval` → `status_warn` (needs attention)
/// - `Done { code: 0 }`    → `status_ok` (clean exit)
/// - `Done { code != 0 }` or `Failed` → `status_error`
/// - `Interrupted`         → `status_muted` (last shutdown clipped it)
pub fn status_dot_color(status: Option<&AgentStatus>, theme: Theme) -> Hsla {
    match status {
        None | Some(AgentStatus::Idle) => theme.fg_subtle,
        Some(AgentStatus::Running) => theme.status_info,
        Some(AgentStatus::WaitingForInput) | Some(AgentStatus::NeedsApproval(_)) => {
            theme.status_warn
        }
        Some(AgentStatus::Done { code: Some(0) }) => theme.status_ok,
        Some(AgentStatus::Done { .. }) => theme.status_error,
        Some(AgentStatus::Failed(_)) => theme.status_error,
        Some(AgentStatus::Interrupted) => theme.status_muted,
    }
}

/// Compute the visual plan for one Workspace row.
pub fn build_workspace_row_plan(
    workspace: &Workspace,
    is_active: bool,
    latest_status: Option<&AgentStatus>,
    theme: Theme,
) -> WorkspaceRowPlan {
    let dot_color = status_dot_color(latest_status, theme);
    let (bg, fg) = if is_active {
        (theme.bg_panel_alt, theme.fg_base)
    } else {
        (theme.bg_panel, theme.fg_base)
    };
    WorkspaceRowPlan {
        name: workspace.name.clone(),
        slug: workspace.slug.clone(),
        dot_color,
        bg,
        fg,
        fg_sub: theme.fg_subtle,
    }
}

/// Render one Workspace row. The trailing "…" button is `.invisible()` by
/// default and revealed on row hover via `group_hover`. `group_name` lets
/// each row participate in its own hover scope.
#[allow(clippy::too_many_arguments)]
pub fn render_workspace_row(
    plan: WorkspaceRowPlan,
    row_id: SharedString,
    group_name: SharedString,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_row_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    on_menu_click: impl Fn(&MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let menu_id: SharedString = format!("{row_id}-menu").into();
    let trailing_btn = div()
        .id(menu_id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(TRAILING_BTN_SIZE))
        .rounded(px(density.r_xs))
        .text_color(theme.fg_muted)
        .invisible()
        .group_hover(group_name.clone(), |s| s.visible())
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
        .child(
            svg()
                .path("icons/ellipsis.svg")
                .size(px(FOLDER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
            cx.stop_propagation();
            on_menu_click(ev, window, cx);
        });

    div()
        .id(row_id)
        .group(group_name)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row * ROW_HEIGHT_MULT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .bg(plan.bg)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .child(
            // Status dot — solid colored circle.
            div()
                .size(px(STATUS_DOT_SIZE))
                .rounded_full()
                .bg(plan.dot_color)
                .flex_shrink_0(),
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
                .child(
                    div()
                        .text_size(px(typography.t_sub_label))
                        .text_color(plan.fg_sub)
                        .child(plan.slug),
                ),
        )
        .child(trailing_btn)
        .on_mouse_down(MouseButton::Left, on_row_click)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str, slug: &str) -> Workspace {
        Workspace {
            id: format!("id-{slug}"),
            project_id: "proj".to_string(),
            name: name.to_string(),
            slug: slug.to_string(),
            branch: format!("oximux/{slug}"),
            worktree_path: format!("/tmp/{slug}"),
            status: "active".to_string(),
            created_at: "2026-05-21T00:00:00Z".to_string(),
            archived_at: None,
        }
    }

    #[test]
    fn dot_color_none_uses_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(status_dot_color(None, t), t.fg_subtle);
    }

    #[test]
    fn dot_color_idle_uses_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(status_dot_color(Some(&AgentStatus::Idle), t), t.fg_subtle);
    }

    #[test]
    fn dot_color_running_uses_status_info() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Running), t),
            t.status_info
        );
    }

    #[test]
    fn dot_color_waiting_uses_status_warn() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::WaitingForInput), t),
            t.status_warn
        );
    }

    #[test]
    fn dot_color_needs_approval_uses_status_warn() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::NeedsApproval("permission".into())), t),
            t.status_warn
        );
    }

    #[test]
    fn dot_color_done_clean_uses_status_ok() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: Some(0) }), t),
            t.status_ok
        );
    }

    #[test]
    fn dot_color_done_with_nonzero_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: Some(1) }), t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_done_with_unknown_code_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: None }), t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_failed_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Failed("boom".into())), t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_interrupted_uses_status_muted() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Interrupted), t),
            t.status_muted
        );
    }

    #[test]
    fn row_plan_active_uses_panel_alt_bg() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, true, None, t);
        assert_eq!(plan.bg, t.bg_panel_alt);
        assert_eq!(plan.fg, t.fg_base);
    }

    #[test]
    fn row_plan_inactive_uses_panel_bg() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, false, None, t);
        assert_eq!(plan.bg, t.bg_panel);
    }

    #[test]
    fn row_plan_carries_name_and_slug() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, false, None, t);
        assert_eq!(plan.name, "Fix Login");
        assert_eq!(plan.slug, "fix-login");
    }

    #[test]
    fn row_plan_dot_color_reflects_latest_status() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_row_plan(&w, false, Some(&AgentStatus::Running), t);
        assert_eq!(plan.dot_color, t.status_info);
    }
}
