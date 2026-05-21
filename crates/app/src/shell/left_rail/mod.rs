//! LeftRail — full workspace + nav rail.
//!
//! Composition (top → bottom):
//!
//! 1. Nav section: Tasks / Automations / Agents / Search rows (shells)
//! 2. WORKSPACES section header with filter / sort / + controls
//! 3. Workspace list — per-project groups rendering OxiMux `Workspace`
//!    rows with status dots derived from the latest agent session.
//! 4. Spacer
//! 5. Bottom toolbar: "Add Project" + settings cog
//!
//! Width is `density.w_left_rail` (250px in cockpit density). Full-collapse
//! toggling is handled at `WorkspaceRoot` via the `left_rail_open` flag.
//!
//! Data source: a `WeakEntity<WorkspaceRoot>` is the single channel to
//! reach `AppState` (recent projects + workspace_repo + agent_session_repo)
//! and `active_project`. Re-fetched on every render at v1 scale — fine
//! for <50 workspaces; switch to subscribe-and-cache if dogfood shows
//! frame-time issues.

pub mod nav_section;
pub mod project_group;
pub mod row_menu;
pub mod toolbar;
pub mod workspace_list_render;
pub mod workspace_row;

use gpui::{
    Context, IntoElement, ParentElement, Render, Styled, WeakEntity, Window, div, px, svg,
};
use oximux_core::Workspace;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::nav_section::{NavItem, render_nav_section};
use crate::shell::left_rail::project_group::{build_project_group_plan, render_project_group};
use crate::shell::left_rail::toolbar::render_toolbar;
use crate::workspace_root::WorkspaceRoot;

const HEADER_ICON_SIZE: f32 = 14.0;

pub struct LeftRail {
    active_nav: NavItem,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl LeftRail {
    /// Sources `theme`/`density`/`typography` from the `WorkspaceRoot` so
    /// the call site stays a one-liner. `weak_root.upgrade()` will be
    /// `None` at the precise moment `WorkspaceRoot::new` is wiring up
    /// (entity slot not yet populated), so the fallback defaults are the
    /// hot path during init — and they match `WorkspaceRoot::new`'s own
    /// defaults verbatim.
    pub fn new(weak_root: WeakEntity<WorkspaceRoot>, cx: &mut Context<Self>) -> Self {
        let (theme, density, typography) = weak_root
            .upgrade()
            .map(|r| {
                let root = r.read(cx);
                (root.theme, root.density, root.typography.clone())
            })
            .unwrap_or_else(|| (Theme::charcoal(), Density::cockpit(), Typography::cockpit()));
        Self {
            active_nav: NavItem::Tasks,
            weak_root,
            theme,
            density,
            typography,
        }
    }

    /// Test-only inspector for the currently-active nav item.
    #[doc(hidden)]
    pub fn active_nav(&self) -> NavItem {
        self.active_nav
    }

    pub fn select_nav(&mut self, item: NavItem, cx: &mut Context<Self>) {
        self.active_nav = item;
        cx.notify();
    }
}

impl Render for LeftRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entity = cx.entity().clone();

        let workspace_list =
            render_workspace_list(&self.weak_root, theme, density, &typography, cx);

        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(density.w_left_rail))
            .bg(theme.bg_panel)
            .border_r_1()
            .border_color(theme.border_inactive)
            .child(render_nav_section(
                self.active_nav,
                &entity,
                theme,
                density,
                &typography,
            ))
            .child(divider(theme))
            .child(workspace_header(theme, density, &typography))
            .child(div().flex_1().w_full().child(workspace_list))
            .child(render_toolbar(theme, density, &typography))
    }
}

fn render_workspace_list(
    weak_root: &WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<LeftRail>,
) -> gpui::AnyElement {
    let Some(root_entity) = weak_root.upgrade() else {
        return placeholder("No workspace root", theme, density, typography).into_any_element();
    };
    let root = root_entity.read(cx);
    let Some(active_project) = root.active_project.clone() else {
        return placeholder("Open a project (⌘O)", theme, density, typography).into_any_element();
    };

    // Re-fetch from DB so the rail reflects creates/renames/archives/deletes
    // emitted after boot. v1 scale: single SQLite call, sub-millisecond.
    let workspaces = match root
        .app_state
        .workspace_repo
        .list_for_project(&active_project.id)
    {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(?err, project_id = %active_project.id, "list_for_project failed");
            return placeholder("Failed to load workspaces", theme, density, typography)
                .into_any_element();
        }
    };

    // `list_for_project` already filters `archived_at IS NULL` at the SQL
    // layer, so no extra in-memory filter is needed.
    let plan = build_project_group_plan(&active_project, &workspaces, true);

    let agent_repo = root.app_state.agent_session_repo.clone();
    let latest_status_for =
        move |workspace_id: &str| match agent_repo.list_for_workspace(workspace_id) {
            Ok(mut sessions) => sessions.drain(..).next(),
            Err(err) => {
                tracing::warn!(?err, %workspace_id, "list_for_workspace failed");
                None
            }
        };

    let weak_root_for_menu = weak_root.clone();
    let on_row_menu = move |workspace: Workspace,
                            x: f32,
                            y: f32,
                            _window: &mut gpui::Window,
                            cx: &mut gpui::App| {
        let _ = weak_root_for_menu.update(cx, |root, cx| root.open_row_menu(workspace, x, y, cx));
    };

    let weak_for_render = weak_root.clone();
    render_project_group(
        plan,
        active_project,
        workspaces,
        latest_status_for,
        None,
        weak_for_render,
        on_row_menu,
        theme,
        density,
        typography,
    )
    .into_any_element()
}

fn placeholder(
    msg: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(60.))
        .px(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).bg(theme.border_inactive)
}

fn workspace_header(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row + 4.))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_label_caps))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child("WORKSPACES"),
        )
        .child(header_icon(theme, "icons/list-collapse.svg"))
}

fn header_icon(theme: Theme, path: &'static str) -> impl IntoElement {
    div().cursor_pointer().child(
        svg()
            .path(path)
            .size(px(HEADER_ICON_SIZE))
            .text_color(theme.fg_muted),
    )
}
