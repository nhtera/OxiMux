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
    /// the phone: may agents control the device, or — on a real device,
    /// asked every time — may one install an app on it.
    pub(super) fn render_consent_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, worktree) = (self.hub.as_ref()?, self.worktree.as_ref()?);
        if let Some((id, device, app)) = hub.read(cx).install_request(worktree) {
            return Some(self.render_install(id, device, app, cx));
        }
        let (udid, name) = hub.read(cx).consent_request(worktree)?;
        Some(self.render_consent(udid, name, cx))
    }

    /// The badge while an agent drives the device. Floats over the top of the
    /// body rather than taking a row: it comes and goes with every verb, and
    /// the phone must not jump each time.
    pub(super) fn render_agent_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, udid) = (self.hub.as_ref()?, self.device(cx)?);
        let worktree = self.worktree.as_ref()?;
        if !hub.read(cx).agent_active(&udid)
            || hub.read(cx).consent_request(worktree).is_some()
            || hub.read(cx).install_request(worktree).is_some()
        {
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
        let body = consent_body(&udid);
        let (allow, deny) = (udid.clone(), udid);
        let allowed_name = name.clone();
        self.render_question(
            format!("Let agents control {name}?"),
            body,
            ("sim-consent-deny", "Don't allow"),
            ("sim-consent-allow", "Allow"),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.deny_agents(&deny, cx));
                }
            }),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.allow_agents(&allow, allowed_name.clone(), cx));
                }
            }),
        )
    }

    /// An agent wants to install `app` on the real device `device`. Asked
    /// every time: controlling a phone is not putting apps on it.
    fn render_install(&self, id: u64, device: String, app: String, cx: &mut Context<Self>) -> AnyElement {
        self.render_question(
            format!("Install {app} on {device}?"),
            "An agent in this worktree wants to install this app on your real device. Allow it only if you expected this build."
                .into(),
            ("sim-install-deny", "Don't install"),
            ("sim-install-allow", "Install"),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.answer_install(id, false, cx));
                }
            }),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.answer_install(id, true, cx));
                }
            }),
        )
    }

    /// One question card: a title, why it is asked, and two answers.
    fn render_question(
        &self,
        title: String,
        body: String,
        (deny_id, deny_label): (&'static str, &'static str),
        (allow_id, allow_label): (&'static str, &'static str),
        on_deny: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        on_allow: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
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
                    .child(title),
            )
            .child(div().text_size(px(ty.t_body_sm)).text_color(theme.fg_muted).child(body))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(Button::new(deny_id).ghost().small().label(deny_label).on_click(on_deny))
                    .child(Button::new(allow_id).primary().small().label(allow_label).on_click(on_allow)),
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

/// Why the consent question is asked. A real device's says so — screenshots
/// may show the owner's own data — and that the answer lasts until quit.
fn consent_body(udid: &DeviceId) -> String {
    if udid.is_physical() {
        "An agent in this worktree wants to tap, type, and take screenshots of this real device. \
         Screenshots go to the agent's model provider and may show your personal data. \
         Access lasts until OxiMux quits."
            .into()
    } else {
        "An agent in this worktree wants to tap, type, and take screenshots of this simulator. \
         Screenshots go to the agent's model provider — don't sign in to real accounts on this device."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_device_is_named_as_one_and_its_access_ends_at_quit() {
        let real = consent_body(&DeviceId("adb:R58M123".into()));
        assert!(real.contains("real device") && real.contains("until OxiMux quits"), "{real}");
        let sim = consent_body(&DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into()));
        assert!(sim.contains("simulator") && !sim.contains("real device"));
    }
}
