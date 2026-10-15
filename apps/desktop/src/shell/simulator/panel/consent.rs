//! Above the phone: the consent question an agent in this worktree is
//! waiting on, or — once agents may drive the device — the "Agent is using
//! this device" badge while one does.
//!
//! The question is built only from what the app resolved itself (the device's
//! name from `simctl`), never from anything the agent sent. It shows only in
//! the panel of the worktree whose agent asked, or in a grid tile showing
//! that worktree's device (see `grid`); other windows and worktrees hear
//! about it through a toast (see `root_glue`). The badge is advisory:
//! the user's own input keeps working while an agent drives.

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_component::{
    Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_remote_host::SimPrincipal;
use oximux_simulator::DeviceId;

use super::SimulatorPanel;
use crate::shell::simulator::hub::AskKind;

impl SimulatorPanel {
    /// The question this worktree's agent is waiting on, in the flow above
    /// the phone: may agents control the device, or — asked every time — may
    /// one install an app on a real device, erase it, send it a push, or
    /// change a real device's settings.
    pub(super) fn render_consent_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.render_consent_for(self.worktree.as_ref()?, cx)
    }

    /// The question `worktree`'s agent is waiting on, if any.
    pub(super) fn render_consent_for(&self, worktree: &std::path::Path, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hub = self.hub.as_ref()?;
        if let Some((id, device, kind, principal)) = hub.read(cx).per_call_request(worktree) {
            return Some(self.render_per_call(id, &device, &kind, principal, cx));
        }
        let (principal, udid, name) = hub.read(cx).consent_request(worktree)?;
        Some(self.render_consent(principal, udid, name, cx))
    }

    /// The badge while an agent drives the device. Floats over the top of the
    /// body rather than taking a row: it comes and goes with every verb, and
    /// the phone must not jump each time.
    pub(super) fn render_agent_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, udid) = (self.hub.as_ref()?, self.device(cx)?);
        let worktree = self.worktree.as_ref()?;
        if !hub.read(cx).agent_active(&udid)
            || hub.read(cx).consent_request(worktree).is_some()
            || hub.read(cx).per_call_request(worktree).is_some()
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

    /// May agents (or, asked apart, an outside MCP client) control the
    /// device? The answer goes to whoever asked.
    fn render_consent(&self, principal: SimPrincipal, udid: DeviceId, name: String, cx: &mut Context<Self>) -> AnyElement {
        let body = consent_body(principal, &udid);
        let title = match principal {
            SimPrincipal::Agent => format!("Let agents control {name}?"),
            SimPrincipal::Mcp => format!("Let an outside MCP client control {name}?"),
        };
        let (allow, deny) = (udid.clone(), udid);
        let allowed_name = name.clone();
        self.render_question(
            title,
            body,
            ("sim-consent-deny", "Don't allow"),
            ("sim-consent-allow", "Allow"),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| match principal {
                        SimPrincipal::Agent => hub.deny_agents(&deny, cx),
                        SimPrincipal::Mcp => hub.deny_mcp(&deny, cx),
                    });
                }
            }),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| match principal {
                        SimPrincipal::Agent => hub.allow_agents(&allow, allowed_name.clone(), cx),
                        SimPrincipal::Mcp => hub.allow_mcp(&allow, cx),
                    });
                }
            }),
        )
    }

    /// A question asked every time (see `hub::per_call`): controlling a
    /// device is not putting apps on it, wiping it, or changing its settings.
    fn render_per_call(&self, id: u64, device: &str, kind: &AskKind, principal: SimPrincipal, cx: &mut Context<Self>) -> AnyElement {
        let (title, body, (deny, allow)) = kind.question_from(device, principal);
        self.render_question(
            title,
            body,
            ("sim-per-call-deny", deny),
            ("sim-per-call-allow", allow),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.answer_per_call(id, false, cx));
                }
            }),
            cx.listener(move |this, _, _window, cx| {
                if let Some(hub) = this.hub.clone() {
                    hub.update(cx, |hub, cx| hub.answer_per_call(id, true, cx));
                }
            }),
        )
    }

    /// One question card: a title, why it is asked, and two answers.
    pub(super) fn render_question(
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
fn consent_body(principal: SimPrincipal, udid: &DeviceId) -> String {
    if principal == SimPrincipal::Mcp {
        let device = if udid.is_physical() { "this real device" } else { "this simulator" };
        return format!(
            "An app outside OxiMux, connected with `oximux mcp`, wants to tap, type, and take screenshots of {device}. \
             Agents' permission does not cover it. Screenshots go to wherever that app sends them. \
             Access lasts until OxiMux quits."
        );
    }
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
        let real = consent_body(SimPrincipal::Agent, &DeviceId("adb:R58M123".into()));
        assert!(real.contains("real device") && real.contains("until OxiMux quits"), "{real}");
        let sim = consent_body(SimPrincipal::Agent, &DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into()));
        assert!(sim.contains("simulator") && !sim.contains("real device"));
        let mcp = consent_body(SimPrincipal::Mcp, &DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into()));
        assert!(mcp.contains("oximux mcp") && mcp.contains("does not cover") && mcp.contains("until OxiMux quits"), "{mcp}");
    }
}
