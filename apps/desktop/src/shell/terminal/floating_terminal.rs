//! In-window floating ("picture-in-picture") terminal: a draggable, resizable
//! terminal card that floats above the workspace panels. Distinct from the
//! OS-level tear-off (a second window) — this is one overlay inside the main
//! window, toggled with `Cmd+Shift+T`.
//!
//! The card hosts a small TAB SET: multiple PTY sessions with a compact
//! strip that appears at two-plus tabs (a single tab renders strip-less —
//! the original look). The entity is held by `WorkspaceRoot` and kept alive
//! across hides (only `visible` flips) so the PTYs survive toggling; the
//! title-bar close button drops the entity, which tears every PTY down via
//! `TerminalView` drop. Geometry AND the tab set persist to the settings
//! repo (debounced); relay-backed tabs reattach across relaunch on the
//! first toggle (see `floating_terminal_persistence`).
//!
//! Spawning stays with `WorkspaceRoot` (it owns cwd resolution + the
//! no-backend bail): the card emits `NewTabRequested` / `ExpandActive…` /
//! `RenameRequested` events instead of touching PTYs itself.

use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, DragMoveEvent, Entity, Focusable, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Point, Render,
    StatefulInteractiveElement, Styled, Task, Window, div, point, prelude::FluentBuilder, px,
};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

use oximux_agents::SharedBackend;
use oximux_pty::TerminalSessionId;

use crate::shell::context_env::SurfaceIds;
use crate::shell::floating_terminal_persistence::{FloatingTabsBlob, PersistedFloatingTab};
use crate::shell::terminal_view::TerminalView;

/// Settings-repo key for the persisted geometry blob.
const GEOMETRY_KEY: &str = "floating_terminal.geometry";
/// Minimum card size so it can't be resized into uselessness.
const MIN_W: f32 = 320.0;
const MIN_H: f32 = 200.0;
/// Title-bar height (the drag handle).
const TITLE_H: f32 = 28.0;
/// Tab-strip height (shown only at 2+ tabs).
const STRIP_H: f32 = 24.0;
/// Bottom-right resize handle hit area.
const RESIZE_HANDLE: f32 = 14.0;
/// Debounce before writing geometry/tabs to SQLite, so a gesture or a tab
/// burst writes once on settle rather than per tick.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

/// Position + size of the floating card, persisted as JSON.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloatingGeom {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for FloatingGeom {
    fn default() -> Self {
        Self { x: 120.0, y: 120.0, w: 560.0, h: 360.0 }
    }
}

impl FloatingGeom {
    fn load(repo: &Option<SettingsRepo>) -> Self {
        repo.as_ref()
            .and_then(|r| r.get(GEOMETRY_KEY).ok().flatten())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

/// Everything `WorkspaceRoot` resolves before a tab can mount: a spawned
/// (or reattached) PTY plus its identity. The card never spawns PTYs.
pub struct FloatingTabSpec {
    pub backend: SharedBackend,
    pub session_id: TerminalSessionId,
    pub ids: SurfaceIds,
    pub cwd: String,
    pub custom_title: Option<String>,
}

/// One mounted floating tab.
struct FloatingTab {
    view: Entity<TerminalView>,
    session_id: TerminalSessionId,
    cwd: String,
    custom_title: Option<String>,
    /// Relay-side PTY id captured at mount — `None` on the in-process
    /// fallback backend (then the tab respawns by cwd on restore).
    external_id: Option<String>,
}

/// Positional default title: first tab keeps the original single-session
/// label, later ones number up. Pure for tests.
pub fn default_tab_title(ordinal: usize) -> String {
    if ordinal == 0 {
        "Terminal".to_string()
    } else {
        format!("Terminal {}", ordinal + 1)
    }
}

/// Drag payloads — markers distinguishing a title-bar move from a corner
/// resize so the two `on_drag_move` handlers don't cross-fire.
struct TitleDrag;
struct ResizeDrag;

/// Zero-size drag preview (GPUI requires `on_drag` to return a render entity;
/// the card itself moves on each `on_drag_move`, so the preview is invisible).
struct DragGhost;
impl Render for DragGhost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

pub struct FloatingTerminal {
    tabs: Vec<FloatingTab>,
    active: usize,
    geom: FloatingGeom,
    /// Cursor offset within the title bar captured on mouse-down so dragging
    /// keeps the grab point under the cursor instead of snapping to a corner.
    drag_grab: Option<Point<Pixels>>,
    settings_repo: Option<SettingsRepo>,
    /// Per-window settings key for the persisted tab set.
    tabs_key: String,
    _geom_persist_task: Option<Task<()>>,
    _tabs_persist_task: Option<Task<()>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl FloatingTerminal {
    /// Mount a card from already-resolved tab specs (one fresh spawn, or a
    /// restored set). `specs` MUST be non-empty — the caller bails before
    /// constructing the entity when no PTY is available.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        specs: Vec<FloatingTabSpec>,
        active: usize,
        tabs_key: String,
        settings_repo: Option<SettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            geom: FloatingGeom::load(&settings_repo),
            drag_grab: None,
            settings_repo,
            tabs_key,
            _geom_persist_task: None,
            _tabs_persist_task: None,
            theme,
            density,
            typography,
        };
        for spec in specs {
            let tab = this.mount_tab(spec, window, cx);
            this.tabs.push(tab);
        }
        this.active = active.min(this.tabs.len().saturating_sub(1));
        this.schedule_persist_tabs(cx);
        this
    }

    fn mount_tab(
        &self,
        spec: FloatingTabSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FloatingTab {
        let view = cx.new(|cx| {
            TerminalView::mount(
                spec.backend,
                spec.session_id,
                spec.ids,
                self.theme,
                self.density,
                self.typography.clone(),
                window,
                cx,
            )
        });
        FloatingTab {
            view,
            session_id: spec.session_id,
            cwd: spec.cwd,
            custom_title: spec.custom_title,
            external_id: crate::shell::terminal_view::external_id_for_session(spec.session_id),
        }
    }

    /// Append a new tab (the `+` button / Cmd+T path — the root spawned the
    /// PTY) and make it active.
    pub fn push_tab(&mut self, spec: FloatingTabSpec, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.mount_tab(spec, window, cx);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.schedule_persist_tabs(cx);
        cx.notify();
    }

    /// Close one tab (chip `×` / Cmd+W). Dropping the view tears its PTY
    /// down. Closing the last tab emits `Close` so the root drops the card
    /// — same end state as the title-bar close.
    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            // The entity drops right after `Close`, which would cancel a
            // debounced write — persist the (now empty) set immediately so
            // the next toggle doesn't resurrect a deliberately-closed tab.
            self.persist_tabs_now();
            cx.emit(FloatingTerminalEvent::Close);
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        self.schedule_persist_tabs(cx);
        cx.notify();
    }

    /// Activate a tab and focus its terminal.
    pub fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() || idx == self.active {
            return;
        }
        self.active = idx;
        self.focus_active(window, cx);
        self.schedule_persist_tabs(cx);
        cx.notify();
    }

    /// Cycle the active tab (NextTab / PrevTab within the card).
    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len() as isize;
        if n < 2 {
            return;
        }
        let next = (self.active as isize + delta).rem_euclid(n) as usize;
        self.activate_tab(next, window, cx);
    }

    /// Set (or reset, with `None`) a tab's custom title.
    pub fn set_tab_title(&mut self, idx: usize, title: Option<String>, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.custom_title = title.filter(|t| !t.trim().is_empty());
            self.schedule_persist_tabs(cx);
            cx.notify();
        }
    }

    /// Display title for a tab — custom when set, positional default else.
    pub fn tab_title(&self, idx: usize) -> String {
        self.tabs
            .get(idx)
            .map(|t| {
                t.custom_title
                    .clone()
                    .unwrap_or_else(|| default_tab_title(idx))
            })
            .unwrap_or_default()
    }

    /// Detach the active tab for expand-to-pane: the view entity (PTY
    /// intact) plus its display title + session id. Emits `Close` when the
    /// card empties — the root drops it after re-homing the view.
    pub fn take_active_tab(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(String, Entity<TerminalView>, TerminalSessionId)> {
        if self.tabs.is_empty() {
            return None;
        }
        let title = self.tab_title(self.active);
        let tab = self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            // Immediate write — `Close` drops the entity and would cancel
            // a debounced one. A stale blob here is worse than elsewhere:
            // the expanded tab's PTY now lives in a pane, and a later
            // restore attempt against its external id would fight the
            // pane's single-subscriber attach.
            self.persist_tabs_now();
            cx.emit(FloatingTerminalEvent::Close);
        } else {
            self.schedule_persist_tabs(cx);
            cx.notify();
        }
        Some((title, tab.view, tab.session_id))
    }

    /// Number of open tabs (root-side guards + tests).
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            tab.view.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    fn set_pos(&mut self, x: f32, y: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_x = (f32::from(vp.width) - self.geom.w).max(0.0);
        let max_y = (f32::from(vp.height) - self.geom.h).max(0.0);
        self.geom.x = x.clamp(0.0, max_x);
        self.geom.y = y.clamp(0.0, max_y);
        self.schedule_persist_geom(cx);
        cx.notify();
    }

    fn set_size(&mut self, w: f32, h: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_w = (f32::from(vp.width) - self.geom.x).max(MIN_W);
        let max_h = (f32::from(vp.height) - self.geom.y).max(MIN_H);
        self.geom.w = w.clamp(MIN_W, max_w);
        self.geom.h = h.clamp(MIN_H, max_h);
        self.schedule_persist_geom(cx);
        cx.notify();
    }

    /// Debounced geometry write — coalesces a gesture's move ticks into one
    /// SQLite write on settle. The previous task is dropped (cancelled) each
    /// time, so only the last tick's write survives.
    fn schedule_persist_geom(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.settings_repo.clone() else {
            return;
        };
        let geom = self.geom;
        self._geom_persist_task = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            if let Ok(json) = serde_json::to_string(&geom)
                && let Err(err) = repo.set(GEOMETRY_KEY, &json)
            {
                tracing::warn!(?err, "failed to persist floating terminal geometry");
            }
        }));
    }

    /// Synchronous tab-set write for paths where the entity is about to
    /// drop (last tab closed / expanded out, title-bar close) — a debounced
    /// task would be cancelled by the drop and leave a stale blob.
    fn persist_tabs_now(&self) {
        if let Some(repo) = &self.settings_repo {
            self.persisted_blob().save(repo, &self.tabs_key);
        }
    }

    /// Debounced tab-set write — same coalescing contract as geometry.
    fn schedule_persist_tabs(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.settings_repo.clone() else {
            return;
        };
        let key = self.tabs_key.clone();
        let blob = self.persisted_blob();
        self._tabs_persist_task = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            blob.save(&repo, &key);
        }));
    }

    /// Snapshot the live tab set into its persisted form.
    fn persisted_blob(&self) -> FloatingTabsBlob {
        FloatingTabsBlob {
            relay_session: crate::shell::terminal_view::relay_session_id_cached(),
            active: self.active,
            tabs: self
                .tabs
                .iter()
                .map(|t| PersistedFloatingTab {
                    custom_title: t.custom_title.clone(),
                    cwd: t.cwd.clone(),
                    external_id: t.external_id.clone(),
                })
                .collect(),
        }
    }

    /// Compact tab strip — rendered only at 2+ tabs so the single-tab card
    /// keeps the original strip-less look.
    fn render_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let active = self.active;
        let chips = (0..self.tabs.len())
            .map(|i| {
                let is_active = i == active;
                let title = self.tab_title(i);
                div()
                    .id(("floating-tab", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .h(px(STRIP_H - 4.0))
                    .px(px(density.gap_inline * 2.0))
                    .rounded(px(density.r_xs))
                    .text_size(px(typography.t_sub_label))
                    .text_color(if is_active { theme.fg_base } else { theme.fg_subtle })
                    .when(is_active, |s| s.bg(theme.bg_panel))
                    .when(!is_active, |s| s.hover(|s| s.bg(theme.hover_overlay)))
                    .cursor_pointer()
                    // Single click activates; double click renames (the
                    // root hosts the dialog).
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        if ev.click_count() >= 2 {
                            cx.emit(FloatingTerminalEvent::RenameRequested { idx: i });
                        } else {
                            this.activate_tab(i, window, cx);
                        }
                    }))
                    .child(
                        div()
                            .max_w(px(140.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .child(
                        div()
                            .id(("floating-tab-close", i))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(14.0))
                            .rounded(px(density.r_xs))
                            .text_color(theme.fg_subtle)
                            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _ev, window, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(i, window, cx);
                                }),
                            ),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .h(px(STRIP_H))
            .px(px(density.gap_inline))
            .bg(self.theme.bg_panel_alt)
            .border_b_1()
            .border_color(theme.border_inactive)
            .children(chips)
    }

    /// Small square title-bar button (`+` / `⤢` / `×`).
    fn title_button(
        &self,
        id: &'static str,
        glyph: &'static str,
        on_down: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(TITLE_H - 8.0))
            .rounded(px(density.r_xs))
            .text_color(theme.fg_muted)
            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
            .child(glyph)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, window, cx| {
                    cx.stop_propagation();
                    on_down(this, window, cx);
                }),
            )
    }
}

impl Render for FloatingTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let geom = self.geom;
        let show_strip = self.tabs.len() >= 2;
        let active_view = self.tabs.get(self.active).map(|t| t.view.clone());
        let title = self.tab_title(self.active);

        let title_bar = div()
            .id("floating-term-title")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(TITLE_H))
            .px(px(density.pad_panel))
            .bg(theme.bg_panel_alt)
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .cursor_grab()
            .child(title)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .child(self.title_button(
                        "floating-term-new-tab",
                        "+",
                        |_this, _window, cx| cx.emit(FloatingTerminalEvent::NewTabRequested),
                        cx,
                    ))
                    .child(self.title_button(
                        "floating-term-expand",
                        "⤢",
                        |_this, _window, cx| {
                            cx.emit(FloatingTerminalEvent::ExpandActiveRequested)
                        },
                        cx,
                    ))
                    .child(self.title_button(
                        "floating-term-close",
                        "×",
                        |this, _window, cx| {
                            // Closing the whole card is a deliberate
                            // teardown: clear the persisted set NOW (not
                            // debounced — the entity is about to drop) so
                            // the next toggle starts fresh.
                            this.tabs.clear();
                            this.persist_tabs_now();
                            cx.emit(FloatingTerminalEvent::Close);
                        },
                        cx,
                    )),
            )
            // Capture the grab offset, then begin the drag.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    this.drag_grab =
                        Some(ev.position - point(px(this.geom.x), px(this.geom.y)));
                }),
            )
            .on_drag(TitleDrag, |_payload, _offset, _window, cx| cx.new(|_| DragGhost));

        let resize_handle = div()
            .id("floating-term-resize")
            .absolute()
            .right(px(0.0))
            .bottom(px(0.0))
            .size(px(RESIZE_HANDLE))
            .cursor_nwse_resize()
            .on_drag(ResizeDrag, |_payload, _offset, _window, cx| {
                cx.new(|_| DragGhost)
            });

        div()
            .id("floating-term-card")
            .absolute()
            // Block the hit-test from walking through to the panels behind the
            // card — without this, clicks on the card's chrome (corners, border)
            // fall through and can activate the panel underneath.
            .occlude()
            .left(px(geom.x))
            .top(px(geom.y))
            .w(px(geom.w))
            .h(px(geom.h))
            .flex()
            .flex_col()
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .overflow_hidden()
            .shadow_lg()
            // Tab actions scoped to the card: these fire only while focus is
            // inside the floating terminal (action dispatch walks the focus
            // path), so the same chords keep their pane meaning elsewhere.
            .on_action(cx.listener(|_this, _: &crate::actions::NewTab, _window, cx| {
                cx.emit(FloatingTerminalEvent::NewTabRequested);
            }))
            .on_action(cx.listener(|this, _: &crate::actions::CloseTab, window, cx| {
                this.close_tab(this.active, window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::actions::NextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::actions::PrevTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            // Move: fired while the title-bar drag is active.
            .on_drag_move::<TitleDrag>(cx.listener(
                |this, ev: &DragMoveEvent<TitleDrag>, window, cx| {
                    if let Some(grab) = this.drag_grab {
                        let p = ev.event.position;
                        this.set_pos(f32::from(p.x - grab.x), f32::from(p.y - grab.y), window, cx);
                    }
                },
            ))
            // Resize: the bottom-right corner follows the cursor.
            .on_drag_move::<ResizeDrag>(cx.listener(
                |this, ev: &DragMoveEvent<ResizeDrag>, window, cx| {
                    let p = ev.event.position;
                    this.set_size(
                        f32::from(p.x) - this.geom.x,
                        f32::from(p.y) - this.geom.y,
                        window,
                        cx,
                    );
                },
            ))
            // Release the title-bar grab offset when the drag ends so a stale
            // value can never leak into a later gesture.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, _cx| {
                    this.drag_grab = None;
                }),
            )
            .child(title_bar)
            .when(show_strip, |card| card.child(self.render_strip(cx)))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .children(active_view),
            )
            .child(resize_handle)
    }
}

/// Emitted to `WorkspaceRoot`. The card stays PTY-free: anything that needs
/// a spawn, a pane group, or a dialog goes up as an event.
pub enum FloatingTerminalEvent {
    /// Title-bar close, or the last tab closed — drop the entity.
    Close,
    /// `+` button / Cmd+T while floating-focused: spawn a PTY at the active
    /// workspace cwd and `push_tab` it.
    NewTabRequested,
    /// `⤢` button: move the active tab's view into the active pane group.
    ExpandActiveRequested,
    /// Double-click on a tab chip: open the rename dialog for `idx`.
    RenameRequested { idx: usize },
}

impl gpui::EventEmitter<FloatingTerminalEvent> for FloatingTerminal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_geom_is_visible_and_above_min() {
        let g = FloatingGeom::default();
        assert!(g.x >= 0.0 && g.y >= 0.0);
        assert!(g.w >= MIN_W && g.h >= MIN_H);
    }

    #[test]
    fn geom_round_trips_through_json() {
        let g = FloatingGeom { x: 10.0, y: 20.0, w: 640.0, h: 480.0 };
        let json = serde_json::to_string(&g).unwrap();
        let back: FloatingGeom = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn load_falls_back_to_default_on_no_repo() {
        assert_eq!(FloatingGeom::load(&None), FloatingGeom::default());
    }

    #[test]
    fn load_reads_persisted_geometry_from_repo() {
        let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
        let g = FloatingGeom { x: 5.0, y: 6.0, w: 700.0, h: 500.0 };
        repo.set(GEOMETRY_KEY, &serde_json::to_string(&g).unwrap())
            .unwrap();
        assert_eq!(FloatingGeom::load(&Some(repo)), g);
    }

    #[test]
    fn load_falls_back_to_default_on_garbage_json_in_repo() {
        // A corrupt value on disk must not panic — load swallows the parse
        // error and returns the default.
        let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
        repo.set(GEOMETRY_KEY, "not valid json").unwrap();
        assert_eq!(FloatingGeom::load(&Some(repo)), FloatingGeom::default());
    }

    #[test]
    fn default_tab_titles_are_positional() {
        assert_eq!(default_tab_title(0), "Terminal");
        assert_eq!(default_tab_title(1), "Terminal 2");
        assert_eq!(default_tab_title(4), "Terminal 5");
    }
}
