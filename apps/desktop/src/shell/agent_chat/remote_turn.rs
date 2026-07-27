//! Turns that were started from a paired phone rather than from this desk.
//!
//! Two things happen when a prompt arrives over the remote RPC. The obvious one
//! is cosmetic: the desktop shows the user's own bubble, because no backend
//! echoes a prompt back and the chat would otherwise display a reply to
//! something it never rendered.
//!
//! The other is a safety gate. Screen control assumes a person is watching the
//! screen being driven and can answer a consent card that appears on the
//! desktop; from a phone they can do neither, and a remote prompt is opaque text
//! — "click Delete and confirm" is indistinguishable from any other instruction,
//! so the device's write tier cannot tell them apart. The turn is therefore
//! marked, and `oximux_computer_use::policy` refuses every screen-control call
//! reached from it.
//!
//! The mark lives here rather than in the session registry for a plain reason:
//! it is keyed by the chat's screen-control session id, and this view is the only
//! thing that holds one.

use futures::StreamExt;
use gpui::{Context, Task, WeakEntity};
use oximux_agents::session_registry::RemotePrompt;
use oximux_agents::thread::ThreadEvent;

use super::{AgentChatView, FOLLOW_FRAMES};

impl AgentChatView {
    /// Drain relayed remote prompts (phone sends) and fold each as a user bubble.
    /// Mirrors `spawn_drain` but for the prompt-echo channel the registry feeds
    /// on a remote `send_prompt`. Ends when the sender drops (rebind/teardown).
    pub(super) fn spawn_remote_prompt_relay(
        mut rx: futures::channel::mpsc::UnboundedReceiver<RemotePrompt>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(prompt) = rx.next().await {
                if this.update(cx, |view, cx| view.push_remote_user_bubble(prompt, cx)).is_err() {
                    return; // view dropped
                }
            }
        })
    }

    /// Show a phone's prompt as a user bubble, and mark the turn it starts.
    ///
    /// This channel carries only prompts that arrived over the remote RPC, so
    /// reaching it *is* the evidence that nobody is at the screen.
    fn push_remote_user_bubble(&mut self, prompt: RemotePrompt, cx: &mut Context<Self>) {
        self.screen_control.begin_remote_turn();
        self.thread.push_user_message_with_images(prompt.text, prompt.images);
        // Pin to the bottom so the new turn is in view, like a local send.
        self.stick_to_bottom = true;
        self.follow_frames = FOLLOW_FRAMES;
        cx.notify();
    }

    /// Let the gate go when the turn it covered finishes.
    ///
    /// Cleared unconditionally rather than paired with a "was this turn remote"
    /// flag: clearing something already clear costs nothing, while a flag that
    /// drifted out of step would leave screen control refused for a chat with
    /// nothing on screen pointing at why.
    ///
    /// The same boundary ends the chat's *driving* turn. That mark is what the
    /// menu-bar indicator and the Escape tap read, and its grant cannot end it:
    /// a grant lasts as long as the chat, so anything waiting on the grant to
    /// lapse would keep claiming this chat drives the screen — and keep Escape
    /// swallowed machine-wide — until the tab was closed.
    pub(super) fn note_turn_boundary(&self, ev: &ThreadEvent) {
        if matches!(ev, ThreadEvent::TurnEnded { .. }) {
            self.screen_control.end_remote_turn();
            self.screen_control.end_driving_turn();
        }
    }
}
