//! The stash section's drag handle and keyboard rail.
//!
//! # Why this is a copy of the graph's, and not a shared primitive
//!
//! The graph's resize has shipped and works. Extracting a common primitive
//! and retrofitting it would put a regression in a working feature on the
//! critical path of a feature request, and the two are less alike than they
//! look: the graph's rail binds against `graph_height` through its own focus
//! handle, and the workspace root selects a drag by *payload type*, so a
//! shared primitive would still need one payload per section. Roughly sixty
//! deliberately duplicated lines is the cheaper trade. If a third resizable
//! section ever appears, extract then — with three call sites the shape is
//! actually known.
//!
//! # What is NOT duplicated: the budget
//!
//! Both sections are `flex_shrink_0` in a column whose only flexible child is
//! the changed-files list, so they spend from one pot. Each section's ceiling
//! is therefore computed from its sibling's live height and pushed down by
//! `SourceControlPanel` (`source_control/sections.rs`), which is the only
//! place that can see both. This module just honours whatever ceiling it was
//! handed.

use crate::scm_layout_settings;
use crate::shell::stash_panel::StashPanel;
use gpui::{
    AnyElement, AppContext as _, Context, ElementId, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled, Window,
    div, prelude::FluentBuilder as _, px,
};

/// Total mouse hit-height of the top drag handle. Matches the graph's so the
/// two boundaries in one column feel the same under the cursor.
const STASH_RESIZE_HIT_PX: f32 = 9.0;
/// Height of the hover/drag highlight bar painted over the hairline.
const STASH_RESIZE_BAR_PX: f32 = 3.0;

/// Drag payload tagging a stash-height resize. Empty — the new height comes
/// from the cursor against an anchor latched at drag start. It exists only so
/// the workspace root's `on_drag_move` can type-select these ticks apart from
/// the graph's and the sidebar's.
#[derive(Debug, Clone)]
pub struct StashResizePayload;

/// Zero-size drag preview. GPUI's `on_drag` must return something to render
/// at the cursor; an edge handle has no preview — the section reflows live.
pub struct StashResizeGhost;

impl Render for StashResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

impl StashPanel {
    /// The height the body is **painted** at: the user's chosen height,
    /// trimmed to whatever the shared budget currently allows.
    ///
    /// Trimming here rather than in `stash_height` is deliberate. The stored
    /// value is what the user asked for and it survives a short window, a
    /// scope switch, or the graph being dragged tall — all of which are
    /// temporary. Writing the trim back into state instead would quietly
    /// forget the request, and the section would not come back when the room
    /// did.
    pub fn painted_height(&self) -> Pixels {
        match self.section_ceiling {
            Some(ceiling) => px(f32::from(self.stash_height)
                .min(ceiling)
                .max(scm_layout_settings::MIN_STASH_HEIGHT)),
            None => self.stash_height,
        }
    }

    /// The ceiling this section may grow to, pushed down by the parent panel
    /// once per render. `None` until the first push — the section then uses
    /// its window clamp alone, which is what a lone section gets anyway.
    pub fn set_section_ceiling(&mut self, ceiling: f32) {
        self.section_ceiling = Some(ceiling);
    }

    /// The height this section asks for when the budget is divided: what the
    /// user dragged to, or `None` while collapsed, because a collapsed
    /// section is a header and nothing else.
    ///
    /// Deliberately the CHOSEN height and not [`Self::painted_height`] —
    /// feeding a painted height back into the split makes the two sections
    /// oscillate. See `scm_layout_settings::fit_sections`.
    pub fn chosen_height(&self) -> Option<f32> {
        (!self.is_collapsed()).then(|| f32::from(self.stash_height))
    }

    /// Clamp a candidate against both the window and the shared budget.
    fn clamp(&self, candidate: f32, window_height: f32) -> f32 {
        let windowed = scm_layout_settings::clamp_stash_height(candidate, window_height);
        match self.section_ceiling {
            Some(ceiling) => windowed
                .min(ceiling)
                .max(scm_layout_settings::MIN_STASH_HEIGHT),
            None => windowed,
        }
    }

    /// Apply a candidate height: clamp, persist, notify. No-op when nothing
    /// moves, so a held Arrow key doesn't pelt the settings store.
    fn set_stash_height(&mut self, candidate: f32, window_height: f32, cx: &mut Context<Self>) {
        let clamped = self.clamp(candidate, window_height);
        let new_height = px(clamped);
        if self.stash_height == new_height {
            return;
        }
        self.stash_height = new_height;
        if let Some(repo) = &self.settings_repo {
            scm_layout_settings::save_stash_height(repo, clamped);
        }
        cx.notify();
    }

    /// One mouse drag tick, routed here by `SourceControlPanel`.
    ///
    /// `cursor_y` is the pointer's window-space Y and `window_height` the live
    /// window height, both read off the root listener's bounds so this never
    /// threads a `Window` through nested entity updates. The first tick
    /// latches the pointer origin — drag-start cannot read the cursor — and
    /// every tick after maps the cursor delta onto the height, so dragging UP
    /// grows the section. No-op when no drag is armed.
    pub fn apply_stash_drag(
        &mut self,
        cursor_y: f32,
        window_height: f32,
        cx: &mut Context<Self>,
    ) {
        let Some((start_height, start_y)) = self.drag_anchor else {
            return;
        };
        self.resizing = true;
        let Some(start_y) = start_y else {
            self.drag_anchor = Some((start_height, Some(cursor_y)));
            return;
        };
        self.set_stash_height(start_height + (start_y - cursor_y), window_height, cx);
    }

    /// Translate a key on the resize rail into a height change. `true` when
    /// the key was consumed, so the listener can stop it bubbling.
    fn handle_resize_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let window_height = f32::from(window.bounds().size.height);
        let candidate = scm_layout_settings::next_section_height(
            f32::from(self.stash_height),
            ev.keystroke.key.as_str(),
            ev.keystroke.modifiers.shift,
            scm_layout_settings::MIN_STASH_HEIGHT,
            window_height,
        );
        match candidate {
            Some(candidate) => {
                self.set_stash_height(candidate, window_height, cx);
                true
            }
            None => false,
        }
    }

    /// Mouse drag handle at the section's TOP edge — the boundary it shares
    /// with the file list above. A hairline at rest; a wider `border_active`
    /// bar lights on hover and stays lit for the whole drag. The matching
    /// `on_drag_move` lives on the workspace root, so the cursor stays inside
    /// the listener's bounds even as it travels out of this section.
    pub(super) fn render_drag_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let resizing = self.resizing;
        let weak = cx.entity().downgrade();
        let hover_bar = div()
            .absolute()
            .left_0()
            .right_0()
            .top(px((STASH_RESIZE_HIT_PX - STASH_RESIZE_BAR_PX) / 2.0))
            .h(px(STASH_RESIZE_BAR_PX))
            .bg(theme.border_inactive)
            .group_hover("stash-resize", move |s| s.bg(theme.border_active))
            .when(resizing, |b| b.bg(theme.border_active));
        div()
            .id(ElementId::Name(SharedString::from("stash-resize-handle")))
            .group("stash-resize")
            .relative()
            .w_full()
            .h(px(STASH_RESIZE_HIT_PX))
            .flex_shrink_0()
            .cursor_row_resize()
            .occlude()
            .child(hover_bar)
            .on_drag(StashResizePayload, move |_payload, _offset, _window, cx| {
                let _ = weak.update(cx, |panel, _| {
                    panel.drag_anchor = Some((f32::from(panel.stash_height), None));
                    panel.resizing = true;
                });
                cx.new(|_| StashResizeGhost)
            })
            .into_any_element()
    }

    /// Focusable keyboard rail at the bottom of the section, so the size is
    /// reachable without a pointer. Arrow / Shift+Arrow / Home / End.
    pub(super) fn render_resize_rail(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let bar_color = if self.resize_focus.is_focused(window) {
            theme.focus_ring
        } else {
            theme.border_inactive
        };
        div()
            .id(ElementId::Name(SharedString::from("stash-resize-rail")))
            .track_focus(&self.resize_focus)
            .w_full()
            .h(px(3.0))
            .flex_shrink_0()
            .bg(bar_color)
            .hover(|s| s.bg(theme.focus_ring))
            .cursor_row_resize()
            .on_key_down(cx.listener(|panel, ev: &KeyDownEvent, window, cx| {
                if panel.handle_resize_key(ev, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }
}
