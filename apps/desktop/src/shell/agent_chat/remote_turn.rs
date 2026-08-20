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
    /// Drain relayed remote prompts (phone sends) and fold each as a user bubble,
    /// and host-synthesized events (an operator-edited approval) into the fold.
    /// One task for both channels — they share a lifetime (bound and re-bound
    /// together) and folding them from one place keeps ordering trivial. Ends
    /// when both senders drop (rebind/teardown).
    pub(super) fn spawn_remote_prompt_relay(
        rx: futures::channel::mpsc::UnboundedReceiver<RemotePrompt>,
        events_rx: futures::channel::mpsc::UnboundedReceiver<ThreadEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        enum Relayed {
            Prompt(RemotePrompt),
            Event(ThreadEvent),
        }
        let mut merged = futures::stream::select(
            rx.map(Relayed::Prompt),
            events_rx.map(Relayed::Event),
        );
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(item) = merged.next().await {
                let folded = this.update(cx, |view, cx| match item {
                    Relayed::Prompt(prompt) => view.push_remote_user_bubble(prompt, cx),
                    Relayed::Event(event) => {
                        // Through the fold's own apply — the registry already
                        // ingested it for remote subscribers; this is the copy
                        // for the transcript this view owns.
                        view.thread.apply(&event);
                        cx.notify();
                    }
                });
                if folded.is_err() {
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
        self.follow_bottom();
        self.follow_frames = FOLLOW_FRAMES;
        cx.notify();
    }

    /// Every piece of screen-control bookkeeping the thread's own events drive:
    /// note a capture the moment one comes back, and let the turn's marks go
    /// when it ends.
    ///
    /// One entry point rather than a call per concern, because each of them
    /// costs a line in `agent_chat/mod.rs` — already the largest file in the
    /// repo and pinned by the size ratchet, which only ever ratchets down. A
    /// second call there is not a style question; it fails the build.
    ///
    /// The remote mark is cleared unconditionally rather than paired with a "was
    /// this turn remote" flag: clearing something already clear costs nothing,
    /// while a flag that drifted out of step would leave screen control refused
    /// for a chat with nothing on screen pointing at why.
    ///
    /// The same boundary ends the chat's *driving* turn, and releases an abort.
    /// Those are what the menu-bar indicator and the Escape tap read, and a
    /// grant cannot end them: a grant lasts as long as the chat, so anything
    /// waiting on one to lapse would keep claiming this chat drives the screen —
    /// and keep Escape swallowed machine-wide — until the tab was closed.
    pub(super) fn note_screen_activity(&self, ev: &ThreadEvent) {
        // After the fold, so a result's own tool call is already in the
        // transcript to be looked up by id.
        self.note_screen_capture(ev);
        if matches!(ev, ThreadEvent::TurnEnded { .. }) {
            self.screen_control.end_remote_turn();
            self.screen_control.end_turn_activity();
        }
    }
}
