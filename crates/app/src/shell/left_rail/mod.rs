//! LeftRail — full workspace + nav rail (replaces the 30-line `sidebar.rs` stub).
//!
//! Composition (top → bottom):
//!
//! 1. Nav section: Tasks / Automations / Agents / Search rows (shells)
//! 2. WORKSPACES section header with filter / sort / + controls
//! 3. Workspace list (reuses `WorktreePanel` state; renders our own rows)
//! 4. Spacer
//! 5. Bottom toolbar: "Add Project" + settings cog
//!
//! Width is `density.w_left_rail` (250px in cockpit density). Full-collapse
//! toggling is handled at `WorkspaceRoot` via the `left_rail_open` flag.

pub mod nav_section;
pub mod toolbar;
pub mod workspace_list_render;

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px, svg,
};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::nav_section::{NavItem, render_nav_section};
use crate::shell::left_rail::toolbar::render_toolbar;
use crate::shell::left_rail::workspace_list_render::{
    build_workspace_row_plan, render_workspace_row,
};
use crate::shell::worktree_panel::{WorktreeListState, WorktreePanel};

const HEADER_ICON_SIZE: f32 = 14.0;

pub struct LeftRail {
    active_nav: NavItem,
    /// Owned for state ownership / async fetch lifetime. Its own Render impl
    /// is not invoked — we render rows from `state()` ourselves.
    worktree_panel: Option<Entity<WorktreePanel>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl LeftRail {
    pub fn new(
        repo: Option<Repository>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let worktree_panel = repo.map(|r| {
            let typography_clone = typography.clone();
            cx.new(|cx| WorktreePanel::new(r, theme, density, typography_clone, window, cx))
        });

        Self {
            active_nav: NavItem::Tasks,
            worktree_panel,
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

        let workspace_list = match self.worktree_panel.as_ref() {
            Some(panel) => render_workspace_list(panel, theme, density, &typography, cx),
            None => empty_state(theme, density, &typography),
        };

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
    panel: &Entity<WorktreePanel>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<LeftRail>,
) -> gpui::AnyElement {
    let state = panel.read(cx).state();
    match state {
        WorktreeListState::Ready(list) => {
            let mut col = div().flex().flex_col().w_full();
            for w in list.iter() {
                let plan = build_workspace_row_plan(w, false, theme);
                col = col.child(render_workspace_row(plan, theme, density, typography));
            }
            col.into_any_element()
        }
        WorktreeListState::Loading | WorktreeListState::Idle => {
            placeholder("Loading worktrees…", theme, density, typography).into_any_element()
        }
        WorktreeListState::Failed(err) => {
            placeholder(&format!("Failed: {err}"), theme, density, typography).into_any_element()
        }
    }
}

fn empty_state(theme: Theme, density: Density, typography: &Typography) -> gpui::AnyElement {
    placeholder("No repository open", theme, density, typography).into_any_element()
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
        // TODO(phase-04): filter, sort, and `+` add controls become interactive.
        .child(header_icon(theme, "icons/sort-descending.svg"))
        .child(header_icon(theme, "icons/plus.svg"))
}

fn header_icon(theme: Theme, path: &'static str) -> impl IntoElement {
    div().cursor_pointer().child(
        svg()
            .path(path)
            .size(px(HEADER_ICON_SIZE))
            .text_color(theme.fg_muted),
    )
}
