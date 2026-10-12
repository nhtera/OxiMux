//! Staging something in the worktree's agent — its chat composer, else an
//! agent terminal's prompt — by the browser picker's route
//! (`SendPickToActiveChat`): an annotated screenshot, a crash.

use gpui::{Context, Window};

use super::{RootRequest, SimulatorPanel};
use crate::actions::SendPickToActiveChat;

impl SimulatorPanel {
    /// Stage `markdown` (and `png`, when there is one) in the active agent.
    pub(super) fn send_to_agent(&self, markdown: String, png: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sink) = self.command_sink.clone() else { return };
        let pick = SendPickToActiveChat { markdown, selector: "Mobile Emulator".into(), png };
        // After this update: the root may update the panel.
        window.defer(cx, move |window, cx| sink(RootRequest::SendToAgent(pick), window, cx));
    }
}
