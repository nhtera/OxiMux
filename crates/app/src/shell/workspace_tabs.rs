//! WorkspaceTabs — flat workspace-level tab strip.
//!
//! Each entry owns an `Entity<MainPane>` (its own split tree of terminals).
//! The strip is rendered into the top_bar's center zone via
//! [`render_tab_strip`]; the active MainPane fills the main row below.
//!
//! Action handlers for `NewTab`/`CloseTab`/`NextTab`/`PrevTab` live here so
//! the workspace tab strip catches the keystrokes (Cmd-T / Cmd-W / Cmd-} /
//! Cmd-{) that bubble up from the focused TerminalView through the active
//! MainPane.

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px, svg,
};

use crate::actions::{SplitDown, SplitLeft, SplitRight, SplitUp};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{CloseTab, NewTab, NextTab, PrevTab};
use crate::shell::main_pane::MainPane;
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView};

struct WorkspaceTab {
    label: SharedString,
    pane: Entity<MainPane>,
    _observer: Subscription,
}

pub struct WorkspaceTabs {
    tabs: Vec<WorkspaceTab>,
    active: usize,
    next_label_n: u64,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
}

impl WorkspaceTabs {
    pub fn new(
        initial_pane: Entity<MainPane>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let observer = cx.observe(&initial_pane, |_, _, cx| cx.notify());
        let label = SharedString::from("Terminal 1");
        let tabs = vec![WorkspaceTab {
            label,
            pane: initial_pane,
            _observer: observer,
        }];
        Self {
            tabs,
            active: 0,
            next_label_n: 2,
            theme,
            density,
            typography,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn active_pane(&self) -> Option<Entity<MainPane>> {
        self.tabs.get(self.active).map(|t| t.pane.clone())
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Forward the chrome width to every tab's MainPane. Inactive tabs still
    /// hold a PTY whose grid must reflect the current visible area, otherwise
    /// switching back would briefly paint with a stale size before the next
    /// resize tick.
    pub fn set_chrome_width(&self, chrome_w: f32, cx: &mut App) {
        for tab in &self.tabs {
            tab.pane
                .update(cx, |pane, cx| pane.set_chrome_width(chrome_w, cx));
        }
    }

    pub fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_pane) = spawn_main_pane(
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        let n = self.next_label_n;
        self.next_label_n += 1;
        let observer = cx.observe(&new_pane, |_, _, cx| cx.notify());
        self.tabs.push(WorkspaceTab {
            label: SharedString::from(format!("Terminal {n}")),
            pane: new_pane,
            _observer: observer,
        });
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            // Closing the last tab drops the workspace into the empty welcome
            // state. `active` is irrelevant when there are no tabs; reset to
            // 0 so the next open_tab lands on a sane index.
            // Move focus to the WorkspaceTabs root so subsequent action
            // dispatches (e.g. ToggleRightSidebar from a top-bar button)
            // still propagate up to WorkspaceRoot's on_action handlers —
            // without a focused descendant, button-fired `dispatch_action`
            // calls have no element chain to bubble through.
            self.active = 0;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active + 1) % self.tabs.len();
        self.set_active(next, window, cx);
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if len < 2 {
            return;
        }
        let prev = (self.active + len - 1) % len;
        self.set_active(prev, window, cx);
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let handle = tab.pane.read(cx).active_focus_handle(cx);
        handle.focus(window, cx);
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tab(window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        self.close_tab(ix, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }
}

/// Build the tab-strip element for embedding in `top_bar`'s center zone.
///
/// Flat strip: each tab fills the chrome row edge-to-edge
/// separated by 1px right borders. The active tab is identified ONLY by a
/// 2px top accent line (focus_ring color) — no background recolor. Inactive
/// tabs render with muted text; hovering brightens them. Close affordance
/// stays hidden on inactive tabs (revealed via `group_hover`) and is
/// permanently visible (muted) on the active tab.
///
/// `entity` is captured by per-tab click handlers, the per-tab × button,
/// and trailing controls so they can mutate state from element closures.
///
/// Plus-button placement: sits inline immediately after the last tab so the
/// affordance reads as "add another tab here", not as a disconnected chrome
/// control. A `flex_1` spacer between the plus button and pane-actions keeps
/// "..." pinned at the right edge. When tabs grow past the available width,
/// the strip's `flex_shrink: 1` + `min_w: 0` causes it to shrink (with
/// horizontal scroll); the spacer collapses to zero so plus stays anchored
/// at the right edge of the (shrunken) strip.
pub fn render_tab_strip(entity: Entity<WorkspaceTabs>, cx: &mut App) -> AnyElement {
    let this = entity.read(cx);
    let theme = this.theme;
    let active = this.active;
    let tab_count = this.tabs.len();

    let mut strip = div()
        .id(SharedString::from(format!(
            "oximux-workspace-tab-strip-{}",
            entity.entity_id()
        )))
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .min_w(px(0.0))
        .overflow_x_scroll()
        .overflow_y_hidden();

    for (ix, tab) in this.tabs.iter().enumerate() {
        strip = strip.child(workspace_tab(
            ix,
            tab.label.clone(),
            ix == active,
            ix < tab_count - 1,
            theme,
            entity.clone(),
        ));
    }

    let mut row = div()
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .min_w(px(0.0))
        .flex_1()
        .child(strip)
        .child(plus_button(theme, entity.clone()))
        .child(div().flex_1().min_w(px(0.0)).h_full());
    // Hide the pane-actions ("...") button when no tabs are open — there is
    // no MainPane to split. The "+" stays so users can open a new terminal.
    if tab_count > 0 {
        row = row.child(pane_actions_button(theme));
    }
    row.into_any_element()
}

/// One flat tab: full chrome-row height, top accent on active,
/// right-border separator from its neighbor.
fn workspace_tab(
    ix: usize,
    label: SharedString,
    is_active: bool,
    has_neighbor_right: bool,
    theme: Theme,
    entity: Entity<WorkspaceTabs>,
) -> impl IntoElement {
    // Unique group per tab so the close X's hover-reveal only triggers on
    // ITS OWN tab's hover (Tailwind group/group-hover idiom). Without the
    // per-ix name, hovering any tab would reveal every close button.
    let group_name = SharedString::from(format!("ws-tab-{ix}"));
    let icon = svg()
        .path("icons/square-terminal.svg")
        .size(px(11.0))
        .text_color(if is_active {
            theme.fg_muted
        } else {
            theme.fg_subtle
        });
    let text_color = if is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    // Always reserve 2px on top so the active accent line doesn't shift
    // tab content vertically when selection changes. Inactive tabs paint
    // that border transparent; active uses the focus-ring color.
    let top_accent = if is_active {
        theme.focus_ring
    } else {
        gpui::transparent_black()
    };
    let separator = if has_neighbor_right {
        theme.border_inactive
    } else {
        gpui::transparent_black()
    };
    let close_btn = close_button(theme, ix, is_active, entity.clone(), group_name.clone());
    let entity_for_click = entity.clone();

    // The top-accent line is the primary visual signal for "active". Right-
    // edge separator is rendered as an absolutely-positioned 1px child so
    // gpui's single `border_color` doesn't have to serve both the top
    // accent (focus_ring or transparent) and the side separator
    // (border_inactive or transparent).
    div()
        .id(SharedString::from(format!("ws-tab-{ix}")))
        .group(group_name)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h_full()
        .px(px(8.0))
        .border_t_2()
        .border_color(top_accent)
        .text_size(px(11.0))
        .text_color(text_color)
        .flex_shrink_0()
        .cursor_pointer()
        .when(!is_active, |s| {
            s.hover(|s| s.text_color(theme.fg_base).bg(theme.bg_panel_alt))
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity_for_click.clone();
            entity.update(cx, |this, cx| this.set_active(ix, window, cx));
        })
        .child(icon)
        .child(
            div()
                .min_w(px(0.0))
                .max_w(px(110.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label),
        )
        .child(close_btn)
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(1.0))
                .bg(separator),
        )
}

/// "..." button that opens the Pane Actions menu (split
/// directions). Dispatches `SplitRight`/`SplitDown`/`SplitLeft`/`SplitUp`
/// from the menu items; this button itself just opens the menu.
fn pane_actions_button(theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/ellipsis.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id("ws-tab-pane-actions")
        .w(px(28.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(crate::actions::OpenPaneActions), cx);
        })
        .child(glyph)
}

/// Static helper exposing per-direction split icons so the dropdown menu
/// (and any future surface) can reuse the same glyphs.
pub fn split_icon(action: SplitDirection) -> &'static str {
    match action {
        SplitDirection::Right => "icons/arrow-right.svg",
        SplitDirection::Down => "icons/arrow-down.svg",
        SplitDirection::Left => "icons/arrow-left.svg",
        SplitDirection::Up => "icons/arrow-up.svg",
    }
}

/// Enum mirror of the four split actions for menu rendering. Kept here
/// (not actions.rs) because it's UI-presentation metadata, not a dispatched
/// action — each variant maps to a real `Split*` action when activated.
#[derive(Clone, Copy)]
pub enum SplitDirection {
    Right,
    Down,
    Left,
    Up,
}

impl SplitDirection {
    pub fn label(self) -> &'static str {
        match self {
            SplitDirection::Right => "Split Right",
            SplitDirection::Down => "Split Down",
            SplitDirection::Left => "Split Left",
            SplitDirection::Up => "Split Up",
        }
    }

    /// Dispatch the corresponding workspace action up the focus chain so the
    /// focused MainPane intercepts it.
    pub fn dispatch(self, window: &mut Window, cx: &mut App) {
        match self {
            SplitDirection::Right => window.dispatch_action(Box::new(SplitRight), cx),
            SplitDirection::Down => window.dispatch_action(Box::new(SplitDown), cx),
            SplitDirection::Left => window.dispatch_action(Box::new(SplitLeft), cx),
            SplitDirection::Up => window.dispatch_action(Box::new(SplitUp), cx),
        }
    }
}

fn close_button(
    theme: Theme,
    ix: usize,
    is_active: bool,
    entity: Entity<WorkspaceTabs>,
    group_name: SharedString,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    // Active tab: close X always visible (muted).
    // Inactive tab: hidden until the parent tab (same `group_name`) is
    // hovered, then `group_hover` flips opacity to 1.0.
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!("ws-tab-close-{ix}")))
        .w(px(14.0))
        .h(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .opacity(initial_opacity)
        .group_hover(group_name, |s| s.opacity(1.0))
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity.clone();
            entity.update(cx, |this, cx| this.close_tab(ix, window, cx));
            cx.stop_propagation();
        })
        .child(glyph)
}

fn plus_button(theme: Theme, entity: Entity<WorkspaceTabs>) -> impl IntoElement {
    let glyph = svg()
        .path("icons/plus.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id("ws-tab-plus")
        .w(px(28.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity.clone();
            entity.update(cx, |this, cx| this.open_tab(window, cx));
            cx.stop_propagation();
        })
        .child(glyph)
}

fn spawn_main_pane(
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceTabs>,
) -> Option<Entity<MainPane>> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        ..SpawnConfig::default()
    };
    let session_id = match backend.spawn(cfg) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pty spawn for new workspace tab failed");
            return None;
        }
    };
    let typography_for_view = typography.clone();
    let initial_view = cx.new(|cx| {
        TerminalView::mount(
            backend,
            session_id,
            theme,
            density,
            typography_for_view,
            window,
            cx,
        )
    });
    Some(cx.new(|cx| MainPane::new(initial_view, theme, density, typography, cx)))
}

impl Focusable for WorkspaceTabs {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceTabs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Render the active MainPane, or the welcome card when no
        // tabs are open (e.g. after closing the last one). The tab strip is
        // built separately by WorkspaceRoot via [`render_tab_strip`] and
        // slotted into top_bar. Action handlers live on the root div so
        // NewTab/CloseTab/NextTab/PrevTab catch keystrokes bubbling from the
        // focused TerminalView (which is inside the active MainPane).
        let focus_handle = self.focus_handle.clone();
        let mut root = div()
            .id("oximux-workspace-tabs")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab));
        if let Some(pane) = self.active_pane() {
            root = root.child(pane);
        } else {
            root = root.child(crate::shell::welcome_view::view(
                self.theme,
                self.density,
                &self.typography,
            ));
        }
        root
    }
}
