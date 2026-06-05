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
pub mod project_menu;
pub mod resize;
pub mod row_menu;
pub mod toolbar;
pub mod workspace_card;
pub mod workspace_list_render;
pub mod workspace_row;

use std::collections::{HashMap, HashSet};

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Render, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px, svg,
};
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::shell::left_rail::workspace_row::DiffCounts;

use crate::left_rail_layout;

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
    projects: Vec<Project>,
    active_project_id: Option<String>,
    /// Selected workspace id — drives the active-row highlight. Pushed
    /// down by `refresh_left_rail` alongside the rest of the snapshot.
    active_workspace_id: Option<String>,
    workspaces_by_project: HashMap<String, Vec<Workspace>>,
    latest_status: LatestStatusMap,
    /// Worktree paths that currently have an open agent tab. A workspace
    /// in this set reads as "live" (green idle dot) even before its
    /// session reports a concrete status.
    live_worktrees: HashSet<String>,
    /// Cached per-worktree diff counts (keyed by worktree path). Populated
    /// by `WorkspaceRoot`'s concurrent diff-fetch background tasks and pushed
    /// down here via `set_sidebar_data`. `None` for a worktree means the
    /// count is not yet available; the card omits the chip rather than blocking.
    diff_counts: HashMap<String, DiffCounts>,
    /// Live rail width. Driven by the right-edge resize handle; read by
    /// `WorkspaceRoot` for pane-area reflow (`left_chrome`).
    width: Pixels,
    /// Settings store for persisting `width` on each drag tick. `None`
    /// in unit tests that build the rail without a DB.
    settings_repo: Option<SettingsRepo>,
    /// Project ids whose group is collapsed (workspace rows hidden).
    /// Persisted to settings so the collapsed view survives restart.
    collapsed: HashSet<String>,
}

impl LeftRail {
    /// Default-construct theme/density/typography. WorkspaceRoot uses the
    /// same constants in its own `new`, so the rail and root always agree.
    pub fn new(weak_root: WeakEntity<WorkspaceRoot>, _cx: &mut Context<Self>) -> Self {
        let density = Density::cockpit();
        Self {
            active_nav: NavItem::Tasks,
            weak_root,
            theme: Theme::charcoal(),
            density,
            typography: Typography::cockpit(),
            projects: Vec::new(),
            active_project_id: None,
            active_workspace_id: None,
            workspaces_by_project: HashMap::new(),
            latest_status: HashMap::new(),
            live_worktrees: HashSet::new(),
            diff_counts: HashMap::new(),
            width: px(density.w_left_rail),
            settings_repo: None,
            collapsed: HashSet::new(),
        }
    }

    /// Install the settings store + load persisted layout (width +
    /// collapsed groups). Called once by `WorkspaceRoot` after
    /// construction (kept out of `new` so unit tests can build a
    /// repo-less rail).
    pub(crate) fn init_layout(&mut self, settings_repo: SettingsRepo) {
        self.width = px(left_rail_layout::load_left_rail_width(&settings_repo));
        self.collapsed = left_rail_layout::load_collapsed_projects(&settings_repo)
            .into_iter()
            .collect();
        self.settings_repo = Some(settings_repo);
    }

    /// Collapse all groups, or expand all if every group is already
    /// collapsed. Persists the new set and re-renders.
    pub(crate) fn toggle_collapse_all(&mut self, cx: &mut Context<Self>) {
        let all_collapsed = !self.projects.is_empty()
            && self.projects.iter().all(|p| self.collapsed.contains(&p.id));
        if all_collapsed {
            self.collapsed.clear();
        } else {
            self.collapsed = self.projects.iter().map(|p| p.id.clone()).collect();
        }
        self.persist_collapsed();
        cx.notify();
    }

    /// Persist the current collapsed set (no-op without a settings repo).
    fn persist_collapsed(&self) {
        if let Some(repo) = &self.settings_repo {
            let ids: Vec<String> = self.collapsed.iter().cloned().collect();
            left_rail_layout::save_collapsed_projects(repo, &ids);
        }
    }

    /// Toggle the collapsed state of a project group, persist the new
    /// set, and re-render.
    pub(crate) fn toggle_collapsed(&mut self, project_id: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&project_id) {
            self.collapsed.insert(project_id);
        }
        self.persist_collapsed();
        cx.notify();
    }

    /// Current rail width — read by `WorkspaceRoot` for pane reflow.
    pub(crate) fn width(&self) -> Pixels {
        self.width
    }

    /// Set the rail width from a drag tick: clamp into bounds, persist,
    /// and re-render. The persisted value lets the width survive restart.
    pub(crate) fn set_width(&mut self, candidate: Pixels, cx: &mut Context<Self>) {
        let clamped = left_rail_layout::clamp_left_rail_width(f32::from(candidate));
        self.width = px(clamped);
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_left_rail_width(repo, clamped);
        }
        cx.notify();
    }

    /// Push the latest sidebar snapshot. Called by
    /// `WorkspaceRoot::refresh_left_rail` at the top of each render.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_sidebar_data(
        &mut self,
        projects: Vec<Project>,
        active_project_id: Option<String>,
        active_workspace_id: Option<String>,
        workspaces_by_project: HashMap<String, Vec<Workspace>>,
        latest_status: LatestStatusMap,
        live_worktrees: HashSet<String>,
        diff_counts: HashMap<String, DiffCounts>,
        cx: &mut Context<Self>,
    ) {
        self.projects = projects;
        self.active_project_id = active_project_id;
        self.active_workspace_id = active_workspace_id;
        self.workspaces_by_project = workspaces_by_project;
        self.latest_status = latest_status;
        self.live_worktrees = live_worktrees;
        self.diff_counts = diff_counts;
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
            self.projects.clone(),
            self.active_project_id.clone(),
            self.active_workspace_id.clone(),
            self.collapsed.clone(),
            entity.clone(),
            self.workspaces_by_project.clone(),
            self.latest_status.clone(),
            self.live_worktrees.clone(),
            self.diff_counts.clone(),
            self.weak_root.clone(),
            theme,
            density,
            &typography,
        );

        // Body fills the column minus the right-edge resize handle.
        let body = div()
            .flex()
            .flex_col()
            .h_full()
            .flex_1()
            .min_w_0()
            .bg(theme.bg_panel)
            .child(render_nav_section(
                self.active_nav,
                &entity,
                theme,
                density,
                &typography,
            ))
            .child(divider(theme))
            .child(workspace_header(&entity, theme, density, &typography))
            .child(div().flex_1().w_full().child(workspace_list))
            .child(render_toolbar(theme, density, &typography));

        div()
            .flex()
            .flex_row()
            .h_full()
            .w(self.width)
            .border_r_1()
            .border_color(theme.border_inactive)
            .child(body)
            .child(resize::build_handle())
    }
}

#[allow(clippy::too_many_arguments)]
fn render_workspace_list(
    projects: Vec<Project>,
    active_project_id: Option<String>,
    active_workspace_id: Option<String>,
    collapsed: HashSet<String>,
    rail: gpui::Entity<LeftRail>,
    workspaces_by_project: HashMap<String, Vec<Workspace>>,
    latest_status: LatestStatusMap,
    live_worktrees: HashSet<String>,
    diff_counts: HashMap<String, DiffCounts>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> gpui::AnyElement {
    if projects.is_empty() {
        return open_project_cta(theme, density, typography).into_any_element();
    }

    let mut col = div().flex().flex_col().w_full();
    for project in projects {
        let workspaces = workspaces_by_project
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        let is_active = active_project_id.as_deref() == Some(project.id.as_str());
        let is_collapsed = collapsed.contains(&project.id);
        let plan = build_project_group_plan(&project, &workspaces, is_active, is_collapsed);

        let status_for_group = latest_status.clone();
        let latest_status_for =
            move |workspace_id: &str| status_for_group.get(workspace_id).cloned().flatten();

        let weak_root_for_menu = weak_root.clone();
        let on_row_menu = move |workspace: Workspace,
                                x: f32,
                                y: f32,
                                _window: &mut gpui::Window,
                                cx: &mut gpui::App| {
            let _ =
                weak_root_for_menu.update(cx, |root, cx| root.open_row_menu(workspace, x, y, cx));
        };

        let weak_root_for_project_menu = weak_root.clone();
        let on_project_menu = move |project: Project,
                                    x: f32,
                                    y: f32,
                                    _window: &mut gpui::Window,
                                    cx: &mut gpui::App| {
            let _ = weak_root_for_project_menu
                .update(cx, |root, cx| root.open_project_menu(project, x, y, cx));
        };

        col = col.child(render_project_group(
            plan,
            project,
            workspaces,
            latest_status_for,
            active_workspace_id.as_deref(),
            &live_worktrees,
            &diff_counts,
            rail.clone(),
            weak_root.clone(),
            on_row_menu,
            on_project_menu,
            theme,
            density,
            typography,
        ));
    }
    col.into_any_element()
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

fn workspace_header(
    rail: &gpui::Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row + 4.))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(
            // "Projects" header label — matches the reference UX's section title.
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child("Projects"),
        )
        .child(collapse_all_icon(rail.clone(), theme))
}

/// Collapse/expand-all toggle. Mirrors the reference UX's section-header control.
fn collapse_all_icon(rail: gpui::Entity<LeftRail>, theme: Theme) -> impl IntoElement {
    div()
        .id("workspaces-header-collapse")
        .cursor_pointer()
        .text_color(theme.fg_muted)
        .hover(|s| s.text_color(theme.fg_base))
        .tooltip(|window, cx| {
            gpui_component::tooltip::Tooltip::new("Collapse / expand all").build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _window, cx| {
            rail.update(cx, |r, cx| r.toggle_collapse_all(cx));
        })
        .child(
            svg()
                .path("icons/list-collapse.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
}
