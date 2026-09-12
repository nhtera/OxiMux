//! Quiet transient toasts — a thin bottom-right stack for cross-surface events
//! that have no permanent home (agent finished, commit failed, PR opened,
//! clipboard ops). The status bar carries persistent repo/agent state; toasts
//! carry the fleeting "this just happened" beat that would otherwise be silent.
//!
//! Design contract: `bg_overlay` card, 1px `border_active`, a 2px left accent
//! bar in the status hue, NO shadow / gradient. Auto-dismiss after a few
//! seconds; oldest trims when the stack overflows. The layer paints as a
//! pass-through overlay (no backdrop) so it never blocks clicks beneath it.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, Context, ElementId, Global, Hsla,
    IntoElement, ParentElement, Render, SharedString, Styled, WeakEntity, Window, div,
    ease_out_quint, px,
};
use gpui::prelude::FluentBuilder;
use gpui_component::button::{Button, ButtonVariants};
use oximux_settings::{Density, Motion, Theme, Typography};

use crate::ui::FloatingSurface;

/// How long a toast stays before it auto-dismisses.
const TOAST_TTL: Duration = Duration::from_secs(4);
/// How long a toast carrying buttons stays. Longer, because it is asking a
/// question rather than reporting a fact, and the answer on timeout is
/// always the do-nothing one — so a slow reader loses nothing but the offer.
const ACTIONABLE_TOAST_TTL: Duration = Duration::from_secs(20);

/// A button on a toast. Clicking runs `on_click` and dismisses the toast;
/// letting the toast time out is the same as never clicking.
#[derive(Clone)]
pub struct ToastAction {
    pub label: SharedString,
    pub on_click: Rc<dyn Fn(&mut App)>,
}

impl ToastAction {
    /// `on_click` runs inside the toast layer's own update, so it must not
    /// call [`toast`] / [`toast_with_actions`] synchronously — that re-enters
    /// the layer and panics. Defer through `cx.spawn` (or an entity update on
    /// something else) and toast from there.
    pub fn new(label: impl Into<SharedString>, on_click: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label: label.into(),
            on_click: Rc::new(on_click),
        }
    }

    /// A button whose only effect is dismissing the toast — the explicit
    /// "no" beside an offer, so declining does not mean waiting.
    pub fn dismiss(label: impl Into<SharedString>) -> Self {
        Self::new(label, |_| {})
    }
}
/// Cap the visible stack; an older toast is dropped when a new one overflows.
const MAX_VISIBLE: usize = 4;

/// Severity of a toast — drives only the left accent hue. Text stays neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    /// A hazard the user has to act on, but not a failure. A conflicted merge
    /// is the case: nothing went wrong, and the work is still waiting.
    Warning,
    Error,
}

impl ToastKind {
    /// Map to the single status-accent hue for this severity.
    fn accent(self, theme: &Theme) -> Hsla {
        match self {
            ToastKind::Info => theme.status_info,
            ToastKind::Success => theme.status_ok,
            ToastKind::Warning => theme.status_warn,
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
    /// True once the dismiss has begun: the card plays its fade-out and a
    /// short timer removes it after `m_toast_out`. Lets the exit animate
    /// instead of the card vanishing the instant its TTL fires.
    exiting: bool,
    /// Buttons under the text. Empty for the ordinary "this just happened"
    /// toast; an offer (rename this worktree?) carries one or two.
    actions: Vec<ToastAction>,
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
        self.push_with_actions(kind, text, Vec::new(), cx);
    }

    /// Enqueue a toast with buttons. Stays longer than a plain toast (see
    /// [`ACTIONABLE_TOAST_TTL`]); a click runs the action and dismisses.
    pub fn push_with_actions(
        &mut self,
        kind: ToastKind,
        text: impl Into<String>,
        actions: Vec<ToastAction>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let ttl = if actions.is_empty() {
            TOAST_TTL
        } else {
            ACTIONABLE_TOAST_TTL
        };
        self.toasts.push(Toast {
            id,
            kind,
            text: text.into(),
            exiting: false,
            actions,
        });
        if self.toasts.len() > MAX_VISIBLE {
            // Trim the oldest PLAIN toast first: a report can be lost to a
            // burst, an offer with buttons should not be — its question may
            // be unrepeatable. Only when every card is an offer does the
            // oldest one go.
            let victim = self
                .toasts
                .iter()
                .position(|t| t.actions.is_empty())
                .unwrap_or(0);
            self.toasts.remove(victim);
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ttl).await;
            let _ = this.update(cx, |layer, cx| layer.dismiss(id, cx));
        })
        .detach();
    }

    /// Begin dismissing a toast (TTL fire or manual). Two-phase so the exit
    /// can animate: mark it `exiting` (which swaps the card to its fade-out)
    /// and arm a short timer that actually removes it after `m_toast_out`.
    /// Idempotent — a second dismiss on an already-exiting toast is ignored so
    /// the remove timer isn't double-armed.
    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if toast.exiting {
            return;
        }
        toast.exiting = true;
        cx.notify();

        let out = crate::motion_settings::active(cx).m_toast_out;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(out).await;
            let _ = this.update(cx, |layer, cx| layer.remove(id, cx));
        })
        .detach();
    }

    /// Drop a toast from the stack after its exit animation has played. No-op
    /// if it was already trimmed.
    fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.id != id);
        if self.toasts.len() != before {
            cx.notify();
        }
    }

    // Returns an owned `AnyElement` (not `impl IntoElement`) so the element
    // is not inferred to borrow `cx` — every card is built in one loop that
    // reuses the same context.
    fn render_card(&self, toast: &Toast, motion: Motion, cx: &mut Context<Self>) -> AnyElement {
        let accent = toast.kind.accent(&self.theme);
        let toast_id = toast.id;
        // Buttons, when the toast is an offer. The first is the affirmative
        // and paints primary; the rest are ghosts. Every click dismisses.
        let buttons: Vec<_> = toast
            .actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let on_click = action.on_click.clone();
                let button = Button::new(ElementId::Name(
                    format!("toast-{toast_id}-action-{i}").into(),
                ))
                .label(action.label.clone())
                .on_click(cx.listener(move |layer, _: &ClickEvent, _window, cx| {
                    (on_click)(cx);
                    layer.dismiss(toast_id, cx);
                }));
                if i == 0 { button.primary() } else { button.ghost() }
            })
            .collect();
        // `flex_1` + `min_w_0`: without them a long single-line text makes
        // this flex item report its unwrapped width, the card blows past its
        // `max_w`, and an offer's buttons paint off the right edge of the
        // window (seen live on the first auto-rename offer). With them the
        // text wraps inside the 360px card and the buttons stay visible.
        let body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .text_size(px(self.typography.t_body_sm))
            .text_color(self.theme.fg_base)
            .child(toast.text.clone())
            .when(!buttons.is_empty(), |b| {
                b.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(self.density.gap_inline))
                        .children(buttons),
                )
            });
        let card = div()
            .flex()
            .items_stretch()
            .max_w(px(360.0))
            .floating_chrome(&self.theme, &self.density)
            .overflow_hidden()
            // 2px status-hue left accent bar — the only color on the card.
            .child(div().w(px(2.0)).bg(accent))
            .child(body);
        // Enter: fade + rise 8px to rest. Exit: fade out in place. Keyed on a
        // phase-specific id so the enter→exit transition starts the fade-out
        // fresh (from full opacity) rather than continuing the enter curve.
        // Reduced motion collapses both durations to ~instant.
        let exiting = toast.exiting;
        let (anim_id, dur) = if exiting {
            (format!("toast-exit-{}", toast.id), motion.m_toast_out)
        } else {
            (format!("toast-enter-{}", toast.id), motion.m_toast_in)
        };
        // Enter rides the spring-tail open curve; exit keeps the plain
        // quint fade so dismissal reads crisp, not springy.
        let anim = if exiting {
            Animation::new(dur).with_easing(ease_out_quint())
        } else {
            Animation::new(dur).with_easing(oximux_settings::ease_out_spring())
        };
        card.with_animation(
            ElementId::Name(anim_id.into()),
            anim,
            move |el, delta| {
                if exiting {
                    el.opacity(1.0 - delta)
                } else {
                    el.opacity(delta).mt(px(8.0 * (1.0 - delta)))
                }
            },
        )
        .into_any_element()
    }
}

impl Render for ToastLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // Nothing queued → render an inert empty node (no overlay, no hit area).
        if self.toasts.is_empty() {
            return div();
        }
        let motion = crate::motion_settings::active(cx);
        let mut cards = Vec::with_capacity(self.toasts.len());
        for toast in &self.toasts {
            cards.push(self.render_card(toast, motion, cx));
        }
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

/// Surface an offer on the active window's layer: a toast with buttons. The
/// first action is the affirmative. Timing out is the same as declining.
pub fn toast_with_actions(
    cx: &mut App,
    kind: ToastKind,
    text: impl Into<String>,
    actions: Vec<ToastAction>,
) {
    let Some(layer) = cx.try_global::<ToastBus>().and_then(|b| b.active.clone()) else {
        return;
    };
    let text = text.into();
    let _ = layer.update(cx, |layer, cx| layer.push_with_actions(kind, text, actions, cx));
}

/// Standard error toast for a failed user-initiated operation:
/// "«op» failed: «first line of err»". Only the first line shows — git
/// and storage errors are often multi-line; full detail belongs in the
/// call site's tracing event, not the card.
pub fn toast_op_error(cx: &mut App, op: &str, err: &str) {
    toast(cx, ToastKind::Error, op_error_text(op, err));
}

/// Pure formatting half of [`toast_op_error`] — split for testability.
fn op_error_text(op: &str, err: &str) -> String {
    let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim();
    format!("{op} failed: {first}")
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

    #[test]
    fn op_error_text_takes_first_nonempty_line() {
        assert_eq!(
            op_error_text("Delete workspace", "fatal: 'x' is dirty\nhint: use --force"),
            "Delete workspace failed: fatal: 'x' is dirty"
        );
        // Leading blank lines are skipped, not shown as an empty reason.
        assert_eq!(
            op_error_text("Stash apply", "\n  conflict in a.rs  \nmore"),
            "Stash apply failed: conflict in a.rs"
        );
        assert_eq!(
            op_error_text("Rename", ""),
            "Rename failed: unknown error"
        );
    }
}
