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
//! Data flow: data is pushed DOWN by `WorkspaceRoot::refresh_left_rail`
//! before each render. LeftRail itself never reads `WorkspaceRoot` —
//! that would re-enter the entity slot during rendering and panic
//! ("cannot read while it is already being updated"). The only thing
//! kept on `weak_root` is dispatch upward via callbacks (e.g.
//! `open_row_menu`), which fire on user events after render completes.

pub mod nav_section;
pub mod project_group;
pub mod row_menu;
pub mod toolbar;
pub mod workspace_list_render;
pub mod workspace_row;

use std::collections::HashMap;

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px, svg,
};
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::OpenProjectPicker;
use crate::shell::left_rail::nav_section::{NavItem, render_nav_section};
use crate::shell::left_rail::project_group::{build_project_group_plan, render_project_group};
use crate::shell::left_rail::toolbar::render_toolbar;
use crate::workspace_root::WorkspaceRoot;

const HEADER_ICON_SIZE: f32 = 14.0;

/// Snapshot of the latest agent-session status for a single workspace.
/// `None` means no sessions have ever been started for that workspace.
pub type LatestStatusMap = HashMap<String, Option<AgentStatus>>;

pub struct LeftRail {
    active_nav: NavItem,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Sidebar data snapshot. `WorkspaceRoot::refresh_left_rail` writes
    /// these before each render; `Render` reads them. Never reach out to
    /// `weak_root` from inside `Render` — re-entrant read of a being-
    /// updated entity panics.
    active_project: Option<Project>,
    workspaces: Vec<Workspace>,
    latest_status: LatestStatusMap,
}

impl LeftRail {
    /// Default-construct theme/density/typography. WorkspaceRoot uses the
    /// same constants in its own `new`, so the rail and root always agree.
    pub fn new(weak_root: WeakEntity<WorkspaceRoot>, _cx: &mut Context<Self>) -> Self {
        Self {
            active_nav: NavItem::Tasks,
            weak_root,
            theme: Theme::charcoal(),
            density: Density::cockpit(),
            typography: Typography::cockpit(),
            active_project: None,
            workspaces: Vec::new(),
            latest_status: HashMap::new(),
        }
    }

    /// Push the latest sidebar snapshot. Called by
    /// `WorkspaceRoot::refresh_left_rail` at the top of each render.
    pub(crate) fn set_sidebar_data(
        &mut self,
        active_project: Option<Project>,
        workspaces: Vec<Workspace>,
        latest_status: LatestStatusMap,
        cx: &mut Context<Self>,
    ) {
        self.active_project = active_project;
        self.workspaces = workspaces;
        self.latest_status = latest_status;
        cx.notify();
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

        let workspace_list = render_workspace_list(
            self.active_project.clone(),
            self.workspaces.clone(),
            self.latest_status.clone(),
            self.weak_root.clone(),
            theme,
            density,
            &typography,
        );

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

#[allow(clippy::too_many_arguments)]
fn render_workspace_list(
    active_project: Option<Project>,
    workspaces: Vec<Workspace>,
    latest_status: LatestStatusMap,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> gpui::AnyElement {
    let Some(active_project) = active_project else {
        return open_project_cta(theme, density, typography).into_any_element();
    };

    let plan = build_project_group_plan(&active_project, &workspaces, true);

    let latest_status_for =
        move |workspace_id: &str| latest_status.get(workspace_id).cloned().flatten();

    let weak_root_for_menu = weak_root.clone();
    let on_row_menu = move |workspace: Workspace,
                            x: f32,
                            y: f32,
                            _window: &mut gpui::Window,
                            cx: &mut gpui::App| {
        let _ = weak_root_for_menu.update(cx, |root, cx| root.open_row_menu(workspace, x, y, cx));
    };

    render_project_group(
        plan,
        active_project,
        workspaces,
        latest_status_for,
        None,
        weak_root,
        on_row_menu,
        theme,
        density,
        typography,
    )
    .into_any_element()
}

/// Empty-state row: clickable "Open a project (⌘O)" that dispatches the
/// project-picker action.
fn open_project_cta(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .id("left-rail-open-project-cta")
        .flex()
        .items_center()
        .justify_center()
        .h(px(60.))
        .px(px(density.pad_panel))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .hover(|s| s.text_color(theme.fg_base))
        .child("Open a project (⌘O)")
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(OpenProjectPicker), cx);
        })
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
