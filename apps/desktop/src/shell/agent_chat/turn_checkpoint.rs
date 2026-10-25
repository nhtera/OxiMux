//! The rewind anchor for each turn: a repo snapshot taken when a prompt is
//! sent, compared with a fresh one when the turn ends, so the "restore files"
//! affordance only lights up on a turn that changed something.

use gpui::Context;

use super::AgentChatView;

impl AgentChatView {
    /// Take a checkpoint anchored to the user entry at `user_index`, off-thread.
    /// Attaches the sha to that entry when done (a no-op if the thread moved on)
    /// and records it as the pre-turn snapshot for the turn-end compare.
    pub(super) fn take_checkpoint_for(&mut self, user_index: usize, cx: &mut Context<Self>) {
        let Some(engine) = self.checkpoint_engine.clone() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        // Bind the snapshot to the session it was taken for. A rewind mints a
        // new session id and renumbers entries, so a straggling callback from a
        // pre-rewind turn must not misattach onto a same-index entry.
        let session = self.thread.session_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.spawn(async move {
            let _ = tx.send(engine.create().await.ok());
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(sha)) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.thread.session_id != session {
                        return; // stale — the session was rewound out from under us
                    }
                    this.thread.attach_checkpoint(user_index, sha.0.clone());
                    this.pre_turn_checkpoint =
                        Some((user_index, oximux_git::checkpoint::CheckpointSha(sha.0)));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// After a turn ends, compare a fresh snapshot against the pre-turn one; if
    /// they differ, light up the rewind "restore files" affordance on that
    /// turn's user entry. The fresh snapshot is only used for the compare.
    pub(super) fn compare_turn_checkpoint(&mut self, cx: &mut Context<Self>) {
        let (Some(engine), Some((index, pre_sha))) =
            (self.checkpoint_engine.clone(), self.pre_turn_checkpoint.take())
        else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let session = self.thread.session_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        handle.spawn(async move {
            let changed = match engine.create().await {
                Ok(post) => engine.differs(&pre_sha, &post).await.unwrap_or(false),
                Err(_) => false,
            };
            let _ = tx.send(changed);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(changed) = rx.await
                && changed
            {
                let _ = this.update(cx, |this, cx| {
                    if this.thread.session_id != session {
                        return; // stale — session rewound before the compare landed
                    }
                    this.thread.set_checkpoint_show(index, true);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}
