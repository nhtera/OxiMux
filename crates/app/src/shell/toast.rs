//! Quiet transient toasts — a thin bottom-right stack for cross-surface events
//! that have no permanent home (agent finished, commit failed, PR opened,
//! clipboard ops). The status bar carries persistent repo/agent state; toasts
//! carry the fleeting "this just happened" beat that would otherwise be silent.
//!
//! Design contract: `bg_overlay` card, 1px `border_active`, a 2px left accent
//! bar in the status hue, NO shadow / gradient. Auto-dismiss after a few
//! seconds; oldest trims when the stack overflows. The layer paints as a
//! pass-through overlay (no backdrop) so it never blocks clicks beneath it.

use std::time::Duration;

use gpui::{
    App, Context, Global, Hsla, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
    div, px,
};
use oximux_settings::{Density, Theme, Typography};

/// How long a toast stays before it auto-dismisses.
const TOAST_TTL: Duration = Duration::from_secs(4);
/// Cap the visible stack; an older toast is dropped when a new one overflows.
const MAX_VISIBLE: usize = 4;

/// Severity of a toast — drives only the left accent hue. Text stays neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    /// Map to the single status-accent hue for this severity.
    fn accent(self, theme: &Theme) -> Hsla {
        match self {
            ToastKind::Info => theme.status_info,
            ToastKind::Success => theme.status_ok,
            ToastKind::Error => theme.status_error,
        }
    }
}

/// One queued toast. `id` is monotonic so the dismiss timer can target the
/// exact toast even after the stack has shifted under trimming.
struct Toast {
    id: u64,
    kind: ToastKind,
    text: String,
}

/// Bottom-right transient toast stack. Owned at the workspace root and mounted
/// as a high-z overlay child. Tokens are pushed down each root render via
/// [`ToastLayer::set_tokens`] (same doctrine as the other rail/pane surfaces).
pub struct ToastLayer {
    theme: Theme,
    density: Density,
    typography: Typography,
    toasts: Vec<Toast>,
    next_id: u64,
}

impl ToastLayer {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            theme,
            density,
            typography,
            toasts: Vec::new(),
            next_id: 0,
        }
    }

    /// Refresh the design tokens from the workspace root each render. Cheap
    /// store-only; no notify (the next paint already carries it).
    pub fn set_tokens(&mut self, theme: Theme, density: Density, typography: Typography) {
        self.theme = theme;
        self.density = density;
        self.typography = typography;
    }

    /// Enqueue a toast and arm its auto-dismiss timer. Trims the oldest when
    /// the stack exceeds [`MAX_VISIBLE`] so a burst can't grow without bound.
    pub fn push(&mut self, kind: ToastKind, text: impl Into<String>, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            text: text.into(),
        });
        if self.toasts.len() > MAX_VISIBLE {
            self.toasts.remove(0);
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_TTL).await;
            let _ = this.update(cx, |layer, cx| layer.dismiss(id, cx));
        })
        .detach();
    }

    /// Remove a toast by id (timer fire or, later, manual dismiss). No-op if it
    /// was already trimmed.
    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.id != id);
        if self.toasts.len() != before {
            cx.notify();
        }
    }

    fn render_card(&self, toast: &Toast) -> impl IntoElement {
        let accent = toast.kind.accent(&self.theme);
        div()
            .flex()
            .items_stretch()
            .max_w(px(360.0))
            .bg(self.theme.bg_overlay)
            .border_1()
            .border_color(self.theme.border_active)
            .rounded(px(self.density.r_card))
            .overflow_hidden()
            // 2px status-hue left accent bar — the only color on the card.
            .child(div().w(px(2.0)).bg(accent))
            .child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(px(self.typography.t_body_sm))
                    .text_color(self.theme.fg_base)
                    .child(toast.text.clone()),
            )
    }
}

impl Render for ToastLayer {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing queued → render an inert empty node (no overlay, no hit area).
        if self.toasts.is_empty() {
            return div();
        }
        let cards: Vec<_> = self.toasts.iter().map(|t| self.render_card(t)).collect();
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .justify_end()
            .items_end()
            .pr(px(16.0))
            // Clear the 24px status bar plus a small gap.
            .pb(px(self.density.h_status_bar + 12.0))
            .gap(px(8.0))
            .children(cards)
    }
}

// ---------------------------------------------------------------------------
// Window-keyed toast bus
// ---------------------------------------------------------------------------
//
// Toasts are window-local UI, but the events that raise them (commit failed,
// PR opened, agent finished, clipboard ops) fire from entities deep in the
// tree that hold no workspace-root handle. Rather than plumb a weak root into
// every one, we keep an app-global pointer to the *active* window's toast
// layer, refreshed whenever a window gains focus. Any code with an `App` can
// then call [`toast`] and it lands in the window the user is looking at.
//
// Known multi-window gap (cosmetic, self-healing): if the *active* window is
// closed while another stays frontmost, macOS delivers no activation event to
// the survivor, so the bus can briefly point at the dropped layer. `toast`
// no-ops on a dead `WeakEntity` (toast is silently dropped, never panics), and
// the next window activation re-points the bus. Not worth a window-close hook
// for a transient-only surface.

#[derive(Default)]
struct ToastBus {
    active: Option<WeakEntity<ToastLayer>>,
}

impl Global for ToastBus {}

/// Point the bus at `layer` as the active window's toast surface. Called on
/// window activation and at first mount.
pub fn set_active_toast_layer(cx: &mut App, layer: WeakEntity<ToastLayer>) {
    if !cx.has_global::<ToastBus>() {
        cx.set_global(ToastBus::default());
    }
    cx.global_mut::<ToastBus>().active = Some(layer);
}

/// Surface a toast on the active window's layer. No-op when no window has
/// registered yet or the registered layer has been dropped (window closed).
pub fn toast(cx: &mut App, kind: ToastKind, text: impl Into<String>) {
    let Some(layer) = cx.try_global::<ToastBus>().and_then(|b| b.active.clone()) else {
        return;
    };
    let text = text.into();
    let _ = layer.update(cx, |layer, cx| layer.push(kind, text, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_maps_to_status_hues() {
        let t = Theme::charcoal();
        assert_eq!(ToastKind::Info.accent(&t), t.status_info);
        assert_eq!(ToastKind::Success.accent(&t), t.status_ok);
        assert_eq!(ToastKind::Error.accent(&t), t.status_error);
    }
}
