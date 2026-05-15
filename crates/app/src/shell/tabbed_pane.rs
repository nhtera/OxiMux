//! TabbedPane — Phase 1 step 7. Each leaf of the workspace pane tree owns one
//! `TabbedPane`, which in turn holds N `TerminalView`s and an active index.
//!
//! Tab strip uses `gpui_component::TabBar` for visuals + click handling.
//! The strip is hidden when only one tab exists so the single-pane case looks
//! identical to step 5 (no chrome cost). Tab labels are `"Tab N"` placeholders;
//! once `TerminalEvent::TitleChange` wiring lands we'll swap in shell titles.
//!
//! `MainPane` observes the TabbedPane (not individual TerminalViews) so it
//! gets one consolidated notify per pane regardless of how many tabs each
//! pane holds. Internally the TabbedPane observes each TerminalView and
//! bubbles a `cx.notify()` up. This keeps focus-ring and dim-overlay repaints
//! O(panes) instead of O(tabs * panes).

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, px,
};
use gpui_component::{
    Sizable as _, Size,
    tab::{Tab, TabBar},
};

use crate::shell::terminal_view::TerminalView;

pub const TAB_STRIP_HEIGHT_PX: f32 = 28.0;

/// Next-tab cycling math. Returns `current` when `len < 2` so the entity's
/// `next_tab` cleanly no-ops on single-tab panes without a separate guard.
fn next_active_index(len: usize, current: usize) -> usize {
    if len < 2 {
        current
    } else {
        (current + 1) % len
    }
}

/// Mirror of [`next_active_index`] for backward cycling.
fn prev_active_index(len: usize, current: usize) -> usize {
    if len < 2 {
        current
    } else {
        (current + len - 1) % len
    }
}

/// After removing the tab at `closing`, what becomes the new active index?
/// `None` means the last tab was just closed — caller (MainPane) should
/// remove the whole pane. `Some(idx)` clamps to the left neighbor so closing
/// the rightmost tab shifts focus one slot left.
fn active_after_close(len_before: usize, closing: usize) -> Option<usize> {
    if len_before <= 1 {
        None
    } else {
        Some(closing.min(len_before - 2))
    }
}

struct TabEntry {
    label: SharedString,
    view: Entity<TerminalView>,
    _observer: Subscription,
}

pub struct TabbedPane {
    tabs: Vec<TabEntry>,
    active: usize,
    next_label_n: u64,
    focus_handle: FocusHandle,
}

impl TabbedPane {
    /// Build a TabbedPane that owns `initial_view` as its first tab.
    pub fn new(initial_view: Entity<TerminalView>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let observer = cx.observe(&initial_view, |_, _, cx| cx.notify());
        let tabs = vec![TabEntry {
            label: SharedString::from("Tab 1"),
            view: initial_view,
            _observer: observer,
        }];
        Self {
            tabs,
            active: 0,
            next_label_n: 2,
            focus_handle,
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Active tab's terminal focus handle. Used by MainPane to delegate focus
    /// and by `contains_focus` to map window focus back to a `PaneId`. Falls
    /// back to the pane's own handle in the transient empty-tabs state between
    /// `close_active` returning `true` and MainPane removing the entity, so a
    /// stray frame in that window doesn't index-panic.
    pub fn active_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.tabs.get(self.active) {
            Some(entry) => entry.view.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }

    /// Whether the given focus handle is one of: this pane's own handle, or
    /// any of its tabs' terminal handles. Lets MainPane.sync_focused_from_window
    /// re-map window focus back to a `PaneId` without poking inside the pane.
    pub fn contains_focus(&self, focused: &FocusHandle, cx: &App) -> bool {
        if &self.focus_handle == focused {
            return true;
        }
        self.tabs
            .iter()
            .any(|t| &t.view.read(cx).focus_handle(cx) == focused)
    }

    /// Push the spawned `TerminalView` as a new active tab. Called by MainPane
    /// after a successful PTY spawn.
    pub fn open_tab(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = SharedString::from(format!("Tab {}", self.next_label_n));
        self.next_label_n += 1;
        let observer = cx.observe(&view, |_, _, cx| cx.notify());
        self.tabs.push(TabEntry {
            label,
            view,
            _observer: observer,
        });
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new = next_active_index(self.tabs.len(), self.active);
        if new != self.active {
            self.active = new;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new = prev_active_index(self.tabs.len(), self.active);
        if new != self.active {
            self.active = new;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Close the active tab. Returns true iff the whole pane should be
    /// removed (last tab closed). Caller (MainPane) cascades into the
    /// existing pane-removal path in that case.
    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let next = active_after_close(self.tabs.len(), self.active);
        if self.tabs.is_empty() {
            return true;
        }
        self.tabs.remove(self.active);
        match next {
            None => true,
            Some(idx) => {
                self.active = idx;
                self.focus_active(window, cx);
                cx.notify();
                false
            }
        }
    }

    /// Stage the target grid on every tab. Per-tab background PTY output
    /// continues to flow even on inactive tabs, so resize must propagate to
    /// all of them (otherwise switching back to a stale tab shows a grid
    /// sized for a previous window state).
    pub fn set_target_grid(&self, cols: u16, rows: u16, cx: &mut Context<Self>) {
        for entry in &self.tabs {
            entry
                .view
                .update(cx, |view, _| view.set_target_grid(cols, rows));
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(entry) = self.tabs.get(self.active) {
            let handle = entry.view.read(cx).focus_handle(cx);
            handle.focus(window, cx);
        }
    }

    fn set_active(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() && ix != self.active {
            self.active = ix;
            self.focus_active(window, cx);
            cx.notify();
        }
    }
}

impl Focusable for TabbedPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TabbedPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Transient empty-tabs state between `close_active` returning `true`
        // and MainPane dropping the entity. Return a bare passthrough so any
        // stray frame in that window doesn't index-panic.
        let Some(active_entry) = self.tabs.get(self.active) else {
            return div().size_full();
        };
        let active_view = active_entry.view.clone();
        // Single-tab: passthrough. No strip, no flex wrapping — the
        // TerminalView (`h_full + w_full` in its own render) fills the leaf
        // directly, same as step 5 behavior before TabbedPane existed.
        // Wrapping it in an extra `flex_1 + min_h:0` div collapses the leaf
        // to ~0 height (gpui's flex doesn't expand a min_h:0 flex_1 child
        // when its sibling list is empty), so the strip-only path matters.
        if self.tabs.len() <= 1 {
            return div().size_full().child(active_view);
        }

        let bar_id = format!("oximux-tab-bar-{}", cx.entity_id());
        let mut bar = TabBar::new(SharedString::from(bar_id))
            .with_size(Size::Small)
            .selected_index(self.active)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.set_active(*ix, window, cx);
            }));
        for entry in &self.tabs {
            bar = bar.child(Tab::new().label(entry.label.clone()));
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(div().h(px(TAB_STRIP_HEIGHT_PX)).child(bar))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .min_w(px(0.0))
                    .child(active_view),
            )
    }
}

#[cfg(test)]
mod tests {
    //! Exercise the same pure helpers the entity methods call, so any future
    //! change to the cycle/close math is caught here instead of going green
    //! against a stale parallel implementation.

    use super::{active_after_close, next_active_index, prev_active_index};

    #[test]
    fn next_cycles_forward() {
        assert_eq!(next_active_index(3, 0), 1);
        assert_eq!(next_active_index(3, 1), 2);
        assert_eq!(next_active_index(3, 2), 0);
    }

    #[test]
    fn next_no_op_on_single_tab() {
        assert_eq!(next_active_index(1, 0), 0);
        assert_eq!(next_active_index(0, 0), 0);
    }

    #[test]
    fn prev_cycles_backward() {
        assert_eq!(prev_active_index(3, 0), 2);
        assert_eq!(prev_active_index(3, 2), 1);
        assert_eq!(prev_active_index(3, 1), 0);
    }

    #[test]
    fn close_signals_pane_removal_when_last() {
        assert_eq!(active_after_close(1, 0), None);
    }

    #[test]
    fn close_clamps_active_to_left_neighbor() {
        // Tabs [A, B, C], close C (idx 2): new active = idx 1 (B).
        assert_eq!(active_after_close(3, 2), Some(1));
        // Close middle B (idx 1): new active = idx 1 (was C, now in slot 1).
        assert_eq!(active_after_close(3, 1), Some(1));
        // Close leftmost A (idx 0): new active = idx 0.
        assert_eq!(active_after_close(3, 0), Some(0));
    }
}
