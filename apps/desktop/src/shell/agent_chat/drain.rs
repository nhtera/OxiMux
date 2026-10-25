//! A connection's events, folded into the view on the foreground.
//!
//! Each drain carries the generation it was started under. A respawn starts a
//! new one, and an older drain stops at its next step: it applies nothing more
//! and, above all, never reports the old process's exit as this chat's
//! disconnect. That matters because a respawn can run from inside a drain —
//! the turn-end event runs a respawn a mid-turn model switch asked for — and
//! that drain then sees its own channel close.

use futures::StreamExt;
use gpui::{Context, Task, WeakEntity};
use oximux_agents::thread::ThreadEvent;

use super::AgentChatView;

impl AgentChatView {
    /// Drain `rx` as this chat's current connection, retiring any earlier drain.
    pub(super) fn start_drain(&mut self, rx: std::sync::mpsc::Receiver<ThreadEvent>, cx: &mut Context<Self>) {
        self.drain_gen += 1;
        self._drain_task = Some(Self::spawn_drain(rx, self.drain_gen, cx));
    }

    /// Bridge the connection's blocking `std::mpsc` receiver onto the
    /// foreground: a dedicated OS thread forwards each decoded event to an async
    /// channel a `cx.spawn` task awaits and applies. The forwarder exits when
    /// the process closes stdout, which ends the async channel and triggers the
    /// fail-closed disconnect handler — unless a newer drain has taken over.
    pub(super) fn spawn_drain(
        rx: std::sync::mpsc::Receiver<ThreadEvent>,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let (fwd_tx, mut fwd_rx) = futures::channel::mpsc::unbounded::<ThreadEvent>();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                if fwd_tx.unbounded_send(ev).is_err() {
                    break; // view gone
                }
            }
            // `rx` disconnected (stdout EOF / process exit): `fwd_tx` drops here,
            // so the foreground task observes the channel end and fails closed.
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(ev) = fwd_rx.next().await {
                // Drain whatever else is already queued behind this event and
                // apply the lot as one batch: under a burst (a fast model's
                // token stream) this collapses N repaints into one for free,
                // with no added latency — these events had all arrived anyway.
                let mut batch = vec![ev];
                while let Ok(queued) = fwd_rx.try_recv() {
                    batch.push(queued);
                }
                if !this.update(cx, |view, cx| view.drain_batch(generation, batch, cx)).unwrap_or(false) {
                    return; // view dropped, or a respawn replaced this drain
                }
            }
            let _ = this.update(cx, |view, cx| view.drain_closed(generation, cx));
        })
    }

    /// Apply a batch from drain `generation`; false once a newer drain is
    /// current, including one a respawn inside this batch started.
    fn drain_batch(&mut self, generation: u64, batch: Vec<ThreadEvent>, cx: &mut Context<Self>) -> bool {
        if self.drain_gen != generation {
            return false;
        }
        self.apply_batch(batch, cx);
        self.drain_gen == generation
    }

    /// Drain `generation`'s process closed its output: a disconnect only while it
    /// is still this chat's connection.
    fn drain_closed(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.drain_gen == generation {
            self.on_disconnect(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use oximux_settings::{Density, Theme, Typography};

    use super::AgentChatView;

    /// A respawn from inside a drain (a turn's end running a deferred model
    /// switch) leaves that drain running: its process's exit, and anything it
    /// still had queued, must not reach the chat now on the new process.
    #[gpui::test]
    async fn a_replaced_drain_neither_applies_nor_disconnects(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                let old = view.drain_gen;
                view.drain_gen += 1; // what `start_drain` does on a respawn
                let entries = view.thread.entries.len();

                let stale = vec![ThreadEvent::Notice("from the old process".into())];
                assert!(!view.drain_batch(old, stale, cx), "the old drain stops");
                assert_eq!(view.thread.entries.len(), entries, "nothing it held is applied");
                view.drain_closed(old, cx);
                assert!(!view.disconnected, "the old process's exit is not this chat's");
                assert!(view.thread.last_error.is_none());

                view.drain_closed(view.drain_gen, cx);
                assert!(view.disconnected, "the current process's exit still fails closed");
            })
            .expect("window update");
    }

    /// The live bug: a mid-turn model switch respawns when the turn ends, from
    /// inside the drain applying that `TurnEnded`. The rest of that batch came
    /// from the old process and is not applied, and the drain stops.
    #[gpui::test]
    async fn a_respawn_inside_a_batch_ends_that_batch_and_its_drain(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        super::super::respawn_gate::tests::RESPAWN_IN_PLACE.with(|f| f.set(true));
        window
            .update(cx, |view, _window, cx| {
                let generation = view.drain_gen;
                view.thread.turn_active = true;
                view.pending_respawn = true; // a model switch asked for mid-turn
                let batch = vec![
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    ThreadEvent::Notice("after the respawn".into()),
                ];
                assert!(!view.drain_batch(generation, batch, cx), "the drain that ran the respawn stops");
                assert!(!view.thread.turn_active, "the turn's end was applied");
                assert!(!view.pending_respawn, "and ran the respawn");
                let late = view.thread.entries.iter().any(|e| format!("{e:?}").contains("after the respawn"));
                assert!(!late, "the old process's later events are not applied");
                view.drain_closed(generation, cx);
                assert!(!view.disconnected, "nor is its exit");
            })
            .expect("window update");
        super::super::respawn_gate::tests::RESPAWN_IN_PLACE.with(|f| f.set(false));
    }
}
