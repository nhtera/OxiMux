//! The chat's writes to the host's durable send receipts and retry counts
//! (V033), so what a crash or restart leaves behind is known rather than
//! guessed: a send the agent may already have is never resent on its own, and
//! a rate-limited turn's attempt count does not reset on relaunch.
//!
//! The tables belong to the host core, which the app installs at boot; with no
//! core (tests) every write here is a no-op and the chat behaves as before.
//! Writes go off the UI thread through one ordered queue for the whole app
//! ([`HostWriter`]), so a chat's writes land in the order it made them: a
//! retry is armed before it is counted as fired, and a supersede never lands
//! after the next turn's arm.

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, Context, Global};
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
    pub lineage: oximux_agents::lineage::LineageStore,
    pub instance: String,
}

impl HostStores {
    pub(super) fn of(cx: &App) -> Option<Self> {
        let core = cx.try_global::<crate::remote_control::RemoteControl>()?.host_core()?;
        Some(Self {
            receipts: core.lineage().receipts(),
            retries: core.lineage().retries(),
            lineage: core.lineage().clone(),
            instance: core.instance().to_string(),
        })
    }
}

type Job = Box<dyn FnOnce(&HostStores) -> anyhow::Result<()> + Send>;

/// The app's one queue of writes to the host's tables, drained in order by a
/// single background task.
struct HostWriter(mpsc::UnboundedSender<(HostStores, Job)>);

impl Global for HostWriter {}

/// Queue `job` behind every write already queued. A no-op with no host core.
fn enqueue(cx: &mut App, job: Job) {
    let Some(stores) = HostStores::of(cx) else { return };
    if !cx.has_global::<HostWriter>() {
        let (tx, mut rx) = mpsc::unbounded::<(HostStores, Job)>();
        cx.background_executor()
            .spawn(async move {
                while let Some((stores, job)) = rx.next().await {
                    if let Err(err) = job(&stores) {
                        tracing::warn!(%err, "host record write failed");
                    }
                }
            })
            .detach();
        cx.set_global(HostWriter(tx));
    }
    let _ = cx.global::<HostWriter>().0.unbounded_send((stores, job));
}

/// Whether a failed turn's send was refused rather than taken: a failure
/// waiting can fix (a limit or an overload) and no reply or tool call after
/// the prompt. Safe to resend verbatim.
pub(super) fn refused(class: RetryClass, turn_had_output: bool) -> bool {
    class.is_retryable() && !turn_had_output
}

impl AgentChatView {
    /// Record proof (or, with `None`, the reservation) for the send `uuid`,
    /// and for every receipt that went out with it.
    pub(super) fn record_receipt(&self, uuid: &str, state: Option<ReceiptState>, cx: &mut Context<Self>) {
        let carried = self.send_gate.lock().unwrap().carried_by(uuid);
        for key in std::iter::once(key_hash(KeyKind::Gate, uuid)).chain(carried) {
            let (session, now) = (self.remote_session_id.clone(), chrono::Local::now());
            enqueue(
                cx,
                Box::new(move |s| match state {
                    None => s.receipts.reserve(&session, &key, &s.instance, now).map(|_| ()),
                    Some(state) => s.receipts.settle(&session, &key, state, &s.instance, now),
                }),
            );
        }
    }

    /// The refused send whose receipt key is `key` went out again: pending
    /// until the agent answers it.
    pub(super) fn record_resend(&self, key: String, cx: &mut Context<Self>) {
        let (session, now) = (self.remote_session_id.clone(), chrono::Local::now());
        enqueue(cx, Box::new(move |s| s.receipts.resend(&session, &key, now)));
    }

    /// The chat moved from `old` to its agent's id: move its records too, after
    /// any write still queued under the old id.
    pub(super) fn rekey_host_records(&self, old: String, new: String, cx: &mut Context<Self>) {
        enqueue(cx, Box::new(move |s| s.lineage.rekey(&old, &new)));
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
        let now = chrono::Local::now();
        enqueue(cx, Box::new(move |s| write(s, now)));
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
