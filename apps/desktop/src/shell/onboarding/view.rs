//! Onboarding wizard render: occluding backdrop + centered card + step
//! bodies + footer nav. The backdrop is deliberately inert (no click-outside
//! dismiss); Esc maps to Skip and Enter to Continue/Finish.

use gpui::{
    Animation, AnimationExt as _, Context, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use std::time::Instant;

use super::{OnboardingStep, OnboardingWizard};

/// Card width — matches the approved mockup (660px content column).
const CARD_WIDTH: f32 = 660.0;
/// Card max height; the body scrolls beyond this.
const CARD_MAX_HEIGHT: f32 = 600.0;

impl Render for OnboardingWizard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        // Keep the model dropdown's items/selection in step with the selected
        // agent (signature-guarded no-op on most paints; needs the Window).
        self.sync_model_select(window, cx);
        // The wizard is modal: while open it must own keyboard dispatch. Late
        // focus grabs elsewhere (e.g. a deferred focus-active-tab) would leave
        // Esc/Enter dead, so re-assert while focus has left the wizard's subtree
        // — but only for a moment after opening. `contains_focused` cannot tell
        // "someone stole focus" from "a popup that opened this frame owns it",
        // because it answers from the previously rendered frame; reclaiming
        // forever therefore closed the model dropdown on the next repaint. See
        // `FOCUS_CLAIM_WINDOW`.
        if self
            .focus_claim_until
            .is_some_and(|deadline| Instant::now() < deadline)
            && !self.focus_handle.contains_focused(window, cx)
        {
            self.focus_handle.focus(window, cx);
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let step = self.step;
        let motion = crate::motion_settings::active(cx);

        let dots = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .pb(px(16.0))
            .child(step_dot(step == OnboardingStep::Agent, theme))
            .child(step_dot(step == OnboardingStep::ChatView, theme))
            // Third dot only when the driver step is reachable this run —
            // a wizard that will finish from step 2 must not promise a step 3.
            .when(self.driver_step_needed(), |row| {
                row.child(step_dot(step == OnboardingStep::Driver, theme))
            });

        let body = div()
            .id("onboarding-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(46.0))
            .child(match step {
                OnboardingStep::Agent => self.render_agent_step(cx),
                OnboardingStep::ChatView => self.render_chat_view_step(cx),
                OnboardingStep::Driver => self.render_driver_step(cx),
            });

        let footer = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .pt(px(20.0))
            .pb(px(18.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(density.gap_inline * 2.0))
                    .when(step != OnboardingStep::Agent, |row| {
                        row.child(
                            Button::new("onboarding-back").ghost().label("← Back").on_click(
                                cx.listener(|this, _, _window, cx| this.back(cx)),
                            ),
                        )
                    })
                    .child(
                        Button::new("onboarding-continue")
                            .primary()
                            .label(match step {
                                OnboardingStep::Agent => "Continue →",
                                OnboardingStep::ChatView if self.driver_step_needed() => {
                                    "Continue →"
                                }
                                OnboardingStep::ChatView | OnboardingStep::Driver => "Finish",
                            })
                            .on_click(cx.listener(|this, _, _window, cx| this.next(cx))),
                    ),
            )
            .child(
                div()
                    .id("onboarding-skip")
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.fg_muted))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _window, cx| this.skip(cx)),
                    )
                    .child("Skip"),
            );

        let card = div()
            .id("onboarding-card")
            .flex()
            .flex_col()
            .w(px(CARD_WIDTH))
            .max_h(px(CARD_MAX_HEIGHT))
            .relative()
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .overflow_hidden()
            // 1px top inner-highlight: elevation without a shadow (design
            // contract for floating surfaces; painted first so it sits under
            // content and never eats clicks).
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(density.r_card))
                    .right(px(density.r_card))
                    .h(px(1.0))
                    .bg(theme.edge_highlight),
            )
            .child(div().pt(px(24.0)).child(dots))
            .child(body)
            .child(footer)
            .track_focus(&self.focus_handle)
            // Escape never arrives as a raw key: the keymap binds it to
            // DismissOverlay app-wide, so GPUI dispatches the ACTION instead.
            // Handle it innermost (this card holds focus) and stop it there —
            // otherwise the root's DismissOverlay handler eats it and Esc
            // silently does nothing in the wizard (the known Escape-swallow).
            .on_action(cx.listener(|this, _: &crate::actions::DismissOverlay, _window, cx| {
                cx.stop_propagation();
                this.skip(cx);
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => this.skip(cx),
                    "enter" => this.next(cx),
                    "down" if this.step == OnboardingStep::Agent => {
                        this.move_selection(1, window, cx)
                    }
                    "up" if this.step == OnboardingStep::Agent => {
                        this.move_selection(-1, window, cx)
                    }
                    "left" if this.step == OnboardingStep::ChatView => {
                        this.open_mode = oximux_settings::OpenMode::Chat;
                        cx.notify();
                    }
                    "right" if this.step == OnboardingStep::ChatView => {
                        this.open_mode = oximux_settings::OpenMode::Terminal;
                        cx.notify();
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }));

        // Backdrop: occludes the workspace but has NO dismiss handler — the
        // only ways out are the explicit Skip / Finish paths.
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0.0, 0.0, 0.02, 0.55))
            // Enter: fade + 8px rise (no transform-scale on div in GPUI).
            // Static id is safe: the wizard unmounts while closed, so each
            // open re-mounts and replays exactly once.
            .child(card.with_animation(
                "onboarding-enter",
                Animation::new(motion.m_overlay).with_easing(oximux_settings::ease_out_spring()),
                |el, delta| el.opacity(delta).mt(px(8.0 * (1.0 - delta))),
            ))
            .into_any_element()
    }
}

fn step_dot(active: bool, theme: oximux_settings::Theme) -> gpui::Div {
    let dot = div().h(px(5.0)).rounded(px(3.0));
    if active {
        dot.w(px(16.0)).bg(theme.fg_base)
    } else {
        dot.w(px(5.0)).bg(theme.fg_subtle).opacity(0.4)
    }
}

impl OnboardingWizard {
    /// Step 1 body: welcome heading + the agent picker (agent_step.rs).
    fn render_agent_step(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let heading = step_heading(
            "Welcome to OxiMux",
            "Choose your default agent — change anytime in Settings",
            self.theme,
            &self.typography.clone(),
        );
        div().flex().flex_col().child(heading).child(self.render_agent_body(cx))
    }

    /// Step 2 body: heading + the open-mode cards (view_step.rs).
    fn render_chat_view_step(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let heading = step_heading(
            "Choose your chat view",
            "Both work with any agent — change anytime in Settings",
            self.theme,
            &self.typography.clone(),
        );
        div()
            .flex()
            .flex_col()
            .child(div().h(px(26.0)))
            .child(heading)
            .child(self.render_view_body(cx))
    }
}

pub(super) fn step_heading(
    title: &'static str,
    subtitle: &'static str,
    theme: oximux_settings::Theme,
    typography: &oximux_settings::Typography,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(7.0))
        .pb(px(24.0))
        .child(
            div()
                .text_size(px(21.0))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(title),
        )
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_muted)
                .child(subtitle),
        )
}
