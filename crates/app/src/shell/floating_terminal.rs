//! In-window floating ("picture-in-picture") terminal: a draggable, resizable
//! terminal card that floats above the workspace panels. Distinct from the
//! OS-level tear-off (a second window) — this is one overlay inside the main
//! window, toggled with `Cmd+Shift+T`.
//!
//! The entity is held by `WorkspaceRoot` and kept alive across hides (only
//! `visible` flips) so the PTY survives toggling; the close button drops the
//! entity, which tears the PTY down via `TerminalView`'s drop. Geometry is
//! persisted (debounced) to the settings repo and restored on next launch.

use std::time::Duration;

use gpui::{
    AppContext, Context, DragMoveEvent, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled, Task, Window, div,
    point, px,
};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

use oximux_agents::SharedBackend;
use oximux_pty::TerminalSessionId;

use crate::shell::context_env::SurfaceIds;
use crate::shell::terminal_view::TerminalView;

/// Settings-repo key for the persisted geometry blob.
const GEOMETRY_KEY: &str = "floating_terminal.geometry";
/// Minimum card size so it can't be resized into uselessness.
const MIN_W: f32 = 320.0;
const MIN_H: f32 = 200.0;
/// Title-bar height (the drag handle).
const TITLE_H: f32 = 28.0;
/// Bottom-right resize handle hit area.
const RESIZE_HANDLE: f32 = 14.0;
/// Debounce before writing geometry to SQLite, so a drag/resize gesture writes
/// once on settle rather than on every move tick.
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
    view: Entity<TerminalView>,
    geom: FloatingGeom,
    /// Cursor offset within the title bar captured on mouse-down so dragging
    /// keeps the grab point under the cursor instead of snapping to a corner.
    drag_grab: Option<Point<Pixels>>,
    settings_repo: Option<SettingsRepo>,
    _persist_task: Option<Task<()>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl FloatingTerminal {
    /// Construct a floating terminal from an already-spawned PTY. The caller
    /// (`WorkspaceRoot`) spawns the PTY first so it can bail before creating
    /// the entity when no backend is available; this fn just mounts the view.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        settings_repo: Option<SettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend,
                session_id,
                ids,
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });
        Self {
            view,
            geom: FloatingGeom::load(&settings_repo),
            drag_grab: None,
            settings_repo,
            _persist_task: None,
            theme,
            density,
            typography,
        }
    }

    fn set_pos(&mut self, x: f32, y: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_x = (f32::from(vp.width) - self.geom.w).max(0.0);
        let max_y = (f32::from(vp.height) - self.geom.h).max(0.0);
        self.geom.x = x.clamp(0.0, max_x);
        self.geom.y = y.clamp(0.0, max_y);
        self.schedule_persist(cx);
        cx.notify();
    }

    fn set_size(&mut self, w: f32, h: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_w = (f32::from(vp.width) - self.geom.x).max(MIN_W);
        let max_h = (f32::from(vp.height) - self.geom.y).max(MIN_H);
        self.geom.w = w.clamp(MIN_W, max_w);
        self.geom.h = h.clamp(MIN_H, max_h);
        self.schedule_persist(cx);
        cx.notify();
    }

    /// Debounced geometry write — coalesces a gesture's move ticks into one
    /// SQLite write on settle. The previous task is dropped (cancelled) each
    /// time, so only the last tick's write survives.
    fn schedule_persist(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.settings_repo.clone() else {
            return;
        };
        let geom = self.geom;
        self._persist_task = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(PERSIST_DEBOUNCE).await;
            if let Ok(json) = serde_json::to_string(&geom)
                && let Err(err) = repo.set(GEOMETRY_KEY, &json)
            {
                tracing::warn!(?err, "failed to persist floating terminal geometry");
            }
        }));
    }
}

impl Render for FloatingTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let geom = self.geom;

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
            .child("Terminal")
            .child(
                div()
                    .id("floating-term-close")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(TITLE_H - 8.0))
                    .rounded(px(density.r_xs))
                    .text_color(theme.fg_muted)
                    .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                    .child("×")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _ev, _window, cx| {
                            cx.stop_propagation();
                            cx.emit(FloatingTerminalEvent::Close);
                        }),
                    ),
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
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.view.clone()),
            )
            .child(resize_handle)
    }
}

/// Emitted to `WorkspaceRoot` when the user clicks the close button.
pub enum FloatingTerminalEvent {
    Close,
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
}
