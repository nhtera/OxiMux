//! Floating usage popover — a borderless panel **window** hosting the usage
//! card above the inline browser.
//!
//! Why a separate window: the inline browser's webview is a native view
//! layered over the GPU canvas, so an in-window GPUI element can't be drawn on
//! top of a visible page. A `WindowKind::PopUp` panel composites at the
//! popup window level (above every normal window's native child views), so the
//! themed card floats over the page without hiding it.
//!
//! Dismissal (the tricky part): GPUI has no cross-window "click outside" event,
//! so the panel opens focused and closes when it resigns key — i.e. the moment
//! the user clicks anything else — plus an explicit Escape. The owner debounces
//! re-open so the same click that dismisses (on the status-bar chip) doesn't
//! immediately reopen it.

use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, size,
};
use oximux_agents::session_log::now_unix_ms;
use oximux_agents::session_log::usage::ProviderUsage;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::usage_meter;
use crate::workspace_root::WorkspaceRoot;

/// Re-open is suppressed for this long after a dismissal so the chip click that
/// closes the popover (which also resigns the panel's key status) doesn't race
/// straight back into a re-open.
pub const REOPEN_DEBOUNCE_MS: i64 = 300;

/// Unix-ms of the last dismissal, written *synchronously* the instant the
/// panel resigns key (the owner's entity-stored handle is cleared a turn later
/// via `defer`, which is too late for the re-open debounce). Read by the chip
/// toggle to swallow the same click that dismissed the popover.
pub static LAST_CLOSED_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

const MARGIN: f32 = 8.0;

/// The popup window's root view: renders the shared usage card and owns the
/// dismiss triggers (resign-key + Escape).
pub struct UsagePopover {
    rows: Vec<ProviderUsage>,
    theme: Theme,
    density: Density,
    typography: Typography,
    owner: WeakEntity<WorkspaceRoot>,
    focus_handle: FocusHandle,
    /// Set once the panel has actually become key, so the initial
    /// not-yet-active tick doesn't dismiss it before it ever shows.
    seen_active: bool,
    _activation: Subscription,
}

impl UsagePopover {
    fn new(
        rows: Vec<ProviderUsage>,
        theme: Theme,
        density: Density,
        typography: Typography,
        owner: WeakEntity<WorkspaceRoot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.seen_active = true;
            } else if this.seen_active {
                this.dismiss(window, cx);
            }
        });
        Self {
            rows,
            theme,
            density,
            typography,
            owner,
            focus_handle,
            seen_active: false,
            _activation: activation,
        }
    }

    fn dismiss(&self, window: &mut Window, cx: &mut Context<Self>) {
        // Stamp the close time synchronously so the chip toggle can swallow the
        // very click that dismissed us (the entity-handle clear below is
        // deferred and would land too late for that check).
        LAST_CLOSED_MS.store(now_unix_ms(), std::sync::atomic::Ordering::SeqCst);
        window.remove_window();
        // Clear the owner's handle on a later turn, never inline: this runs
        // from the activation observer, which fires *during* the same chip
        // click whose handler is also updating `WorkspaceRoot` — a nested
        // `update` on that entity would panic. `defer` lets the current borrow
        // release first.
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |root, _| root.note_usage_popover_closed());
        });
    }
}

impl Focusable for UsagePopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for UsagePopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.dismiss(window, cx);
                }
            }))
            .child(usage_meter::render_usage_popover(
                &self.rows,
                now_unix_ms(),
                self.theme,
                self.density,
                &self.typography,
            ))
    }
}

/// Open the floating usage popover anchored to the status-bar's bottom-right
/// corner, returning its handle for the owner to track (so a second chip click
/// can dismiss it). The card data is snapshotted at open time.
pub fn open(
    rows: Vec<ProviderUsage>,
    theme: Theme,
    density: Density,
    typography: Typography,
    owner: WeakEntity<WorkspaceRoot>,
    window: &mut Window,
    cx: &mut App,
) -> Option<gpui::WindowHandle<UsagePopover>> {
    // Sized to the content: a second account roughly doubles the card, and a
    // fixed height would either clip it or leave a slab of empty panel under a
    // single one. The window has to be sized before anything renders, so the
    // card's own measurement function is the only place this can come from.
    let popover = size(
        px(usage_meter::POPOVER_WIDTH),
        px(usage_meter::popover_height(&rows, density, &typography)),
    );
    let main = window.bounds();
    // Bottom-right of the main window, just above the status bar.
    let origin = point(
        main.origin.x + main.size.width - popover.width - px(MARGIN),
        main.origin.y + main.size.height - popover.height - px(density.h_status_bar) - px(MARGIN),
    );
    let display_id = window.display(cx).map(|d| d.id());

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin,
            size: popover,
        })),
        titlebar: None,
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        ..Default::default()
    };

    cx.open_window(options, move |window, cx| {
        cx.new(|cx| UsagePopover::new(rows, theme, density, typography, owner, window, cx))
    })
    .ok()
}
