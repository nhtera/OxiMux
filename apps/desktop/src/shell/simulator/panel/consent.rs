//! Above the phone: the consent question an agent in this worktree is
//! waiting on, or — once agents may drive the device — the "Agent is using
//! this device" badge while one does.
//!
//! The question is built only from what the app resolved itself (the device's
//! name from `simctl`), never from anything the agent sent. It shows only in
//! the panel of the worktree whose agent asked; other windows and worktrees
//! hear about it through a toast (see `root_glue`). The badge is advisory:
//! the user's own input keeps working while an agent drives.

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::DeviceId;

use super::SimulatorPanel;

impl SimulatorPanel {
    /// The question this worktree's agent is waiting on, in the flow above
    /// the phone.
    pub(super) fn render_consent_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, worktree) = (self.hub.as_ref()?, self.worktree.as_ref()?);
        let (udid, name) = hub.read(cx).consent_request(worktree)?;
        Some(self.render_consent(udid, name, cx))
    }

    /// The badge while an agent drives the device. Floats over the top of the
    /// body rather than taking a row: it comes and goes with every verb, and
    /// the phone must not jump each time.
    pub(super) fn render_agent_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, udid) = (self.hub.as_ref()?, self.device(cx)?);
        if !hub.read(cx).agent_active(&udid) || hub.read(cx).consent_request(self.worktree.as_ref()?).is_some() {
            return None;
        }
        Some(
            div()
                .absolute()
                .top(px(self.density.pad_row * 0.5))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(self.render_badge())
                .into_any_element(),
        )
    }

    fn render_consent(&self, udid: DeviceId, name: String, cx: &mut Context<Self>) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let (allow, deny) = (udid.clone(), udid);
        let allowed_name = name.clone();
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
                    .child(format!("Let agents control {name}?")),
            )
            .child(div().text_size(px(ty.t_body_sm)).text_color(theme.fg_muted).child(
                "An agent in this worktree wants to tap, type, and take screenshots of this simulator. \
                 Screenshots go to the agent's model provider — don't sign in to real accounts on this device.",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(Button::new("sim-consent-deny").ghost().small().label("Don't allow").on_click(cx.listener(
                        move |this, _, _window, cx| {
                            if let Some(hub) = this.hub.clone() {
                                hub.update(cx, |hub, cx| hub.deny_agents(&deny, cx));
                            }
                        },
                    )))
                    .child(Button::new("sim-consent-allow").primary().small().label("Allow").on_click(cx.listener(
                        move |this, _, _window, cx| {
                            if let Some(hub) = this.hub.clone() {
                                hub.update(cx, |hub, cx| hub.allow_agents(&allow, allowed_name.clone(), cx));
                            }
                        },
                    ))),
            )
            .into_any_element()
    }

    fn render_badge(&self) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .flex_none()
            .px(px(density.pad_panel))
            .py(px(density.pad_row * 0.5))
            .rounded_full()
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .child(div().size(px(6.)).rounded_full().bg(theme.status_info))
            .child(div().text_size(px(ty.t_body_sm)).text_color(theme.fg_base).child("Agent is using this device"))
            .into_any_element()
    }
}
