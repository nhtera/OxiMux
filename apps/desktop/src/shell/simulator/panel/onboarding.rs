//! First-run help in the panel: example prompts to hand an agent (each with a
//! Copy button), shown while no device is attached, and a one-time intro
//! callout over the first streaming device. "Got it" records `intro_seen` in
//! `simulator.toml`; it is not security state, so a lost value only shows
//! the callout again.

use gpui::{AnyElement, ClipboardItem, Context, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use super::{PanelEvent, SimulatorPanel, settings};
use crate::app_settings::simulator_settings;
use crate::shell::simulator::hub::NoticeKind;

/// What to ask an agent for, copied exactly. Maestro is named only as the
/// export format.
pub(crate) const EXAMPLE_PROMPTS: [&str; 3] = [
    "Build and run the app on the simulator, then sign in as test@example.com and tell me what breaks",
    "Turn on dark mode and check every screen for contrast issues",
    "Record the checkout flow and export it as a Maestro test",
];

/// The intro callout's points, in order.
const INTRO: [&str; 3] = [
    "Agents ask before they control a device.",
    "Hold ⌥ over the screen to inspect an element.",
    "The phone button in the toolbar sets dark mode, location and recording.",
];

impl SimulatorPanel {
    /// The example prompts, each with its Copy button.
    pub(super) fn render_examples(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let mut list = div().flex().flex_col().w_full().gap(px(density.gap_inline)).child(
            div().text_size(px(ty.t_body_sm)).font_weight(ty.w_semibold).text_color(theme.fg_muted).child("Try asking your agent"),
        );
        for (i, prompt) in EXAMPLE_PROMPTS.iter().enumerate() {
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .w_full()
                    .gap(px(density.gap_inline))
                    .child(div().flex_1().min_w(px(0.)).text_size(px(ty.t_body_sm)).text_color(theme.fg_base).child(*prompt))
                    .child(
                        Button::new(("sim-example-copy", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::default().path("icons/copy.svg"))
                            .tooltip("Copy prompt")
                            .on_click(cx.listener(move |_, _, _window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(prompt.to_string()));
                                cx.emit(PanelEvent::Notice(NoticeKind::Success, "Prompt copied".into()));
                            })),
                    ),
            );
        }
        list.into_any_element()
    }

    /// The intro callout, until the user dismisses it once.
    pub(super) fn render_intro(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if settings(cx).intro_seen {
            return None;
        }
        let (theme, density, ty) = (self.theme, self.density, self.typography.clone());
        let bullets = INTRO.iter().map(|point| {
            div()
                .flex()
                .flex_row()
                .gap(px(density.gap_inline))
                .text_size(px(ty.t_body_sm))
                .text_color(theme.fg_base)
                .child("•")
                .child(div().flex_1().min_w(px(0.)).child(*point))
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(density.gap_inline))
                .w_full()
                .max_w(px(360.))
                .flex_none()
                .p(px(density.pad_panel * 1.5))
                .rounded(px(density.r_card))
                .border_1()
                .border_color(theme.border_active)
                .bg(theme.bg_panel_alt)
                .child(
                    div()
                        .text_size(px(ty.t_body_md))
                        .font_weight(ty.w_semibold)
                        .text_color(theme.fg_base)
                        .child("Your agents can use this device"),
                )
                .children(bullets)
                .child(div().h(px(density.gap_inline)))
                .child(self.render_examples(cx))
                .child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("sim-intro-ok")
                            .primary()
                            .small()
                            .label("Got it")
                            .on_click(cx.listener(|_, _, _window, cx| dismiss_intro(cx))),
                    ),
                )
                .into_any_element(),
        )
    }
}

/// "Got it": saved, and installed so every window drops the callout.
fn dismiss_intro(cx: &mut Context<SimulatorPanel>) {
    let mut next = settings(cx);
    next.intro_seen = true;
    if let Err(err) = simulator_settings::save(&next) {
        tracing::warn!(%err, "simulator settings not saved");
    }
    cx.set_global(next);
    cx.refresh_windows();
}
