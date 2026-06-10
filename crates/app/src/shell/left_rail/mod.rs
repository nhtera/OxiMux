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
    AppContext, Context, Entity, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Render, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, WeakEntity, Window, div, px, svg,
};
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::shell::left_rail::workspace_row::DiffCounts;

use crate::left_rail_layout;

use crate::actions::OpenProjectPicker;
use crate::shell::agents_dashboard::model::attention_rank;
use crate::shell::agents_dashboard::render_agents_dashboard;
use crate::shell::left_rail::nav_section::{NavItem, render_nav_section};
use crate::shell::left_rail::project_group::{build_project_group_plan, render_project_group};
use crate::shell::left_rail::toolbar::render_toolbar;
use crate::shell::left_rail::workspace_list_render::{WorkspaceSortMode, sort_workspaces};
use crate::shell::tasks_view::TasksView;
use crate::workspace_root::WorkspaceRoot;

const HEADER_ICON_SIZE: f32 = 14.0;

/// Snapshot of the latest agent-session status for a single workspace.
/// `None` means no sessions have ever been started for that workspace.
pub type LatestStatusMap = HashMap<String, Option<AgentStatus>>;

pub struct LeftRail {
    /// Which nav page is open, or `None` for the home view (workspace list,
    /// no nav row highlighted). Clicking the active nav toggles back to home.
    active_nav: Option<NavItem>,
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
    /// How workspace rows are ordered within each project group. Persisted
    /// so the choice survives restart.
    sort_mode: WorkspaceSortMode,
    /// Scroll position for the agents dashboard `uniform_list`. Stored on
    /// `LeftRail` so it survives re-renders while the Agents nav is active.
    agents_scroll: UniformListScrollHandle,
    /// Tasks page entity (GitHub issue/PR browser), mounted when the Tasks nav
    /// is active. Owns its own async fetch + filter state.
    tasks_view: Entity<TasksView>,
}

impl LeftRail {
    /// Default-construct theme/density/typography. WorkspaceRoot uses the
    /// same constants in its own `new`, so the rail and root always agree.
    pub fn new(weak_root: WeakEntity<WorkspaceRoot>, cx: &mut Context<Self>) -> Self {
        let density = Density::cockpit();
        let theme = Theme::charcoal();
        let typography = Typography::cockpit();
        let tasks_view = cx.new(|cx| {
            TasksView::new(weak_root.clone(), theme, density, typography.clone(), cx)
        });
        Self {
            active_nav: None,
            weak_root,
            theme,
            density,
            typography,
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
            sort_mode: WorkspaceSortMode::default(),
            agents_scroll: UniformListScrollHandle::new(),
            tasks_view,
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
        self.sort_mode = left_rail_layout::load_sort_mode(&settings_repo);
        self.settings_repo = Some(settings_repo);
    }

    /// Current workspace sort mode. Test-only inspector.
    #[doc(hidden)]
    pub fn sort_mode(&self) -> WorkspaceSortMode {
        self.sort_mode
    }

    /// Advance to the next sort mode (Smart → Recent → Manual → Smart),
    /// persist it, and re-render.
    pub(crate) fn cycle_sort_mode(&mut self, cx: &mut Context<Self>) {
        self.sort_mode = self.sort_mode.next();
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_sort_mode(repo, self.sort_mode);
        }
        cx.notify();
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
        let project_changed = self.active_project_id != active_project_id;
        self.projects = projects;
        self.active_project_id = active_project_id;
        self.active_workspace_id = active_workspace_id;
        self.workspaces_by_project = workspaces_by_project;
        self.latest_status = latest_status;
        self.live_worktrees = live_worktrees;
        self.diff_counts = diff_counts;

        // Keep the Tasks page's project in sync; re-fetch only when the active
        // project actually changes while the page is open.
        let active = self.active_project();
        let tasks_open = self.active_nav == Some(NavItem::Tasks);
        self.tasks_view.update(cx, |tv, cx| {
            tv.set_project(active, cx);
            if project_changed && tasks_open {
                tv.refresh(cx);
            }
        });
        cx.notify();
    }

    /// Test-only inspector for the currently-active nav item (`None` = home).
    #[doc(hidden)]
    pub fn active_nav(&self) -> Option<NavItem> {
        self.active_nav
    }

    /// Resolve the active `Project` from the current snapshot, if any.
    fn active_project(&self) -> Option<oximux_core::Project> {
        let id = self.active_project_id.as_ref()?;
        self.projects.iter().find(|p| &p.id == id).cloned()
    }

    /// Resolve the active workspace's tint swatch from the current snapshot.
    /// `WorkspaceRoot` reads this each render to accent the active tab strip,
    /// so the tint stays in sync without a separately-cached copy.
    pub(crate) fn active_workspace_tint(
        &self,
    ) -> Option<crate::shell::pane_group::TabColor> {
        let ws_id = self.active_workspace_id.as_ref()?;
        let proj_id = self.active_project_id.as_ref()?;
        self.workspaces_by_project
            .get(proj_id)?
            .iter()
            .find(|w| &w.id == ws_id)?
            .tint
            .as_deref()
            .and_then(crate::shell::pane_group::TabColor::from_slug)
    }

    /// Return to the home view (workspace list, no nav highlighted). Used after
    /// creating a workspace from the Tasks page so the new row is visible.
    pub(crate) fn go_home(&mut self, cx: &mut Context<Self>) {
        if self.active_nav.is_some() {
            self.active_nav = None;
            cx.notify();
        }
    }

    /// Toggle a nav page. Clicking the active page returns to the home view
    /// (workspace list). Opening the Tasks page seeds it with the active
    /// project and triggers a fetch (cheap if already loaded).
    pub fn select_nav(&mut self, item: NavItem, cx: &mut Context<Self>) {
        self.active_nav = if self.active_nav == Some(item) {
            None
        } else {
            Some(item)
        };
        if self.active_nav == Some(NavItem::Tasks) {
            let active = self.active_project();
            self.tasks_view.update(cx, |tv, cx| {
                tv.set_project(active, cx);
                tv.activate(cx);
            });
        }
        cx.notify();
    }
}

impl Render for LeftRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entity = cx.entity().clone();

        // The flex-1 body slot changes depending on the active nav page.
        // Agents → agents dashboard; Tasks → issue/PR browser; home (None) and
        // the not-yet-built shells → the workspace list.
        let content_body: gpui::AnyElement = if self.active_nav == Some(NavItem::Agents) {
            render_agents_dashboard(
                &self.projects,
                &self.workspaces_by_project,
                &self.latest_status,
                &self.live_worktrees,
                &self.diff_counts,
                self.weak_root.clone(),
                &self.agents_scroll,
                theme,
                density,
                &typography,
            )
        } else if self.active_nav == Some(NavItem::Tasks) {
            div()
                .flex()
                .flex_col()
                .h_full()
                .w_full()
                .child(self.tasks_view.clone())
                .into_any_element()
        } else {
            let workspace_list = render_workspace_list(
                self.projects.clone(),
                self.active_project_id.clone(),
                self.active_workspace_id.clone(),
                self.collapsed.clone(),
                self.sort_mode,
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
            div()
                .flex()
                .flex_col()
                .h_full()
                .w_full()
                .child(workspace_header(
                    &entity,
                    self.sort_mode,
                    theme,
                    density,
                    &typography,
                ))
                .child(div().flex_1().w_full().child(workspace_list))
                .into_any_element()
        };

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
            .child(div().flex_1().w_full().child(content_body))
            .child(render_toolbar(theme, density, &typography));

        let weak_root_for_drop = self.weak_root.clone();
        div()
            .id("left-rail-root")
            .flex()
            .flex_row()
            .h_full()
            .w(self.width)
            .border_r_1()
            .border_color(theme.border_inactive)
            // OS-native folder drop target: tint while a directory drag
            // hovers the rail, register + activate the folder(s) on drop.
            // File (non-directory) drops are ignored.
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                style.bg(Hsla {
                    a: 0.4,
                    ..theme.selection
                })
            })
            .on_drop::<gpui::ExternalPaths>(move |payload, window, cx| {
                let dirs: Vec<std::path::PathBuf> = payload
                    .paths()
                    .iter()
                    .filter(|p| p.is_dir())
                    .cloned()
                    .collect();
                if dirs.is_empty() {
                    return;
                }
                let _ = weak_root_for_drop.update(cx, |root, cx| {
                    for dir in dirs {
                        root.add_project_from_drop(dir, window, cx);
                    }
                });
            })
            .child(body)
            .child(resize::build_handle(theme))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_workspace_list(
    projects: Vec<Project>,
    active_project_id: Option<String>,
    active_workspace_id: Option<String>,
    collapsed: HashSet<String>,
    sort_mode: WorkspaceSortMode,
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
        // Order rows per the active sort mode. The attention tier reuses the
        // same ranking the agents dashboard uses, so both surfaces agree on
        // what "needs attention" means.
        let workspaces = sort_workspaces(&workspaces, &project.root_path, sort_mode, |ws| {
            let status = latest_status.get(&ws.id).cloned().flatten();
            attention_rank(status.as_ref(), live_worktrees.contains(&ws.worktree_path))
        });
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
    sort_mode: WorkspaceSortMode,
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
            // Section title.
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child("Projects"),
        )
        .child(sort_mode_chip(rail.clone(), sort_mode, theme, density, typography))
        .child(collapse_all_icon(rail.clone(), theme))
}

/// Sort-order toggle. A compact text chip showing the active mode; clicking
/// cycles Smart → Recent → Manual and persists the choice.
fn sort_mode_chip(
    rail: gpui::Entity<LeftRail>,
    sort_mode: WorkspaceSortMode,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .id("workspaces-header-sort")
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_xs))
        .cursor_pointer()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .hover(|s| s.bg(theme.hover_overlay).text_color(theme.fg_base))
        .tooltip(|window, cx| {
            gpui_component::tooltip::Tooltip::new("Sort: Smart → Recent → Manual")
                .build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _window, cx| {
            rail.update(cx, |r, cx| r.cycle_sort_mode(cx));
        })
        .child(sort_mode.label())
}

/// Collapse/expand-all toggle: one click collapses every project group, or
/// expands all when they are already collapsed.
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
