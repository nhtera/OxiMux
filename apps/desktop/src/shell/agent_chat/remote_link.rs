//! The chat view's link into the host's session registry: binding the live
//! connection under the session's remote id, re-keying it onto the agent's own
//! id once one is minted, and publishing what a remote client shows for it
//! (title, effective model and permission mode, transcript).
//!
//! Moved verbatim out of `agent_chat/mod.rs` (no behaviour change) to give the
//! view file headroom under the file-size ratchet; the orchestration work
//! (spawn credentials, lineage rekey, the working flag) lands here.

use gpui::Context;
use oximux_agents::session_registry::SessionMeta;

use super::{AgentChatView, RemoteControl};

impl AgentChatView {
    /// Move this session onto the agent's own id once the agent has minted one.
    ///
    /// A chat that has never run is registered under a positional placeholder,
    /// which is worthless to a remote client the moment the desktop restarts.
    /// The agent's id names the conversation instead, so the session is re-keyed
    /// onto it at the first opportunity — which is also the first moment it has
    /// any history worth reaching from another device.
    ///
    /// Re-registering under the new id restarts `seq` at 1, so the old id is
    /// dropped only after the new one is live: a client subscribed to the
    /// placeholder sees its stream end and resubscribes, rather than sitting on a
    /// cursor no future event will ever exceed.
    pub(super) fn rekey_remote_session_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.thread.session_id.clone() else {
            return;
        };
        if agent_id.is_empty() || agent_id == self.remote_session_id {
            return;
        }
        let previous = std::mem::replace(&mut self.remote_session_id, agent_id);
        // `bind_remote` registers under the field just replaced; the old entry is
        // removed afterwards so the two never coexist beyond this call.
        self.bind_remote(cx);
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.rekey_session(&previous, &self.remote_session_id);
        }
    }

    /// (Re)bind this session into the remote-control registry: drop any prior
    /// binding, then register the current connection under [`Self::remote_session_id`]
    /// — but only while remote control is enabled, so a disabled desktop registers
    /// nothing and clones no events. Called on connect and on respawn.
    pub(super) fn bind_remote(&mut self, cx: &mut Context<Self>) {
        // Deliberately does NOT unregister first. On a respawn this runs again for
        // the same session id, and `register` swaps the backend in place — keeping
        // `seq` monotonic so a subscribed phone keeps receiving. Unregistering here
        // would mint a fresh handle at seq 1, which every subscriber would silently
        // discard as already-seen. Teardown paths still call `unbind_remote`.
        let bound = self
            .connection
            .clone()
            .and_then(|conn| {
                cx.try_global::<RemoteControl>().and_then(|rc| rc.bind(&self.remote_session_id, conn))
            });
        match bound {
            Some(binding) => {
                self.remote = Some(binding);
                // Expose the current transcript at once — on a restart this fold was
                // restored from disk and never entered the event ring, so a phone
                // opening the session before the next event would otherwise see it
                // empty. Meta too, so the row is labelled from the first list.
                self.publish_remote_meta();
                self.publish_remote_transcript();
                // Re-arm the remote-prompt echo relay against the new binding.
                let (tx, rx) = futures::channel::mpsc::unbounded();
                let (event_tx, event_rx) = futures::channel::mpsc::unbounded();
                if let Some(binding) = &self.remote {
                    binding.set_prompt_sink(tx);
                    binding.set_event_sink(event_tx);
                }
                self._remote_prompt_task = Some(Self::spawn_remote_prompt_relay(rx, event_rx, cx));
                // Point the *existing* choice relay at the new binding rather than
                // starting a fresh one. This path runs inside `change_model` →
                // `respawn`, so the relay is mid-flight on the very pick that
                // caused it: replacing the task here would drop that future and
                // cancel its reply, telling the phone a change it just made had
                // failed. Re-registering the same sender is enough — a rebind may
                // mint a new handle (after an unbind), and this points it back.
                let choice_tx = self.choice_relay_sender(cx);
                if let Some(binding) = &self.remote {
                    binding.set_choice_sink(choice_tx);
                }
            }
            // No connection — drop any prior binding.
            None => self.unbind_remote(),
        }
    }

    /// Drop this session's registry binding (on disconnect / teardown). No-op when
    /// unbound. Explicit because the registry retains its own handle `Arc`, so
    /// dropping the view's handle alone would not evict the session.
    pub(super) fn unbind_remote(&mut self) {
        if let Some(binding) = self.remote.take() {
            binding.unregister(&self.remote_session_id);
        }
    }

    /// Push this tab's title + effective model into the registry so a remote
    /// session list renders them instead of the raw `agent-N` id. No-op when remote
    /// is disabled (`remote` is `None`); the registry skips unchanged values, which
    /// is the common case on a per-batch call.
    pub(super) fn publish_remote_meta(&self) {
        let Some(binding) = &self.remote else {
            return;
        };
        binding.set_meta(SessionMeta {
            // The tab's visible title (a manual rename, else the running label) is
            // what the desktop shows, so it wins; fall back to the thread's
            // provider-native title until the pane group has synced one.
            title: self.remote_tab_title.clone().or_else(|| self.thread.title.clone()),
            model: self.effective_model(),
            permission_mode: self.effective_permission_mode(),
            // Git RPCs resolve their repository from this.
            cwd: Some(self.cwd.clone()),
        });
        // Every event batch passes through here, so the first batch of a turn
        // and its `TurnEnded` both reach the host's lineage subscriber.
        binding.set_working(
            self.thread.turn_active
                || self.thread.pending_permission().is_some()
                || self.thread.pending_question().is_some(),
        );
    }

    /// The model this tab is actually running under.
    ///
    /// The same shape as [`Self::effective_permission_mode`], and for the same
    /// reason: `model` is `None` until the user picks one, so a session that has
    /// never had a pick would otherwise publish nothing and a remote picker would
    /// render every model unselected — as if the session were running on no model
    /// at all. The desktop's own composer resolves it exactly this way, falling
    /// back to the connection's default when the tab holds no pick.
    ///
    /// The thread's negotiated model still wins: once the backend reports what it
    /// actually loaded, that is the truth, and it can differ from the default the
    /// child was launched with.
    pub(super) fn effective_model(&self) -> Option<String> {
        self.thread
            .model
            .clone()
            .or_else(|| self.model.clone())
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_model()))
    }

    /// The permission mode this tab is actually running under.
    ///
    /// `permission_mode` is `None` for the backend's baseline — that is what makes
    /// `respawn` omit the flag — so the field alone cannot say what is in force.
    /// A remote picker needs the resolved answer: it holds no connection of its
    /// own to ask for the baseline.
    pub(super) fn effective_permission_mode(&self) -> Option<String> {
        self.permission_mode
            .clone()
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_mode()))
    }

    /// Record the visible tab title (a manual rename, else the running `Chat N` /
    /// agent label) the desktop shows for this chat, so a remote session list
    /// renders the same name instead of the raw `agent-N` id. Pushed by the owning
    /// pane group on tab create, rename, and ambient title change; re-publishes the
    /// registry meta only when the title actually changed.
    pub fn set_remote_tab_title(&mut self, title: Option<String>) {
        if self.remote_tab_title == title {
            return;
        }
        self.remote_tab_title = title;
        self.publish_remote_meta();
    }
}
