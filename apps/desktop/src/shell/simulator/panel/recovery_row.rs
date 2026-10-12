//! The blank-screen ladder in the panel (see the hub's `recovery`): a line
//! while a step runs, then a hint that suggests erasing — which only the
//! user does, through the Device menu's own question.

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div, px};

use super::SimulatorPanel;
use super::device_menu::DevicePrompt;
use crate::shell::simulator::hub::RecoveryStep;

impl SimulatorPanel {
    pub(super) fn render_recovery_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (hub, udid) = (self.hub.as_ref()?, self.device(cx)?);
        let (theme, density, ty) = (self.theme, self.density, &self.typography);
        let line = |text: &'static str| {
            div().w_full().text_center().text_size(px(ty.t_body_sm)).text_color(theme.fg_muted).py(px(density.gap_inline)).child(text).into_any_element()
        };
        match hub.read(cx).recovery_step(&udid) {
            RecoveryStep::Watching => None,
            RecoveryStep::WaitingForBoot => Some(line("No picture yet: waiting for the boot to finish, then restarting the stream…")),
            RecoveryStep::Rebooting => Some(line("Still no picture: rebooting the simulator (1/1)…")),
            RecoveryStep::Hint if self.recovery_dismissed.contains(&udid) => None,
            RecoveryStep::Hint => {
                let (dismiss, erase) = (udid.clone(), udid.clone());
                Some(self.render_question(
                    format!("Still no picture from {}", self.device_name(cx)),
                    "Restarting the stream did not help. Erasing the device often does (every app and its data are deleted).".into(),
                    ("sim-recovery-dismiss", "Dismiss"),
                    ("sim-recovery-erase", "Erase…"),
                    cx.listener(move |this, _, _window, cx| {
                        this.recovery_dismissed.insert(dismiss.clone());
                        cx.notify();
                    }),
                    // The Device menu's own question: a plain confirmation.
                    cx.listener(move |this, _, _window, cx| {
                        this.recovery_dismissed.insert(erase.clone());
                        this.device_prompt = Some(DevicePrompt::Erase(erase.clone()));
                        cx.notify();
                    }),
                ))
            }
        }
    }
}
