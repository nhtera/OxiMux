//! Render for `PaneGroup`: per-leaf tab strip + active content area.
//!
//! Layout: a horizontal tab strip at the top + the active tab's content
//! filling the remainder. Each tab carries an icon (file vs terminal),
//! a label, and an "×" close button that appears on hover or when the
//! tab is active. Click a tab to activate; mouse-down on the × closes
//! the tab without activating it.
//!
//! Action handlers for `NewTab`/`CloseTab`/`NextTab`/`PrevTab`/`NewAgent`
//! live on this group's root container so keystrokes bubbling up from
//! the focused leaf hit the right group's logic (not the entire
//! workspace's).

use gpui::{
    AnyElement, App, AppContext, Context, DragMoveEvent, Entity, ExternalPaths, InteractiveElement,
    IntoElement, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, div, point, prelude::FluentBuilder, px, svg,
};
use oximux_settings::Theme;

use super::sub_pane::TerminalSplitTree;
use super::tab_drag::{TabDragPayload, TabDragPreview};
use super::{PaneGroup, PaneGroupTab, PaneGroupTabKind, TabDragHover, TabInsertSide};
use crate::actions::{
    ActivateGroupTab, OpenPaneActionsAt, OpenTabContextMenuAt, RequestOpenAdapterPicker,
};
use crate::shell::agent_status_badge::render_dot;
use crate::shell::divider::{ActiveDivider, DividerBoundsCache, split_row_bounds_canvas};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::file_drag::FilePathDragPayload;
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;

pub const TAB_STRIP_HEIGHT_PX: f32 = 28.0;
const TAB_PAD_X_PX: f32 = 12.0;
const CLOSE_BUTTON_SIZE_PX: f32 = 14.0;
const ICON_SIZE_PX: f32 = 11.0;
const PLUS_BUTTON_WIDTH_PX: f32 = 28.0;
/// "..." Pane Actions button width — matches the `+` neighbor so the
/// trailing button cluster stays balanced.
const ELLIPSIS_BUTTON_WIDTH_PX: f32 = 28.0;

impl Render for PaneGroup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drag-cancel cleanup: chips' `on_drag_move` fires only while a
        // drag is live, so a press-Escape (or off-window release) leaves
        // the last hover state painted. Mirrors the body-level guard in
        // `ProjectPanes::render` — if no drag is in flight but our
        // hover is set, clear it so the next paint has no stale insertion
        // bar.
        if !cx.has_active_drag() && self.drag_hover().is_some() {
            self.set_drag_hover(None, cx);
        }

        // Auto-close the pane of any terminal whose shell cleanly exited
        // (queued by the `CleanExit` subscription, which has no window):
        // stacked tab / split leaf / whole tab as the cascade dictates. Done
        // before the element tree is built so this frame paints the post-close
        // state directly.
        self.close_lone_exited_tabs(window, cx);

        // Lazy install — first render is the first paint where we have
        // window + cx + focus_handle in the same scope. Subsequent calls
        // are idempotent.
        self.ensure_mru_focus_out_sub(window, cx);
        // Lazily start the background sweep that flags editor tabs whose file
        // was deleted on disk (renders a strikethrough badge).
        self.ensure_external_mutation_sweep(cx);

        // Push on-screen state down to every TerminalView so hidden tabs can
        // throttle their PTY poll. The shown set = the per-leaf active views of
        // THIS group's active tab (or just the zoomed leaf when one is zoomed);
        // every other view is hidden. Computing that set is cheap (a few
        // leaves); the per-view sweep only runs when the shown set actually
        // changes (tab/leaf-tab switch, split, zoom) — not on every
        // steady-state PTY-output frame.
        let desired: std::collections::HashSet<gpui::EntityId> = match self
            .tabs
            .get(self.active)
            .map(|tab| &tab.content)
        {
            Some(PaneContent::Terminal(tree)) => {
                if let Some(zoomed) = tree.zoomed() {
                    tree.get(zoomed).map(|v| v.entity_id()).into_iter().collect()
                } else {
                    tree.iter_live().map(|(_, v)| v.entity_id()).collect()
                }
            }
            _ => std::collections::HashSet::new(),
        };
        if desired != self.last_visible_ids {
            for tab in self.tabs.iter() {
                let PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                for (_, _, view) in tree.iter_all_views() {
                    let visible = desired.contains(&view.entity_id());
                    view.update(cx, |v, vcx| v.set_visible(visible, vcx));
                }
            }
            self.last_visible_ids = desired;
        }

        // Browser visibility sweep: the native webview floats over the GPU
        // canvas, so it must be hidden whenever its tab isn't the active one
        // OR a modal covers the panes. Cheap — `set_active` early-returns
        // when unchanged. Runs every render so tab switches / modal toggles
        // take effect on the next frame.
        let webview_suppressed = crate::shell::browser_view::webview_suppressed(cx);
        for (idx, tab) in self.tabs.iter().enumerate() {
            if let PaneContent::Browser(view) = &tab.content {
                let show = idx == self.active && !webview_suppressed;
                view.update(cx, |v, _| v.set_active(show));
            }
        }

        let entity = cx.entity().clone();
        let focus_handle = self.focus_handle_clone();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let is_empty = self.tabs.is_empty();

        // PTY grid sizing is driven per-TerminalView from its canvas
        // bounds now (see the note on the former `dispatch_active_grid`),
        // so there's no workspace-level grid dispatch here anymore.

        let group_entity = entity.clone();
        let active_content: Option<AnyElement> = self.active_tab().map(|tab| match &tab.content {
            PaneContent::Terminal(tree) => render_sub_pane_tree(
                tree,
                group_entity.clone(),
                theme,
                self.sub_divider_bounds_cache(),
            ),
            PaneContent::Editor(view) => view.clone().into_any_element(),
            // Diff tabs render the DiffView entity directly as the active
            // pane content — same pattern as Editor. The DiffView owns
            // its own scroll, header, and per-hunk layout.
            PaneContent::Diff(view) => view.clone().into_any_element(),
            // Browser tabs render the BrowserView entity directly — its
            // toolbar + anchor canvas; the native webview floats over it.
            PaneContent::Browser(view) => view.clone().into_any_element(),
            // Tasks tab renders the TasksView entity directly — same pattern
            // as Diff/Browser: the view owns its own scroll and layout.
            PaneContent::Tasks(view) => view.clone().into_any_element(),
            // Agent Chat renders its own transcript + composer, same pattern.
            PaneContent::AgentChat(view) => view.clone().into_any_element(),
        });

        // Empty-pane placeholder: when the user closes the last tab of
        // the only surviving group, render the welcome card (logo +
        // shortcut hints) so the center pane never falls back to a
        // black void. `purge_empty_groups` ensures any non-last empty
        // group has already been removed by the time we get here.
        let empty_placeholder: Option<AnyElement> = is_empty.then(|| {
            crate::shell::welcome_view::view(theme, density, &typography).into_any_element()
        });

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_hidden()
            .when_some(active_content, |s, child| s.child(child))
            .when_some(empty_placeholder, |s, child| s.child(child));

        // Optional MRU HUD overlay — only visible while the user holds
        // Ctrl after pressing Ctrl+Tab. Rendered AFTER body in the same
        // outer flex column then positioned absolute over it via the
        // helper's own styling.
        let mru_overlay: Option<AnyElement> = self.mru_switcher().map(|state| {
            render_mru_hud(state.snapshot.as_slice(), state.cursor, &self.tabs, theme)
        });

        // While a sub-pane divider is held, a topmost occluding overlay
        // captures all moves (so resize tracks past the divider hitbox and
        // no stray events reach the terminals beneath) until release.
        let sub_divider_capture: Option<AnyElement> = self
            .active_sub_divider()
            .map(|active| sub_divider_capture_overlay(entity.clone(), active.axis));

        // Prompt composer bar, docked at the bottom of the active agent pane,
        // shown ONLY over its origin agent (so it can never render on, or submit
        // to, a different tab).
        let composer_overlay: Option<AnyElement> = self.composer_for_active_tab().map(|composer| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(12.0))
                .px(px(24.0))
                .flex()
                .justify_center()
                .child(div().w_full().max_w(px(720.0)).child(composer.clone()))
                .into_any_element()
        });

        div()
            .id(SharedString::from(format!(
                "pane-group-{}",
                entity.entity_id()
            )))
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .on_action(cx.listener(PaneGroup::on_new_tab))
            .on_action(cx.listener(PaneGroup::on_new_browser_tab))
            .on_action(cx.listener(PaneGroup::on_new_tab_in_pane))
            .on_action(cx.listener(PaneGroup::on_close_tab))
            .on_action(cx.listener(PaneGroup::on_next_tab))
            .on_action(cx.listener(PaneGroup::on_prev_tab))
            .on_action(cx.listener(PaneGroup::on_mru_next))
            .on_action(cx.listener(PaneGroup::on_mru_prev))
            .on_action(cx.listener(PaneGroup::on_new_agent))
            .on_action(cx.listener(PaneGroup::on_new_agent_chat))
            .on_action(cx.listener(PaneGroup::on_open_composer_bar))
            .on_action(cx.listener(PaneGroup::on_split_sub_pane_right))
            .on_action(cx.listener(PaneGroup::on_split_sub_pane_down))
            .on_action(cx.listener(PaneGroup::on_focus_next_sub_pane))
            .on_action(cx.listener(PaneGroup::on_focus_prev_sub_pane))
            .on_action(cx.listener(PaneGroup::on_toggle_zoom_sub_pane))
            // Modifier-up listener — commits the MRU switch when the
            // user releases Ctrl. Standard hold-modifier switcher
            // pattern. We commit when `ctrl` flips OFF
            // and a switcher is currently open; everything else is a
            // no-op so other modifier transitions don't disturb state.
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, window, cx| {
                if this.mru_switcher().is_some() && !ev.control {
                    this.commit_mru_switch(window, cx);
                }
            }))
            .child(body)
            .when_some(composer_overlay, |s, overlay| s.child(overlay))
            .when_some(sub_divider_capture, |s, overlay| s.child(overlay))
            .when_some(mru_overlay, |s, overlay| s.child(overlay))
            // Unsaved-changes prompt for a dirty editor tab close. Modal
            // overlay (occludes the panes) centered over this group; built
            // per-request, `None` when idle. Rendered last so it sits on top.
            .when_some(self.dirty_close_dialog(), |s, dialog| {
                s.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
    }
}

/// Build a tab strip element for a `PaneGroup` entity. Free function
/// so `ProjectPanes` can render the strip externally — either hoisted
/// into the top bar (for the topmost group) or wrapped above the
/// group's body (for non-topmost groups).
///
/// `is_focused` controls visibility of the per-pane "..." button (only
/// the focused group shows it, matching the reference editor's
/// behavior). `show_pane_actions` is `false` for the hoisted topmost
/// strip — the top bar already renders its own "..." button there.
#[allow(clippy::too_many_arguments)]
pub fn build_tab_strip_for(
    entity: Entity<PaneGroup>,
    group_id: PaneGroupId,
    is_focused: bool,
    show_pane_actions: bool,
    theme: Theme,
    workspace_tint: Option<super::TabColor>,
    project_panes: Entity<ProjectPanes>,
    cx: &App,
) -> AnyElement {
    let group = entity.read(cx);
    // Walk `visible_tabs` so reordered chips render in their new slot
    // while their original insertion idx (used by click / close /
    // context-menu handlers) is preserved.
    let tabs: Vec<PaneGroupTabHeader> = group
        .visible_tabs()
        .map(|(idx, t)| {
            // A plain terminal running a recognized agent adopts the agent
            // chip wholesale — icon, status dot, and name — so a hand-launched
            // agent is visually identical to a spawned one (a user-set custom
            // title still wins for the label).
            let ambient = detect_tab_agent(t, cx);
            PaneGroupTabHeader {
                tab_idx: idx,
                label: t.custom_title.clone().unwrap_or_else(|| {
                    // Browser tabs show the live page title (falling back to the
                    // URL host) read from the BrowserView, so the chip tracks
                    // navigation. A user-set custom title still wins above.
                    if let PaneContent::Browser(view) = &t.content {
                        view.read(cx).chrome_label()
                    } else {
                        // Label precedence: recognized agent chip > the
                        // program's OSC 2 window title > the static slug.
                        // (A user-set `custom_title` already won above and
                        // pins the name against further OSC-2 overrides.)
                        ambient
                            .as_ref()
                            .map(|a| a.label.into())
                            .or_else(|| terminal_osc2_title(t, cx))
                            .unwrap_or_else(|| t.label.clone())
                    }
                }),
                kind_marker: ambient
                    .as_ref()
                    .map(|a| PaneTabKindMarker::Agent(a.adapter_id))
                    .unwrap_or_else(|| kind_marker(&t.kind)),
                agent_status: ambient
                    .as_ref()
                    .map(|a| a.status.clone())
                    .or_else(|| agent_status_for(&t.kind)),
                attention: attention_for(&t.content, cx),
                color: t.color,
                pinned: t.pinned,
                is_preview: t.is_preview,
                external_mutation: t.external_mutation,
            }
        })
        .collect();
    let active = group.active();
    let drag_hover = group.drag_hover();
    let scroll_handle = group.tab_strip_scroll_handle();
    let _ = group;
    build_tab_strip_from_headers(
        entity,
        group_id,
        &tabs,
        active,
        drag_hover,
        is_focused,
        show_pane_actions,
        theme,
        workspace_tint,
        scroll_handle,
        project_panes,
    )
}

/// Lightweight projection of a `PaneGroupTab` that the strip render
/// needs. Avoids holding a borrow of the group while we build elements
/// (the strip itself uses `entity` for click handlers).
struct PaneGroupTabHeader {
    /// Insertion-order index — what click / close / drag handlers
    /// pass back to the group. Distinct from the chip's visual
    /// position once the user has reordered tabs.
    tab_idx: usize,
    label: SharedString,
    kind_marker: PaneTabKindMarker,
    agent_status: Option<oximux_core::AgentStatus>,
    /// True when a terminal in this tab has a pending attention signal
    /// (unfocused-pane bell). Lights the tab chip so a background bell is
    /// visible even when its pane isn't shown.
    attention: bool,
    /// User-picked color tag, rendered as a 2px left-edge bar on the
    /// chip. `None` = no color (default chrome).
    color: Option<super::TabColor>,
    /// Pinned state — drives the pin-icon-in-place-of-close button on
    /// the chip.
    pinned: bool,
    /// Reusable single-click preview tab — rendered with an italic label.
    is_preview: bool,
    /// On-disk mutation of the backing file (deleted/renamed) — rendered as a
    /// strikethrough label plus a trailing suffix.
    external_mutation: Option<super::ExternalMutation>,
}

#[derive(Clone, Copy)]
enum PaneTabKindMarker {
    Terminal,
    /// Agent tab — carries the registry adapter slug so the chip can show a
    /// per-adapter brand glyph instead of the generic terminal icon.
    Agent(&'static str),
    Editor,
    /// Diff tabs reuse the Editor chip styling (file icon, no agent
    /// status badge) but keep their own marker so future visual
    /// differentiation (e.g. a `±` adornment) is a single-line change.
    Diff,
    /// Browser tabs — globe glyph, no agent status badge.
    Browser,
    /// Tasks tab — list-tree glyph, no agent status badge.
    Tasks,
    /// Agent Chat tab — message glyph, no agent status badge (status is shown
    /// inline in the transcript, not on the chip).
    AgentChat,
}

/// A recognized agent running inside a plain terminal tab, detected live from
/// its OSC title. Drives the tab chip's name, brand icon, and status dot so a
/// hand-launched agent presents identically to a spawned one.
struct TabAgent {
    label: &'static str,
    /// Registry adapter slug for the brand glyph (`agent_icon`), or `""` when
    /// the agent isn't one with a dedicated icon (falls back to terminal).
    adapter_id: &'static str,
    status: oximux_core::AgentStatus,
}

/// Detect an agent in a *plain* terminal tab from the active view's OSC title.
/// `None` for non-terminal tabs, terminals with no title, or titles that don't
/// classify as agent activity. Spawned `Agent` tabs are left to their own
/// `status_rx`-driven chip.
fn detect_tab_agent(tab: &PaneGroupTab, cx: &App) -> Option<TabAgent> {
    if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
        return None;
    }
    let PaneContent::Terminal(tree) = &tab.content else {
        return None;
    };
    let title = tree.active_view()?.read(cx).title()?;
    let status = oximux_agents::classify_agent_title(title)?;
    let label = oximux_agents::agent_label_from_title(title)?;
    Some(TabAgent {
        label,
        adapter_id: crate::shell::agent_presentation::adapter_id_for_label(label),
        status,
    })
}

/// The active terminal view's OSC 2 window title for a *plain* terminal tab,
/// trimmed and non-empty, used as the chip label when the user hasn't set a
/// custom title and the title doesn't classify as agent activity (e.g. `vim`
/// or `ssh` setting the window title). `None` for non-terminal tabs or when no
/// title has been emitted. Pull-read at render time, so it tracks live without
/// any push plumbing; `custom_title` (manual rename) still wins above this.
fn terminal_osc2_title(tab: &PaneGroupTab, cx: &App) -> Option<SharedString> {
    if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
        return None;
    }
    let PaneContent::Terminal(tree) = &tab.content else {
        return None;
    };
    let view = tree.active_view()?;
    let title = view.read(cx).title()?;
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    // Cap pathological titles (a shell that sets its title to the full cwd) so
    // the chip layout never has to lay out a multi-hundred-char string; the
    // chip truncates visually too, but this bounds the work up front.
    const MAX_TITLE_CHARS: usize = 80;
    let capped: String = title.chars().take(MAX_TITLE_CHARS).collect();
    Some(SharedString::from(capped))
}

fn kind_marker(kind: &PaneGroupTabKind) -> PaneTabKindMarker {
    match kind {
        PaneGroupTabKind::Terminal => PaneTabKindMarker::Terminal,
        PaneGroupTabKind::Agent { adapter_id, .. } => PaneTabKindMarker::Agent(adapter_id),
        PaneGroupTabKind::Editor { .. } => PaneTabKindMarker::Editor,
        // Commit-detail tabs share the Diff marker — same `±` semantics
        // and same chip styling. The dedup key differs (SHA vs path)
        // but the visual chrome is identical.
        PaneGroupTabKind::Diff { .. }
        | PaneGroupTabKind::Commit { .. }
        | PaneGroupTabKind::BranchFile { .. }
        | PaneGroupTabKind::CombinedDiff { .. } => PaneTabKindMarker::Diff,
        PaneGroupTabKind::Browser { .. } => PaneTabKindMarker::Browser,
        PaneGroupTabKind::Tasks => PaneTabKindMarker::Tasks,
        PaneGroupTabKind::AgentChat { .. } => PaneTabKindMarker::AgentChat,
    }
}

fn agent_status_for(kind: &PaneGroupTabKind) -> Option<oximux_core::AgentStatus> {
    if let PaneGroupTabKind::Agent { status_rx, .. } = kind {
        Some(status_rx.borrow().status.clone())
    } else {
        None
    }
}

/// Per-adapter brand glyph for an agent tab. Slugs match the agent
/// registry; unknown / custom adapters fall back to the generic terminal
/// glyph so a tab always has an icon.
fn agent_icon(adapter_id: &str) -> &'static str {
    match adapter_id {
        "claude-code" => "icons/claude-code.svg",
        "codex" => "icons/codex.svg",
        "aider" => "icons/aider.svg",
        _ => "icons/square-terminal.svg",
    }
}

/// True when any sub-pane terminal in this tab has a pending attention
/// signal (an unfocused-pane bell). Lights the tab chip so a bell in a
/// BACKGROUND tab is visible — the pane ring alone shows nothing when the
/// pane isn't on screen. Editor/Diff tabs never signal attention.
fn attention_for(content: &crate::shell::pane_content::PaneContent, cx: &App) -> bool {
    use crate::shell::pane_content::PaneContent;
    match content {
        // iter_all_views (not iter_live) so a bell from a BACKGROUND
        // per-pane tab still lights the workspace tab chip.
        PaneContent::Terminal(tree) => tree
            .iter_all_views()
            .any(|(_, _, v)| v.read(cx).attention()),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_tab_strip_from_headers(
    entity: Entity<PaneGroup>,
    group_id: PaneGroupId,
    tabs: &[PaneGroupTabHeader],
    active: usize,
    drag_hover: Option<TabDragHover>,
    is_focused: bool,
    show_pane_actions: bool,
    theme: Theme,
    workspace_tint: Option<super::TabColor>,
    scroll_handle: gpui::ScrollHandle,
    project_panes: Entity<ProjectPanes>,
) -> AnyElement {
    let entity_id = entity.entity_id();
    let strip_hover_entity = entity.clone();
    let strip_drop_entity = entity.clone();
    // Cross-group strip drop needs `ProjectPanes` to steal+insert the tab.
    let strip_drop_panes = project_panes.clone();
    let strip_file_hover_entity = entity.clone();
    let strip_file_drop_entity = entity.clone();
    // F4.6: parallel pair for the OS-level (Finder) native drop. GPUI
    // routes Finder drops as an internal drag with `ExternalPaths`
    // payload, so the dispatch is identical — we just need a second
    // listener registered for that type.
    let strip_native_hover_entity = entity.clone();
    let strip_native_drop_entity = entity.clone();
    // Visible insertion slot the drag would land on. `Before` → slot is
    // at target_visible_idx; `After` → slot is at target_visible_idx + 1.
    // Both adjacent chips paint a single edge each so the user reads ONE
    // continuous 2px bar between them (adjacent-edge tab-indicator
    // pattern).
    let insertion_slot = drag_hover.map(|h| match h.side {
        TabInsertSide::Before => h.target_visible_idx,
        TabInsertSide::After => h.target_visible_idx + 1,
    });

    // Clone for the wheel-scroll handler — the original `scroll_handle`
    // is still consumed by `.track_scroll(&scroll_handle)` below.
    let wheel_scroll_handle = scroll_handle.clone();

    // Inner scroll container: ONLY the tab chips scroll. The "+" and
    // "..." buttons sit OUTSIDE this div so they stay pinned at the
    // right edge of the strip even when many tabs overflow. The
    // `track_scroll(handle)` wires this viewport to the group's
    // `ScrollHandle` so `PaneGroup::pin_tab_strip_to_end` (called on
    // every tab append) snaps the viewport to its right edge.
    let mut chips = div()
        .id(SharedString::from(format!(
            "pane-group-tab-strip-{entity_id}"
        )))
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .flex_1()
        .min_w(px(0.0))
        .overflow_x_scroll()
        .overflow_y_hidden()
        .track_scroll(&scroll_handle)
        // Strip-level capture-phase pass: clears the hover state so a
        // cursor that has left every chip (over the `+`, the trailing
        // spacer, or outside the strip entirely) doesn't keep the last
        // chip's insertion bar pinned on. Each chip's own on_drag_move
        // re-sets the hover when the cursor is inside its bounds. Applies
        // to foreign-source drags too (cross-group strip drop) so the
        // insertion bar can preview the destination slot.
        .on_drag_move::<TabDragPayload>(move |_ev: &DragMoveEvent<TabDragPayload>, _window, cx| {
            strip_hover_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
        })
        // Strip drop handler. Same-group reorder is performed by the
        // chip-level on_drop (which also fires on the bubble path) — here
        // we only zero the hover for it. A FOREIGN source dropped on this
        // strip moves its tab INTO this group at the slot the insertion
        // bar previewed (cross-group strip drop). A drop with no live
        // hover (over the `+` gap / trailing spacer) appends.
        .on_drop::<TabDragPayload>(move |payload: &TabDragPayload, window, cx| {
            if payload.source_group == group_id {
                strip_drop_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
                return;
            }
            // Resolve the destination visible slot from the live hover
            // hint before clearing it. No hover → append at the end.
            let dest = strip_drop_entity.read(cx);
            let visible_slot = dest
                .drag_hover()
                .map(|h| match h.side {
                    TabInsertSide::Before => h.target_visible_idx,
                    TabInsertSide::After => h.target_visible_idx + 1,
                })
                .unwrap_or_else(|| dest.tab_count());
            strip_drop_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
            strip_drop_panes.update(cx, |p, cx| {
                p.transfer_tab_at(
                    payload.source_group,
                    payload.source_tab_idx,
                    group_id,
                    visible_slot,
                    window,
                    cx,
                );
            });
        })
        // File-tree drag passing through the strip — same capture-pass
        // clear so a cursor that's left every chip (over `+`, trailing
        // spacer, or outside the strip) loses the insertion bar. Each
        // chip's own FilePathDragPayload `on_drag_move` re-sets the
        // hover when the cursor returns inside its bounds.
        .on_drag_move::<FilePathDragPayload>(
            move |_: &DragMoveEvent<FilePathDragPayload>, _window, cx| {
                strip_file_hover_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
            },
        )
        // File drop in the strip but outside any chip's bounds (e.g. on
        // the `+` button gap) → append the file at the end. Same hover
        // cleanup so the insertion bar disappears.
        .on_drop::<FilePathDragPayload>(move |payload: &FilePathDragPayload, window, cx| {
            let path = payload.path.clone();
            strip_file_drop_entity.update(cx, |g, cx| {
                g.set_drag_hover(None, cx);
                g.open_or_activate_editor_tab_at(path, None, false, window, cx);
            });
        })
        // F4.6: OS-native Finder drop variant. GPUI synthesises an
        // internal drag carrying `ExternalPaths` when the user drags
        // files from Finder over the window; same capture-pass + drop
        // logic as the in-app file-tree drag.
        .on_drag_move::<ExternalPaths>(move |_: &DragMoveEvent<ExternalPaths>, _window, cx| {
            strip_native_hover_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
        })
        .on_drop::<ExternalPaths>(move |payload: &ExternalPaths, window, cx| {
            let paths = filter_droppable_files(payload.paths());
            strip_native_drop_entity.update(cx, |g, cx| {
                g.set_drag_hover(None, cx);
                // Multi-file Finder drops append in sequence so the
                // user reads them left-to-right in the strip.
                for path in paths {
                    g.open_or_activate_editor_tab_at(path, None, false, window, cx);
                }
            });
        })
        // Mouse-wheel horizontal scroll: map vertical wheel delta onto
        // the strip's horizontal scroll offset. Trackpad horizontal
        // swipes already flow through the native `overflow_x_scroll`
        // axis, so they arrive as `delta.x` and bypass this remap.
        // GPUI's `ScrollDelta::pixel_delta(line_height)` normalizes
        // pixel-deltas (trackpad) and line-deltas (USB wheel) to a
        // consistent pixel space; `TAB_STRIP_HEIGHT_PX` is a sensible
        // line-height for chips that are exactly one strip tall.
        .on_scroll_wheel(move |ev: &ScrollWheelEvent, _window, _cx| {
            let pixel_delta: Point<Pixels> = ev.delta.pixel_delta(px(TAB_STRIP_HEIGHT_PX));
            let dy = pixel_delta.y;
            if f32::from(dy).abs() < f32::EPSILON {
                return;
            }
            let current = wheel_scroll_handle.offset();
            wheel_scroll_handle.set_offset(point(current.x - dy, current.y));
        });
    let tab_count = tabs.len();
    for (visible_idx, header) in tabs.iter().enumerate() {
        let tab_idx = header.tab_idx;
        // Two-edge insertion bar: this chip paints a Right bar when the
        // slot is just AFTER it, and a Left bar when the slot is at this
        // chip's position. The two adjacent edges combine into one
        // visually continuous 2px line between chips.
        let drag_edge: Option<TabInsertSide> = insertion_slot.and_then(|slot| {
            if slot == visible_idx + 1 {
                Some(TabInsertSide::After)
            } else if slot == visible_idx {
                Some(TabInsertSide::Before)
            } else {
                None
            }
        });
        chips = chips.child(render_tab_chip(
            entity_id.as_u64(),
            group_id,
            tab_idx,
            visible_idx,
            header.label.clone(),
            header.kind_marker,
            header.agent_status.as_ref(),
            header.attention,
            tab_idx == active,
            is_focused,
            drag_edge,
            header.color,
            header.pinned,
            header.is_preview,
            header.external_mutation,
            theme,
            workspace_tint,
            entity.clone(),
            project_panes.clone(),
        ));
    }
    let _ = tab_count;

    // Outer container holds the scroll viewport + pinned trailing
    // cluster. `flex_row` keeps everything on one line; the inner
    // `chips` div takes flex_1 so it absorbs all remaining width while
    // the trailing buttons stay flex-shrink-0.
    //
    // The strip's bottom edge is a single neutral divider separating the
    // tab row from the content below — it never carries the accent. Focus
    // and the active tab are signalled by the active chip's own bottom
    // indicator (see `render_tab_chip`), so there's one localized accent
    // cue under the active tab instead of a full-width colored line.
    let border_color = theme.border_inactive;
    // Scroll-edge fade hints. Subtle 8px gradients painted absolutely
    // over the strip's left and right edges of the SCROLLING region
    // (right edge offset by the trailing cluster so the fade doesn't
    // bleed onto the `+` / `...` buttons). Visibility is keyed off the
    // scroll handle: show LEFT when the user has scrolled right (offset
    // negative); show RIGHT when there's more content offscreen to the
    // right. When both checks are false the strip fits exactly.
    let offset_x = f32::from(scroll_handle.offset().x);
    let max_offset_x = f32::from(scroll_handle.max_offset().x);
    let show_left_fade = offset_x < -0.5;
    let show_right_fade = offset_x > -(max_offset_x - 0.5);
    let trailing_cluster_px = if show_pane_actions {
        PLUS_BUTTON_WIDTH_PX + ELLIPSIS_BUTTON_WIDTH_PX
    } else {
        PLUS_BUTTON_WIDTH_PX
    };
    let mut row = div()
        .flex()
        .flex_row()
        .items_stretch()
        .h(px(TAB_STRIP_HEIGHT_PX))
        .w_full()
        .relative()
        .bg(theme.bg_panel)
        .border_b_2()
        .border_color(border_color)
        .child(chips)
        .child(plus_button(entity_id.as_u64(), theme));
    if show_pane_actions {
        row = row.child(pane_actions_button(entity_id.as_u64(), is_focused, theme));
    }
    // Overlays last so they paint on top of the chips.
    if show_left_fade {
        row = row.child(scroll_fade_overlay(true, trailing_cluster_px, theme));
    }
    if show_right_fade {
        row = row.child(scroll_fade_overlay(false, trailing_cluster_px, theme));
    }
    row.into_any_element()
}

// NOTE: PTY grid sizing moved INTO each `TerminalView`'s canvas paint
// closure (`sync_grid_to_canvas`), which derives (cols, rows) from the
// canvas's real bounds. That removed the old `dispatch_active_grid`
// estimate (viewport − hardcoded chrome), which drifted whenever the
// assumed chrome height/width didn't match the live layout and caused
// full-screen TUIs to render their absolute-positioned UI for the wrong
// column count (scrambled output). The canvas-bounds path also fixes
// split sub-panes for free — each sub-pane sizes its PTY to its own
// slice rather than inheriting a full-width estimate.

/// Recursively render a `TerminalSplitTree` into a flex layout. Single-
/// pane trees collapse to just the sub-pane view; split nodes become
/// flex-row (horizontal axis) or flex-col (vertical axis) containers
/// with `border_color(theme.border_active)` dividers between children.
///
/// Zoom short-circuit: when `tree.zoomed()` is `Some(idx)`, the named
/// sub-pane fills the body and the rest of the tree is ignored (other
/// PTYs stay alive in memory — toggle off to restore).
///
/// Active-pane rim glow is deferred — the tree highlights the active
/// pane via focus state, which TerminalView paints on its own.
fn render_sub_pane_tree(
    tree: &TerminalSplitTree,
    group: Entity<PaneGroup>,
    theme: Theme,
    bounds_cache: DividerBoundsCache,
) -> AnyElement {
    // Zoom fast path: bypass tree traversal entirely.
    if let Some(idx) = tree.zoomed()
        && let Some(view) = tree.get(idx)
    {
        return view.clone().into_any_element();
    }
    build_sub_pane_node(tree.tree(), tree, group, theme, &[], &bounds_cache)
        .unwrap_or_else(|| div().size_full().into_any_element())
}

/// Recursive walker for `render_sub_pane_tree`. Each Split node renders
/// as a flex row/col with interactive resize handles between siblings;
/// each Leaf renders as a `TerminalView`. `path` tracks the index trail
/// to the current Split so divider drags can target it precisely via
/// `PaneGroup::set_active_sub_pane_weights`.
fn build_sub_pane_node(
    node: &crate::shell::pane_tree::PaneTree<usize>,
    tree: &TerminalSplitTree,
    group: Entity<PaneGroup>,
    theme: Theme,
    path: &[usize],
    bounds_cache: &DividerBoundsCache,
) -> Option<AnyElement> {
    use crate::shell::pane_tree::{Axis, PaneTree};
    match node {
        PaneTree::Leaf(idx) => render_leaf(*idx, tree),
        PaneTree::Split {
            axis,
            children,
            weights,
        } => {
            let split_axis = *axis;
            let sum: f32 = weights.iter().sum();
            let sum = if sum > 0.0 { sum } else { 1.0 };
            // Path owned for the bounds canvas / divider handlers so it
            // survives the render closure dropping. Cheap clone — usually
            // 0..3 entries deep.
            let split_path = path.to_vec();
            let weights_snapshot = weights.clone();
            // `.relative()` anchors the bounds-recording canvas to this
            // sub-pane split row; the canvas records its geometry into the
            // group's cache so the divider MouseDown can seed the resize.
            let mut row = div().flex().relative().size_full().overflow_hidden();
            row = match split_axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            row = row.child(split_row_bounds_canvas(
                bounds_cache.clone(),
                split_path.clone(),
            ));
            for (i, (child, weight)) in children.iter().zip(weights_snapshot.iter()).enumerate() {
                let frac = weight / sum;
                let mut slot = div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden();
                slot = match split_axis {
                    Axis::Horizontal => slot.w(gpui::relative(frac)).h_full(),
                    Axis::Vertical => slot.h(gpui::relative(frac)).w_full(),
                };
                let mut child_path = split_path.clone();
                child_path.push(i);
                if let Some(child_el) =
                    build_sub_pane_node(child, tree, group.clone(), theme, &child_path, bounds_cache)
                {
                    row = row.child(slot.child(child_el));
                } else {
                    row = row.child(slot);
                }
                if i + 1 < children.len() {
                    row = row.child(sub_pane_divider(
                        split_path.clone(),
                        i,
                        split_axis,
                        weights_snapshot.clone(),
                        theme,
                        group.clone(),
                        bounds_cache.clone(),
                    ));
                }
            }
            Some(row.into_any_element())
        }
    }
}

/// Render one split leaf — exactly one terminal per pane (no per-pane tab
/// strip). Returns the bare terminal view; the active pane is signalled by
/// the terminal's own focus-flash rim (painted inside `TerminalView` on
/// focus change) plus the inactive-pane dim, matching the reference app —
/// no persistent outline box, so the split stays clean.
fn render_leaf(idx: usize, tree: &TerminalSplitTree) -> Option<AnyElement> {
    let leaf = tree.leaf(idx)?;
    Some(leaf.active_view().clone().into_any_element())
}

/// Hit-area padding around the visible 1 px stripe — matches the
/// workspace-level divider so the drag-pickup feel is consistent.
const SUB_PANE_DIVIDER_HIT_PAD_PX: f32 = 3.0;

/// Build an interactive resize handle for a sub-pane divider. A flush 1 px
/// hairline (only 1 px of layout, so panes stay edge-to-edge) plus a
/// transparent hit overlay extended `SUB_PANE_DIVIDER_HIT_PAD_PX` past the
/// line on each side; cursor flips to col/row-resize over it. Resize uses
/// direct mouse capture: MouseDown arms the divider on `PaneGroup` (seeding
/// the parent split-row bounds from `bounds_cache`); the group's capture
/// overlay resizes on MouseMove and disarms on MouseUp. Double-click resets
/// the split to equal.
#[allow(clippy::too_many_arguments)]
fn sub_pane_divider(
    path: Vec<usize>,
    divider_idx: usize,
    axis: crate::shell::pane_tree::Axis,
    weights: Vec<f32>,
    theme: Theme,
    group: Entity<PaneGroup>,
    bounds_cache: DividerBoundsCache,
) -> AnyElement {
    use crate::shell::pane_tree::Axis;
    let id_string = format!(
        "sub-pane-divider-{}-{}",
        path.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("-"),
        divider_idx,
    );
    let pad = SUB_PANE_DIVIDER_HIT_PAD_PX;
    let span = px(1.0 + 2.0 * pad);
    // A flush 1px hairline: it consumes only 1px of layout, so the two panes
    // sit edge-to-edge with one subtle seam (the same `border_inactive` the
    // workspace divider uses) instead of a wide bright gutter. The draggable
    // region is a transparent overlay extended past the line on both sides,
    // so the thin line stays easy to grab without widening the visible seam.
    let line = div().relative().flex_shrink_0().bg(theme.border_inactive);
    let line = match axis {
        Axis::Horizontal => line.w(px(1.0)).h_full(),
        Axis::Vertical => line.h(px(1.0)).w_full(),
    };
    let hot = div()
        .id(SharedString::from(id_string))
        .absolute()
        .occlude();
    let hot = match axis {
        Axis::Horizontal => hot
            .top(px(0.0))
            .h_full()
            .left(px(-pad))
            .w(span)
            .cursor_col_resize(),
        Axis::Vertical => hot
            .left(px(0.0))
            .w_full()
            .top(px(-pad))
            .h(span)
            .cursor_row_resize(),
    };
    line.child(
        hot.on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _window, cx| {
            // Double-click resets the split to equal weights — don't arm.
            if ev.click_count >= 2 {
                let path = path.clone();
                group.update(cx, |g, cx| g.reset_sub_split_to_equal(&path, cx));
                cx.stop_propagation();
                return;
            }
            // First frame may have no cached bounds yet — ignore the press.
            let Some(bounds) = bounds_cache.borrow().get(&path).copied() else {
                return;
            };
            let active = ActiveDivider {
                split_path: path.clone(),
                divider_idx,
                axis,
                initial_weights: weights.clone(),
                container_origin: bounds.origin,
                container_size: bounds.size,
            };
            group.update(cx, |g, cx| g.arm_sub_divider(active, cx));
            cx.stop_propagation();
        }),
    )
    .into_any_element()
}

/// Full-area capture overlay rendered on top of a `PaneGroup` body while a
/// sub-pane divider is armed. Occluding + topmost so MouseMove only reaches
/// it (no stray events to the terminals beneath), resizing on every move.
/// MouseUp (over it or elsewhere in-window) disarms; an off-window release
/// is caught by the next move's released-button check.
fn sub_divider_capture_overlay(
    group: Entity<PaneGroup>,
    axis: crate::shell::pane_tree::Axis,
) -> AnyElement {
    use crate::shell::pane_tree::Axis;
    let mut overlay = div().absolute().inset_0().occlude();
    overlay = match axis {
        Axis::Horizontal => overlay.cursor_col_resize(),
        Axis::Vertical => overlay.cursor_row_resize(),
    };
    let move_group = group.clone();
    let down_group = group.clone();
    let up_group = group.clone();
    let up_out_group = group.clone();
    overlay
        // Catch the second click of a double-click here — it lands on this
        // overlay (raised by the first click's arm), not the divider hitbox.
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _window, cx| {
            if ev.click_count >= 2 {
                down_group.update(cx, |g, cx| g.reset_sub_divider_double_click(cx));
            }
        })
        .on_mouse_move(move |ev: &MouseMoveEvent, _window, cx| {
            if ev.pressed_button != Some(MouseButton::Left) {
                move_group.update(cx, |g, cx| g.disarm_sub_divider(cx));
                return;
            }
            move_group.update(cx, |g, cx| {
                g.resize_active_sub_divider(ev.position, cx);
            });
        })
        .on_mouse_up(
            MouseButton::Left,
            move |_ev: &MouseUpEvent, _window, cx| {
                up_group.update(cx, |g, cx| g.disarm_sub_divider(cx));
            },
        )
        .on_mouse_up_out(
            MouseButton::Left,
            move |_ev: &MouseUpEvent, _window, cx| {
                up_out_group.update(cx, |g, cx| g.disarm_sub_divider(cx));
            },
        )
        .into_any_element()
}

/// Compute new sub-pane weights when divider `divider_idx` is dragged
/// to `frac` of the parent split (0.0 = left/top edge, 1.0 = right/
/// bottom). Only the two children adjacent to the divider redistribute
/// — outer siblings keep their initial weight. Matches
/// `redistribute_weights` in project_panes/render.rs; duplicated here
/// because the two callers operate on different leaf-id types and the
/// math itself is identical and tiny.
pub(super) fn redistribute_sub_pane_weights(
    initial: &[f32],
    divider_idx: usize,
    frac: f32,
) -> Vec<f32> {
    let mut out = initial.to_vec();
    if divider_idx + 1 >= initial.len() {
        return out;
    }
    let sum: f32 = initial.iter().sum();
    if sum <= 0.0 {
        return out;
    }
    let before_pair: f32 = initial[..divider_idx].iter().sum();
    let pair_sum = initial[divider_idx] + initial[divider_idx + 1];
    if pair_sum <= 0.0 {
        return out;
    }
    let target_total = frac * sum;
    let frac_in_pair = ((target_total - before_pair) / pair_sum).clamp(0.0, 1.0);
    out[divider_idx] = pair_sum * frac_in_pair;
    out[divider_idx + 1] = pair_sum * (1.0 - frac_in_pair);
    out
}

/// F4.6: filter an OS-native drop's payload down to paths we can open
/// as editor tabs. Directories are silently dropped — opening a folder
/// as a tab makes no sense (and there's no project-open contract here
/// yet); a future plan can wire directory drops into the workspace
/// picker. Nonexistent paths are also skipped because the editor view
/// would surface a missing-file placeholder for them anyway; better to
/// noop the drop than create a junk tab.
///
/// `pub(crate)` so `project_panes::render` can share the same filter
/// from its body-zone drop handler.
pub(crate) fn filter_droppable_files(paths: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    paths.iter().filter(|p| p.is_file()).cloned().collect()
}

#[allow(clippy::too_many_arguments)]
fn render_tab_chip(
    entity_id_raw: u64,
    group_id: PaneGroupId,
    ix: usize,
    visible_idx: usize,
    label: SharedString,
    marker: PaneTabKindMarker,
    agent_status: Option<&oximux_core::AgentStatus>,
    attention: bool,
    is_active: bool,
    is_focused: bool,
    drag_edge: Option<TabInsertSide>,
    color_tag: Option<super::TabColor>,
    is_pinned: bool,
    is_preview: bool,
    external_mutation: Option<super::ExternalMutation>,
    theme: Theme,
    workspace_tint: Option<super::TabColor>,
    entity: Entity<PaneGroup>,
    project_panes: Entity<ProjectPanes>,
) -> impl IntoElement {
    let icon_path = match marker {
        // Diff tabs reuse the editor "file" glyph until a dedicated
        // diff icon ships (deferred — see plan phase 04 file-type icons).
        PaneTabKindMarker::Editor | PaneTabKindMarker::Diff => "icons/file.svg",
        PaneTabKindMarker::Terminal => "icons/square-terminal.svg",
        PaneTabKindMarker::Browser => "icons/globe.svg",
        PaneTabKindMarker::Tasks => "icons/list-tree.svg",
        PaneTabKindMarker::AgentChat => "icons/sparkles.svg",
        PaneTabKindMarker::Agent(adapter_id) => agent_icon(adapter_id),
    };
    let icon_color = if is_active {
        theme.fg_muted
    } else {
        theme.fg_subtle
    };
    let text_color = if is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    // The active tab's bottom edge carries a 2px accent underline (the
    // active-tab indicator), in the active workspace's tint when one is set
    // (a workspace identifier), falling back to the focus ring otherwise.
    // The tint is workspace-level, so in a split every group's active tab
    // wears it — deliberate (it identifies the workspace, not the focused
    // pane). The unfocused group's underline is dimmed so the focused
    // group's active tab still reads as the brightest cue.
    let active_indicator: Option<AnyElement> = is_active.then(|| {
        let accent: gpui::Hsla = match workspace_tint {
            Some(c) => gpui::rgb(c.rgb()).into(),
            None => theme.focus_ring,
        };
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(2.0))
            .bg(accent)
            .when(!is_focused, |s| s.opacity(0.45))
            .into_any_element()
    });

    let group_name = SharedString::from(format!("pane-group-tab-{entity_id_raw}-{ix}"));
    let activate_entity = entity.clone();
    let hover_entity = entity.clone();
    let drop_entity = entity.clone();
    // Cross-group strip drop: GPUI's `on_drop` consumes the active drag and
    // stops propagation as soon as a hovered element with a matching drop
    // listener is found — so a foreign tab dropped ON a chip is eaten by
    // THIS chip before the strip-body handler can run. The chip therefore
    // must move the foreign tab itself, which needs `ProjectPanes`.
    let drop_panes = project_panes.clone();
    let file_hover_entity = entity.clone();
    let file_drop_entity = entity.clone();
    // F4.6: separate clones for the OS-native (Finder) drop variants —
    // GPUI dispatches by TypeId so registering for both `ExternalPaths`
    // and `FilePathDragPayload` is necessary; one set of closures can't
    // serve both because each `on_drag_move`/`on_drop` is typed.
    let native_hover_entity = entity.clone();
    let native_drop_entity = entity.clone();

    let agent_dot: Option<AnyElement> = agent_status.map(|status| {
        let dot_id =
            SharedString::from(format!("pane-group-agent-status-dot-{entity_id_raw}-{ix}"));
        render_dot(dot_id, status, theme).into_any_element()
    });

    // Bell attention: a small blue dot when a terminal in this tab rang the
    // bell while unfocused. Suppressed on the active tab (you're looking at
    // it; the pane ring covers the focused case). Makes a background bell
    // visible on the strip.
    let attention_dot: Option<AnyElement> = (attention && !is_active).then(|| {
        div()
            .size(px(6.))
            .rounded_full()
            .bg(theme.status_info)
            .into_any_element()
    });

    let drag_payload = TabDragPayload {
        source_group: group_id,
        source_tab_idx: ix,
        source_visible_idx: visible_idx,
        source_pinned: is_pinned,
    };
    let preview_label = label.clone();
    let preview_icon = SharedString::from(icon_path);
    let preview_color = color_tag;
    let preview_theme = theme;

    // Optional left-edge color bar (2px) when the user has assigned a
    // color tag. Rendered as an absolutely-positioned child so it
    // overlays the chip's existing chrome without shifting layout.
    let color_bar: Option<AnyElement> = color_tag.map(|c| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(px(2.0))
            .bg(gpui::rgb(c.rgb()))
            .into_any_element()
    });

    div()
        .id(SharedString::from(format!(
            "pane-group-tab-{entity_id_raw}-{ix}"
        )))
        .group(group_name.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h_full()
        .px(px(TAB_PAD_X_PX))
        .text_size(px(11.0))
        .text_color(text_color)
        .flex_shrink_0()
        .cursor_pointer()
        .relative()
        // Active tab takes the content-canvas background so it reads as
        // connected to the surface below it; inactive tabs sit on the
        // strip background and only fill on hover.
        .when(is_active, |s| s.bg(theme.bg_base))
        .when(!is_active, |s| {
            s.hover(|s| s.text_color(theme.fg_base).bg(theme.hover_overlay))
        })
        .when_some(color_bar, |s, bar| s.child(bar))
        .when_some(active_indicator, |s, bar| s.child(bar))
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
            // Activate the chip's tab within its OWN group, then dispatch
            // an `ActivateGroupTab` so the workspace also switches active
            // group focus when the chip belongs to a non-active group.
            // Double-clicking the chip promotes a preview tab to permanent
            // (the italic single-click preview sticks instead of being
            // replaced by the next preview open).
            let entity = activate_entity.clone();
            let promote = ev.click_count >= 2;
            entity.update(cx, |this, cx| {
                this.set_active(ix, window, cx);
                if promote {
                    this.promote_tab_to_permanent(ix, cx);
                }
            });
            window.dispatch_action(
                Box::new(ActivateGroupTab {
                    group_id: group_id.0,
                    tab_idx: ix as u32,
                }),
                cx,
            );
        })
        .on_mouse_down(
            MouseButton::Right,
            move |ev: &MouseDownEvent, window, cx| {
                let pos = ev.position;
                window.dispatch_action(
                    Box::new(OpenTabContextMenuAt {
                        x: f32::from(pos.x),
                        y: f32::from(pos.y),
                        group_id: group_id.0,
                        tab_idx: ix as u32,
                    }),
                    cx,
                );
                cx.stop_propagation();
            },
        )
        .on_drag(drag_payload, move |_payload, _offset, _window, cx| {
            cx.new(|_| {
                TabDragPreview::new(
                    preview_label.clone(),
                    preview_icon.clone(),
                    preview_color,
                    preview_theme,
                )
            })
        })
        .on_drag_move::<TabDragPayload>(move |ev: &DragMoveEvent<TabDragPayload>, _window, cx| {
            // Mid-bounds split: cursor left of center → insert before
            // this chip; right of center → insert after. A foreign-source
            // drag (from another group) also previews the insertion bar
            // here so a cross-group strip drop shows where it will land;
            // the strip-level on_drop performs the actual move.
            let payload = ev.drag(cx);
            // Pinned source can't be torn out of its group (`take_tab`
            // refuses it), so the cross-group move would silently fail —
            // suppress the affordance entirely rather than letting the
            // user aim at an action that can't land.
            if payload.source_group != group_id && payload.source_pinned {
                return;
            }
            // Drop-on-self silence: when the user drags a chip across
            // its OWN bounds (same group), do NOT re-set the hover. The
            // strip-level capture pass already cleared it, so leaving it
            // cleared means the insertion bar disappears over the source
            // chip (where dropping would be a no-op layout move) instead
            // of briefly painting on the chip itself. Only applies to the
            // source chip in its own group — a foreign chip at the same
            // visible index must still preview.
            if payload.source_group == group_id && payload.source_tab_idx == ix {
                return;
            }
            // GPUI fires on_drag_move on EVERY registered listener for
            // every move event (no hitbox filter), so without this
            // bounds check every chip in the strip would update hover
            // state on every frame — last-rendered chip wins, dragging
            // the insertion bar to the rightmost chip regardless of
            // cursor. The strip-level capture pass clears hover first;
            // we re-set it only when the cursor is actually inside this
            // chip's bounds.
            let bounds = ev.bounds;
            if !bounds.contains(&ev.event.position) {
                return;
            }
            let mid_x = bounds.origin.x + bounds.size.width / 2.0;
            let side = if ev.event.position.x < mid_x {
                TabInsertSide::Before
            } else {
                TabInsertSide::After
            };
            let hover = Some(TabDragHover {
                target_visible_idx: visible_idx,
                side,
            });
            hover_entity.update(cx, |g, cx| g.set_drag_hover(hover, cx));
        })
        .on_drop::<TabDragPayload>(move |payload: &TabDragPayload, window, cx| {
            // Foreign-source drop landing ON this chip: move the tab into
            // THIS group at the previewed slot. GPUI consumes the drag on
            // the first matching hovered drop listener (this chip), so the
            // strip-body handler would never see it — the chip has to move
            // it. A pinned source can't be torn out, so it's a clean no-op.
            if payload.source_group != group_id {
                if payload.source_pinned {
                    drop_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
                    return;
                }
                let dest = drop_entity.read(cx);
                let visible_slot = dest
                    .drag_hover()
                    .map(|h| match h.side {
                        TabInsertSide::Before => h.target_visible_idx,
                        TabInsertSide::After => h.target_visible_idx + 1,
                    })
                    .unwrap_or_else(|| dest.tab_count());
                drop_entity.update(cx, |g, cx| g.set_drag_hover(None, cx));
                drop_panes.update(cx, |p, cx| {
                    p.transfer_tab_at(
                        payload.source_group,
                        payload.source_tab_idx,
                        group_id,
                        visible_slot,
                        window,
                        cx,
                    );
                });
                return;
            }
            // Same-group reorder. Resolve destination visible idx from the
            // live hover hint (set by the most-recent on_drag_move).
            // Falling back to the chip's own visible idx keeps the no-op
            // case safe.
            drop_entity.update(cx, |g, cx| {
                let Some(hover) = g.drag_hover() else {
                    g.set_drag_hover(None, cx);
                    return;
                };
                let from = g
                    .visible_position_of(payload.source_tab_idx)
                    .unwrap_or(payload.source_visible_idx);
                let raw_to = match hover.side {
                    TabInsertSide::Before => hover.target_visible_idx,
                    TabInsertSide::After => hover.target_visible_idx + 1,
                };
                // `move_tab` removes `from` first then inserts at `to`,
                // so a drop that aims for the slot AFTER `from`
                // collapses into the same position. Translate the
                // visible-slot target into the post-remove insert index.
                let to = if raw_to > from { raw_to - 1 } else { raw_to };
                let bounded = to.min(g.tabs().len().saturating_sub(1));
                g.move_tab(from, bounded);
                g.set_drag_hover(None, cx);
            });
        })
        // File-row drag passing over this chip — same insertion-bar feel
        // as tab reorder. Pure UI mirroring: bounds check, mid-x → side,
        // set hover. No source-self guard (a file row can't be the chip
        // itself) and no cross-group filter (file rows aren't bound to a
        // group).
        .on_drag_move::<FilePathDragPayload>(
            move |ev: &DragMoveEvent<FilePathDragPayload>, _window, cx| {
                let bounds = ev.bounds;
                if !bounds.contains(&ev.event.position) {
                    return;
                }
                let mid_x = bounds.origin.x + bounds.size.width / 2.0;
                let side = if ev.event.position.x < mid_x {
                    TabInsertSide::Before
                } else {
                    TabInsertSide::After
                };
                let hover = Some(TabDragHover {
                    target_visible_idx: visible_idx,
                    side,
                });
                file_hover_entity.update(cx, |g, cx| g.set_drag_hover(hover, cx));
            },
        )
        // File drop on a chip → insert (or move-and-activate) the editor
        // tab at the resolved visual slot.
        .on_drop::<FilePathDragPayload>(move |payload: &FilePathDragPayload, window, cx| {
            let path = payload.path.clone();
            file_drop_entity.update(cx, |g, cx| {
                let visible_target = g.drag_hover().map(|h| match h.side {
                    TabInsertSide::Before => h.target_visible_idx,
                    TabInsertSide::After => h.target_visible_idx + 1,
                });
                g.set_drag_hover(None, cx);
                g.open_or_activate_editor_tab_at(path, visible_target, false, window, cx);
            });
        })
        // F4.6: chip-level OS-native drop. Mirrors the FilePathDragPayload
        // hover/drop pair so the insertion bar paints the same way for
        // Finder drops. Multi-file Finder drops insert in sequence at
        // increasing visible indices so the strip reads left-to-right
        // in the order Finder selected them.
        .on_drag_move::<ExternalPaths>(move |ev: &DragMoveEvent<ExternalPaths>, _window, cx| {
            let bounds = ev.bounds;
            if !bounds.contains(&ev.event.position) {
                return;
            }
            let mid_x = bounds.origin.x + bounds.size.width / 2.0;
            let side = if ev.event.position.x < mid_x {
                TabInsertSide::Before
            } else {
                TabInsertSide::After
            };
            let hover = Some(TabDragHover {
                target_visible_idx: visible_idx,
                side,
            });
            native_hover_entity.update(cx, |g, cx| g.set_drag_hover(hover, cx));
        })
        .on_drop::<ExternalPaths>(move |payload: &ExternalPaths, window, cx| {
            let paths = filter_droppable_files(payload.paths());
            native_drop_entity.update(cx, |g, cx| {
                let base_target = g.drag_hover().map(|h| match h.side {
                    TabInsertSide::Before => h.target_visible_idx,
                    TabInsertSide::After => h.target_visible_idx + 1,
                });
                g.set_drag_hover(None, cx);
                for (offset, path) in paths.into_iter().enumerate() {
                    let target = base_target.map(|t| t + offset);
                    g.open_or_activate_editor_tab_at(path, target, false, window, cx);
                }
            });
        })
        .child(
            svg()
                .path(icon_path)
                .size(px(ICON_SIZE_PX))
                .text_color(icon_color),
        )
        .when_some(agent_dot, |s, dot| s.child(dot))
        .when_some(attention_dot, |s, dot| s.child(dot))
        .child(
            div()
                // Preview tabs read italic; an on-disk delete/rename strikes
                // the label through so the tab clearly flags the mismatch.
                .when(is_preview, |d| d.italic())
                .when(external_mutation.is_some(), |d| d.line_through())
                .child(label),
        )
        // Trailing "deleted"/"renamed" suffix for an externally-mutated file.
        .when_some(external_mutation, |s, mutation| {
            let suffix = match mutation {
                super::ExternalMutation::Deleted => "deleted",
                super::ExternalMutation::Renamed => "renamed",
            };
            s.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.fg_subtle)
                    .child(suffix),
            )
        })
        .child(if is_pinned {
            pin_indicator(entity_id_raw, ix, theme).into_any_element()
        } else {
            close_button(entity_id_raw, ix, is_active, entity, group_name, theme).into_any_element()
        })
        .when_some(drag_edge, |s, side| {
            s.child(insertion_bar(entity_id_raw, ix, side, theme))
        })
}

/// Pin-state indicator that replaces the close button on a pinned tab.
/// Same footprint as `close_button` so swapping doesn't reflow the chip;
/// non-interactive (the right-click "Unpin Tab" row is the toggle path)
/// to keep accidental clicks from un-pinning.
fn pin_indicator(entity_id_raw: u64, ix: usize, theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/pin.svg")
        .size(px(10.0))
        .text_color(theme.fg_muted);
    div()
        .id(SharedString::from(format!(
            "pane-group-tab-pin-{entity_id_raw}-{ix}"
        )))
        .w(px(CLOSE_BUTTON_SIZE_PX))
        .h(px(CLOSE_BUTTON_SIZE_PX))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .child(glyph)
}

/// 2px-wide blue insertion bar overlay. Painted on the chip's leading
/// or trailing edge when the user is dragging another chip toward that
/// slot. `top:0 / bottom:0` so it spans the full strip height; absolute
/// positioning keeps the chip's content from reflowing under it.
fn insertion_bar(
    entity_id_raw: u64,
    ix: usize,
    side: TabInsertSide,
    theme: Theme,
) -> impl IntoElement {
    let base = div()
        .id(SharedString::from(format!(
            "pane-group-tab-insertion-bar-{entity_id_raw}-{ix}"
        )))
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(2.0))
        .bg(theme.focus_ring);
    match side {
        TabInsertSide::Before => base.left_0(),
        TabInsertSide::After => base.right_0(),
    }
}

/// Trailing "..." button on the focused group's strip. Dispatches
/// `OpenPaneActionsAt` with the cursor's absolute window coordinates so
/// the shared `PaneActionsMenu` popup anchors to the click point rather
/// than the workspace top-right corner. Unfocused groups still reserve
/// the slot via a zero-width collapse so focus shifts don't reflow the
/// strip.
fn pane_actions_button(entity_id_raw: u64, is_focused: bool, theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/ellipsis.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    let (width_px, opacity) = if is_focused {
        (ELLIPSIS_BUTTON_WIDTH_PX, 1.0_f32)
    } else {
        (0.0_f32, 0.0_f32)
    };
    div()
        .id(SharedString::from(format!(
            "pane-group-actions-{entity_id_raw}"
        )))
        .w(px(width_px))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .overflow_hidden()
        .opacity(opacity)
        .cursor_pointer()
        .when(is_focused, |s| s.hover(|s| s.bg(theme.hover_overlay)))
        .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, cx| {
            let pos = ev.position;
            window.dispatch_action(
                Box::new(OpenPaneActionsAt {
                    x: f32::from(pos.x),
                    y: f32::from(pos.y),
                }),
                cx,
            );
        })
        .child(glyph)
}

/// Trailing "+" button on every group's strip.
///
/// Left-click opens the adapter picker popover — the picker offers
/// "New Terminal" (⌘T) plus one row per registered agent adapter. Direct
/// terminal spawn is reachable via the ⌘T keybind on `NewTab`; the `+`
/// button is the surface for picking a non-default kind.
///
/// The click x is passed as `Some(cursor_x)` so the popover anchors
/// under THIS group's `+` — fixes the multi-group anchor bug acknowledged
/// in `plans/reports/refactor-260524-0131-tab-manager-parity.md`.
fn plus_button(entity_id_raw: u64, theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/plus.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id(SharedString::from(format!(
            "pane-group-plus-{entity_id_raw}"
        )))
        .w(px(PLUS_BUTTON_WIDTH_PX))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, cx| {
            window.dispatch_action(
                Box::new(RequestOpenAdapterPicker {
                    x: Some(f32::from(ev.position.x)),
                }),
                cx,
            );
        })
        .child(glyph)
}

/// Render the floating MRU switcher panel — shown while the user holds
/// Ctrl after pressing Ctrl+Tab. Lists the captured snapshot of tab
/// indices with the current cursor row highlighted.
///
/// Pure render — no input handling. Activation happens on Ctrl release
/// in `PaneGroup::commit_mru_switch`; advancing happens on Ctrl+Tab key
/// dispatch in `on_mru_next`/`on_mru_prev`. The panel intentionally
/// stops mouse propagation so clicks on the body underneath don't steal
/// focus mid-cycle.
fn render_mru_hud(
    snapshot: &[usize],
    cursor: usize,
    tabs: &[PaneGroupTab],
    theme: Theme,
) -> AnyElement {
    let mut card = div()
        .flex()
        .flex_col()
        .min_w(px(320.0))
        .max_w(px(560.0))
        .p(px(8.0))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        .rounded(px(8.0))
        .shadow_lg();
    card = card.child(
        div()
            .px(px(8.0))
            .pb(px(6.0))
            .text_size(px(11.0))
            .text_color(theme.fg_subtle)
            .child("Switch Tab"),
    );
    for (row_ix, &tab_idx) in snapshot.iter().enumerate() {
        let Some(tab) = tabs.get(tab_idx) else {
            continue;
        };
        let label = tab
            .custom_title
            .clone()
            .unwrap_or_else(|| tab.label.clone());
        let icon_path = match tab.kind {
            // Diff and commit-detail tabs share the file glyph with
            // editor tabs (see marker note above).
            PaneGroupTabKind::Editor { .. }
            | PaneGroupTabKind::Diff { .. }
            | PaneGroupTabKind::Commit { .. }
            | PaneGroupTabKind::BranchFile { .. }
            | PaneGroupTabKind::CombinedDiff { .. } => "icons/file.svg",
            PaneGroupTabKind::Terminal => "icons/square-terminal.svg",
            PaneGroupTabKind::Browser { .. } => "icons/globe.svg",
            // Tasks uses the list-tree glyph for the Ctrl+Tab switcher row.
            PaneGroupTabKind::Tasks => "icons/list-tree.svg",
            // Agent Chat uses the sparkles glyph (AI convention), matching its
            // tab chip in the Ctrl+Tab switcher.
            PaneGroupTabKind::AgentChat { .. } => "icons/sparkles.svg",
            // Match the tab chip: brand the agent by its adapter glyph so
            // the Ctrl+Tab switcher reads the same as the strip.
            PaneGroupTabKind::Agent { adapter_id, .. } => agent_icon(adapter_id),
        };
        let is_highlighted = row_ix == cursor;
        let row_bg = if is_highlighted {
            theme.selection
        } else {
            gpui::transparent_black()
        };
        let row_fg = if is_highlighted {
            theme.fg_base
        } else {
            theme.fg_muted
        };
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(28.0))
            .px(px(10.0))
            .rounded(px(4.0))
            .bg(row_bg)
            .text_size(px(12.0))
            .text_color(row_fg)
            .child(svg().path(icon_path).size(px(12.0)).text_color(row_fg))
            .child(div().flex_1().child(label));
        card = card.child(row);
    }
    // Position absolute, centered horizontally + ~30% from top.
    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(120.0))
        // Block clicks reaching the body so a stray pointerdown doesn't
        // refocus another tab mid-cycle.
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _window, cx| {
            cx.stop_propagation();
        })
        .child(card)
        .into_any_element()
}

/// Thin 8-px gradient overlay painted at the strip's left or right
/// scroll edge. Acts as a visual cue that more chips exist offscreen.
/// Left fade reserves no trailing offset; right fade is inset by the
/// trailing chrome cluster width so it doesn't bleed onto the `+` /
/// `...` buttons.
///
/// The gradient is a single-color overlay with low opacity — gpui's
/// linear-gradient builder isn't trivially exposed for tiny edge fades,
/// so a solid-bg with a low alpha gives the same visual hint at a
/// fraction of the complexity.
fn scroll_fade_overlay(is_left: bool, trailing_cluster_px: f32, theme: Theme) -> impl IntoElement {
    let mut overlay = div()
        .absolute()
        .top(px(0.0))
        // -2px so the fade clears the strip's bottom border without
        // covering it (the border is the focused-group highlight).
        .bottom(px(2.0))
        .w(px(8.0))
        .bg(theme.bg_panel)
        .opacity(0.7)
        .child(div());
    if is_left {
        overlay = overlay.left(px(0.0));
    } else {
        overlay = overlay.right(px(trailing_cluster_px));
    }
    overlay
}

fn close_button(
    entity_id_raw: u64,
    ix: usize,
    is_active: bool,
    entity: Entity<PaneGroup>,
    group_name: SharedString,
    theme: Theme,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!(
            "pane-group-tab-close-{entity_id_raw}-{ix}"
        )))
        .w(px(CLOSE_BUTTON_SIZE_PX))
        .h(px(CLOSE_BUTTON_SIZE_PX))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .opacity(initial_opacity)
        .group_hover(group_name, |s| s.opacity(1.0))
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity.clone();
            entity.update(cx, |this, cx| this.request_close_tab(ix, window, cx));
            cx.stop_propagation();
        })
        .child(glyph)
}

#[cfg(test)]
mod sub_pane_resize_tests {
    use super::redistribute_sub_pane_weights;

    #[test]
    fn midpoint_keeps_two_pane_balanced() {
        let new_w = redistribute_sub_pane_weights(&[1.0, 1.0], 0, 0.5);
        assert!((new_w[0] - 1.0).abs() < 1e-3);
        assert!((new_w[1] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn three_pane_outer_pane_untouched() {
        let new_w = redistribute_sub_pane_weights(&[1.0, 1.0, 1.0], 1, 0.8);
        assert!((new_w[0] - 1.0).abs() < 1e-3, "outer pane untouched");
        let pair_sum = new_w[1] + new_w[2];
        assert!((pair_sum - 2.0).abs() < 1e-3, "pair sum preserved");
    }

    #[test]
    fn invalid_divider_idx_returns_initial() {
        let new_w = redistribute_sub_pane_weights(&[1.0, 1.0], 5, 0.5);
        assert_eq!(new_w, vec![1.0, 1.0]);
    }
}

#[cfg(test)]
mod filter_droppable_files_tests {
    use super::filter_droppable_files;
    use std::path::PathBuf;

    /// Use this crate's own Cargo.toml as a known-real file; Cargo runs
    /// tests with CWD set to the crate root.
    fn known_file() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
    }

    fn known_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn real_file_passes_through() {
        let out = filter_droppable_files(&[known_file()]);
        assert_eq!(out, vec![known_file()]);
    }

    #[test]
    fn directory_is_filtered_out() {
        let out = filter_droppable_files(&[known_dir()]);
        assert!(out.is_empty(), "directory drops must not become tabs");
    }

    #[test]
    fn missing_path_is_filtered_out() {
        let phantom = PathBuf::from("/tmp/this/path/cannot/exist/xyz123");
        let out = filter_droppable_files(&[phantom]);
        assert!(out.is_empty());
    }

    #[test]
    fn mixed_drop_keeps_only_files() {
        let inputs = vec![known_file(), known_dir(), PathBuf::from("/nope/missing")];
        let out = filter_droppable_files(&inputs);
        assert_eq!(out, vec![known_file()]);
    }
}
