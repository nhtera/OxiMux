//! One-shot tab-title generation for a chat's first message.
//!
//! Split out of `mod.rs` — which sits at the `xtask file-size-lint` ratchet —
//! as its own concern: the spawn, the tokio bridge, and the two events the
//! result feeds. The gate that decides *whether* to generate stays in
//! `send_text`, beside the other first-message decisions.

use std::sync::Arc;

use gpui::Context;

use super::{AgentChatEvent, AgentChatView};

impl AgentChatView {
    /// Kick off a one-shot LLM title generation for this chat's first message.
    /// Owned on `title_task` so a tab close drops it. The generation runs a child
    /// process needing a tokio reactor, so it's handed to the tokio runtime and
    /// bridged back via a oneshot (the proven `source_control::ai_generation`
    /// pattern); a bounded timeout + `kill_on_drop` cap any lingering child
    /// (see `oximux_agents::tab_title` for the cap and why it is what it is).
    /// Any failure (missing `claude`, timeout, non-JSON reply) silently keeps the
    /// counter label. On success the result rides the existing, already-safe
    /// `TitleChanged` sink (a manual rename still wins in the header render) and,
    /// because it is a generated summary, the auto-rename signal
    /// `TaskSummaryReady`.
    pub(super) fn spawn_title_generation(&mut self, first_message: String, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        self.title_task = Some(cx.spawn(async move |this, cx| {
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                return;
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            handle.spawn(async move {
                let title = oximux_agents::tab_title::generate_title(&first_message, &cwd, cancel).await;
                let _ = tx.send(title);
            });
            if let Ok(Some(title)) = rx.await {
                let _ = this.update(cx, |view, cx| {
                    cx.emit(AgentChatEvent::TitleChanged(title.clone()));
                    cx.emit(AgentChatEvent::TaskSummaryReady {
                        cwd: view.cwd.clone(),
                        summary: title,
                    });
                });
            }
        }));
    }
}
