//! The chat's writes to the host's durable send receipts and retry counts
//! (V033), so what a crash or restart leaves behind is known rather than
//! guessed: a send the agent may already have is never resent on its own, and
//! a rate-limited turn's attempt count does not reset on relaunch.
//!
//! The tables belong to the host core, which the app installs at boot; with no
//! core (tests) every write here is a no-op and the chat behaves as before.
//! Writes go to the background executor: each is one indexed row, and an
//! out-of-order pair settles the same (a proof that beats its reservation
//! inserts the row, and the reservation then leaves it alone).

use gpui::{App, Context};
use oximux_agents::retry::RetryClass;
use oximux_agents::retry::store::{Resolution, RetryStore};
use oximux_agents::send_receipts::{KeyKind, ReceiptState, Receipts, key_hash};

use oximux_agents::thread::{ThreadEntry, ThreadEvent};

use super::AgentChatView;

/// The host's tables, as one chat writes them.
#[derive(Clone)]
pub(super) struct HostStores {
    pub receipts: Receipts,
    pub retries: RetryStore,
    pub instance: String,
}

impl HostStores {
    pub(super) fn of(cx: &App) -> Option<Self> {
        let core = cx.try_global::<crate::remote_control::RemoteControl>()?.host_core()?;
        Some(Self {
            receipts: core.lineage().receipts(),
            retries: core.lineage().retries(),
            instance: core.instance().to_string(),
        })
    }
}

/// Whether a failed turn's send was refused rather than taken: a failure
/// waiting can fix (a limit or an overload) and no reply or tool call after
/// the prompt. Safe to resend verbatim.
pub(super) fn refused(class: RetryClass, turn_had_output: bool) -> bool {
    class.is_retryable() && !turn_had_output
}

impl AgentChatView {
    /// Record proof (or, with `None`, the reservation) for the send `uuid`.
    pub(super) fn record_receipt(&self, uuid: &str, state: Option<ReceiptState>, cx: &mut Context<Self>) {
        let Some(stores) = HostStores::of(cx) else { return };
        let (session, key) = (self.remote_session_id.clone(), key_hash(KeyKind::Gate, uuid));
        let now = chrono::Local::now();
        cx.background_executor()
            .spawn(async move {
                let done = match state {
                    None => stores.receipts.reserve(&session, &key, &stores.instance, now).map(|_| ()),
                    Some(state) => stores.receipts.settle(&session, &key, state, &stores.instance, now),
                };
                if let Err(err) = done {
                    tracing::warn!(%err, "send receipt write failed");
                }
            })
            .detach();
    }

    /// Settle the host's receipts from an agent event (or a stand-in for one):
    /// a send it started on, or reported, was accepted; a turn that ended well
    /// ends any retry that got it there.
    pub(super) fn settle_receipts(&mut self, event: &ThreadEvent, cx: &mut Context<Self>) {
        match event {
            ThreadEvent::UserMessageAccepted { uuid } => self.record_receipt(uuid, Some(ReceiptState::Accepted), cx),
            ThreadEvent::UserMessagesDelivered { uuids } => {
                for uuid in uuids {
                    self.record_receipt(uuid, Some(ReceiptState::Accepted), cx);
                }
            }
            ThreadEvent::TurnEnded { is_error: false, .. } => self.retry_succeeded(cx),
            _ => {}
        }
    }

    /// What is recorded for the send `uuid` (read at restore only).
    pub(super) fn receipt_state(&self, uuid: &str, cx: &App) -> Option<ReceiptState> {
        let stores = HostStores::of(cx)?;
        stores.receipts.state(&self.remote_session_id, &key_hash(KeyKind::Gate, uuid)).ok().flatten()
    }

    /// The key the failed turn's retries count under: the receipt key of the
    /// send that started it.
    pub(super) fn retry_turn_key(&self) -> String {
        let id = self.health.sent_id().unwrap_or_else(|| {
            // An untagged send: its text and place in the transcript.
            let at = self.thread.entries.iter().rposition(|e| matches!(e, ThreadEntry::User { .. }));
            let text = at.and_then(|i| match &self.thread.entries[i] {
                ThreadEntry::User { text, .. } => Some(text.as_str()),
                _ => None,
            });
            format!("untagged:{}:{}", at.unwrap_or(0), text.unwrap_or_default())
        });
        key_hash(KeyKind::Gate, &id)
    }

    /// Attempts already fired for `turn_key`, as the host recorded them.
    pub(super) fn durable_attempts(&self, turn_key: &str, cx: &App) -> u32 {
        HostStores::of(cx)
            .and_then(|s| s.retries.attempts(&self.remote_session_id, turn_key).ok())
            .unwrap_or(0)
    }

    /// The host's live retry for this chat: its turn key and attempts.
    pub(super) fn durable_live_retry(&self, cx: &App) -> Option<(String, u32)> {
        let live = HostStores::of(cx)?.retries.live(&self.remote_session_id).ok()??;
        Some((live.turn_key, live.attempts))
    }

    /// A retry was armed for `turn_key`.
    pub(super) fn record_retry_armed(&self, turn_key: String, class: RetryClass, reason: String, wake_at_ms: i64, cx: &mut Context<Self>) {
        let session = self.remote_session_id.clone();
        self.with_retries(cx, move |s, now| {
            let class = oximux_agents::retry::driver::class_text(class);
            s.retries.arm(&session, &turn_key, class, &reason, wake_at_ms, &s.instance, now).map(|_| ())
        });
    }

    /// The armed retry fired.
    pub(super) fn record_retry_fired(&self, turn_key: String, cx: &mut Context<Self>) {
        let session = self.remote_session_id.clone();
        self.with_retries(cx, move |s, _| s.retries.fired(&session, &turn_key).map(|_| ()));
    }

    /// The chat's live retry ended.
    pub(super) fn record_retry_resolved(&self, how: Resolution, cx: &mut Context<Self>) {
        let session = self.remote_session_id.clone();
        self.with_retries(cx, move |s, now| s.retries.resolve(&session, how, now).map(|_| ()));
    }

    fn with_retries(
        &self,
        cx: &mut Context<Self>,
        write: impl FnOnce(&HostStores, chrono::DateTime<chrono::Local>) -> anyhow::Result<()> + Send + 'static,
    ) {
        let Some(stores) = HostStores::of(cx) else { return };
        let now = chrono::Local::now();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = write(&stores, now) {
                    tracing::warn!(%err, "retry record write failed");
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_retryable_failure_with_no_output_was_refused() {
        assert!(refused(RetryClass::Overload, false));
        assert!(!refused(RetryClass::Overload, true), "the agent had started: it took the prompt");
        assert!(!refused(RetryClass::Other, false), "not a refusal waiting can fix");
    }
}

#[cfg(test)]
#[path = "receipt_link_tests.rs"]
mod view_tests;
