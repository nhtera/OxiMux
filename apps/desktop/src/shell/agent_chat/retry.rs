//! Automatic re-send of a turn that failed on a provider limit.
//!
//! The policy — what counts as retryable, how long to wait, how many times —
//! lives in `oximux_agents::retry` and is pure. This module holds only the
//! per-chat state and the timer, so the decision stays testable without a view.
//!
//! The re-send itself reuses `retry_last_turn`, the same path the manual Retry
//! button drives: it respawns a stopped child, resends the last user entry
//! verbatim, and re-anchors the turn checkpoint. An automatic retry that sent
//! by some other route would drift from the manual one, and the two would
//! disagree about exactly the edge cases that are hard to test.

use gpui::Task;
use oximux_agents::retry::{RetrySettings as AgentRetryPolicy, classify_failure};
use oximux_settings::agent_retry::AgentRetrySettings;

// The sibling modules (`transcript`, `assemble`, …) all glob the parent for the
// shared gpui + view vocabulary; matching them keeps this file's imports from
// drifting out of step as that vocabulary moves.
use super::*;

/// A retry the app has committed to, holding the timer that will fire it.
///
/// Dropping this cancels the retry: the `Task` is aborted on drop, so Cancel,
/// a new user send, and closing the tab all need no extra teardown.
pub(super) struct PendingRetry {
    /// Unix ms the retry fires at — rendered as a countdown.
    pub wake_at_ms: i64,
    /// Short human reason ("Usage limit reached", "Provider overloaded").
    pub reason: String,
    /// Cancels on drop. Never read.
    pub _task: Task<()>,
}

/// Per-chat retry state.
#[derive(Default)]
pub(super) struct ChatRetry {
    /// Automatic attempts already spent on the turn currently being retried.
    ///
    /// Reset when the user sends something new or a turn succeeds — the cap is
    /// per turn, not per session, so a long conversation that hits a limit once
    /// an hour is not eventually locked out.
    pub attempt: u32,
    /// The armed retry, if any.
    pub pending: Option<PendingRetry>,
}

impl ChatRetry {
    /// Forget any armed retry and reset the attempt count. Called when the user
    /// takes over — a new send, or an explicit Cancel.
    pub fn clear(&mut self) {
        self.pending = None;
        self.attempt = 0;
    }

    /// Whether a retry is currently armed.
    pub fn is_armed(&self) -> bool {
        self.pending.is_some()
    }
}

/// The one-line reason shown on the card for a class.
pub(super) fn reason_for(class: oximux_agents::retry::RetryClass) -> String {
    use oximux_agents::retry::RetryClass;
    match class {
        RetryClass::Window { .. } => "Usage limit reached".into(),
        RetryClass::Overload => "Provider overloaded".into(),
        // Never armed, so never rendered; kept total rather than `unreachable!`
        // so a future class cannot panic the transcript.
        RetryClass::Spend | RetryClass::Other => "Turn failed".into(),
    }
}

impl AgentChatView {
    /// Decide whether the turn that just failed should be re-sent
    /// automatically, and arm a timer if so.
    ///
    /// Everything about *whether* and *when* is delegated to the pure policy in
    /// `oximux_agents::retry`; this method only supplies the inputs and holds
    /// the timer. Three conditions are checked here rather than there because
    /// they are properties of this view, not of the failure: an interrupted
    /// turn is one the user stopped, a disconnected child has nothing to send
    /// to, and a disabled setting means never.
    pub(super) fn arm_retry_if_limited(&mut self, cx: &mut Context<Self>) {
        if self.interrupted || self.disconnected {
            return;
        }
        let settings = cx
            .try_global::<AgentRetrySettings>()
            .copied()
            .unwrap_or_else(AgentRetrySettings::shipped);
        if !settings.enabled {
            return;
        }
        let class = classify_failure(
            self.thread.last_rate_limit.as_ref(),
            self.thread.last_turn_failure.clone(),
        );
        let now_ms = chrono::Utc::now().timestamp_millis();
        let policy =
            AgentRetryPolicy { max_automatic_wait: settings.max_automatic_wait.duration() };
        // The seed must differ per *thread*, not per call: threads on one
        // account see the same reset time, and a shared seed would give them
        // all the same jitter — the storm the jitter exists to prevent. The
        // session id is the stable per-thread value; the clock breaks ties
        // between two chats that somehow hash alike.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&self.remote_session_id, &mut hasher);
        std::hash::Hash::hash(&now_ms, &mut hasher);
        let seed = std::hash::Hasher::finish(&hasher);
        let Some(wake_at_ms) =
            oximux_agents::retry::schedule(class, self.retry.attempt, now_ms, &policy, seed)
        else {
            return;
        };
        // Clearing the error text is what swaps the error card for the queued
        // card: the transcript renders whichever is set.
        self.thread.last_error = None;
        let task = cx.spawn(async move |this, cx| {
            loop {
                let remaining = this
                    .update(cx, |view, _| {
                        view.retry.pending.as_ref().map(|p| p.wake_at_ms)
                    })
                    .ok()
                    .flatten()
                    .map(|wake| wake - chrono::Utc::now().timestamp_millis());
                let Some(remaining) = remaining else { return };
                if remaining <= 0 {
                    break;
                }
                // Repaint cadence, not polling: the card shows a countdown, and
                // an idle chat repaints for no other reason. Coarse while the
                // wait is long (the card reads "3h 10m"), per-second once it is
                // short enough for the seconds to be the thing being read.
                let step = if remaining <= 60_000 { 1_000 } else { 30_000 };
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(step.min(remaining) as u64))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |view, cx| {
                // Attempt is counted at fire time, not at arm time, so a retry
                // the user cancels costs nothing.
                view.retry.pending = None;
                view.retry.attempt += 1;
                view.retry_last_turn(cx);
            });
        });
        self.retry.pending = Some(PendingRetry {
            wake_at_ms,
            reason: reason_for(class),
            _task: task,
        });
        cx.notify();
    }

    /// Fire an armed retry immediately (the card's "Send now").
    pub(super) fn send_retry_now(&mut self, cx: &mut Context<Self>) {
        if self.retry.pending.take().is_none() {
            return;
        }
        self.retry.attempt += 1;
        self.retry_last_turn(cx);
    }

    /// Drop an armed retry (the card's "Cancel").
    ///
    /// Restores the error the retry replaced, so cancelling leaves the user
    /// looking at why the turn failed rather than at a blank tail.
    pub(super) fn cancel_retry(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.retry.pending.take() else { return };
        self.thread.last_error = Some(format!("{} — retry cancelled.", pending.reason));
        self.retry.attempt = 0;
        cx.notify();
    }

    /// "Send now" on the queued-retry card — fire the held turn immediately
    /// instead of waiting out the countdown.
    pub(super) fn retry_send_now_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo) = (self.theme, &self.typography);
        div()
            .id("chat-retry-send-now")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .bg(theme.status_warning.opacity(0.18))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_warning)
            .hover(|s| s.bg(theme.status_warning.opacity(0.3)))
            .child(SharedString::from("Send now"))
            .on_click(cx.listener(|this, _e, _window, cx| this.send_retry_now(cx)))
            .into_any_element()
    }

    /// "Cancel" on the queued-retry card — drop the held turn and show the
    /// failure it came from.
    pub(super) fn retry_cancel_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo) = (self.theme, &self.typography);
        div()
            .id("chat-retry-cancel")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_muted)
            .hover(|s| s.bg(theme.bg_panel_alt))
            .child(SharedString::from("Cancel"))
            .on_click(cx.listener(|this, _e, _window, cx| this.cancel_retry(cx)))
            .into_any_element()
    }

    /// Re-send the last user prompt after a turn ended in error (or the child
    /// crashed). Reachable only from the idle error / disconnected tail cards —
    /// gated on `!turn_active` so it never double-sends mid-turn. A crashed or
    /// stopped child is respawned (via `--resume`) before the prompt is
    /// retransmitted; the prompt bubble is already the tail entry, so it is NOT
    /// pushed again.
    pub(super) fn retry_last_turn(&mut self, cx: &mut Context<Self>) {
        if self.thread.turn_active {
            return; // a turn is already streaming — nothing to retry
        }
        // A crashed / stopped child can't receive input — bring it back first.
        if self.disconnected || self.interrupted {
            self.respawn(cx);
            if self.disconnected {
                // Respawn failed (e.g. the resume file is gone); `respawn` left
                // its own error text — keep the card rather than silently no-op.
                cx.notify();
                return;
            }
        }
        let last_user_idx = self
            .thread
            .entries
            .iter()
            .rposition(|e| matches!(e, ThreadEntry::User { .. }));
        let last_user = last_user_idx.and_then(|i| match &self.thread.entries[i] {
            ThreadEntry::User { text, images, .. } => Some((i, text.clone(), images.clone())),
            _ => None,
        });
        match last_user {
            Some((idx, text, images)) => {
                let sent = match &self.connection {
                    Some(conn) => match conn.send_user_message_with_images(&text, &images) {
                        Ok(()) => {
                            self.thread.last_error = None;
                            self.thread.turn_active = true;
                            true
                        }
                        Err(e) => {
                            self.thread.last_error = Some(format!("Send failed: {e}"));
                            false
                        }
                    },
                    None => false,
                };
                // Re-anchor the pre-turn checkpoint to the retried turn (as a
                // fresh send would), so the "restore files" rewind affordance
                // keeps tracking repo changes for it.
                if sent {
                    self.take_checkpoint_for(idx, cx);
                }
            }
            None => {
                // No prompt to replay (the connection failed before any turn) —
                // the respawn above already restored a working, error-free idle
                // state, so just drop the card.
                self.thread.last_error = None;
            }
        }
        self.follow_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// A small "Retry" control for the error / disconnected tail cards. Its
    /// click re-sends the last user prompt (respawning the child first if it
    /// crashed or was stopped).
    pub(super) fn retry_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typo = &self.typography;
        div()
            .id("chat-retry-turn")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .bg(theme.status_error.opacity(0.15))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_error)
            .hover(|s| s.bg(theme.status_error.opacity(0.28)))
            .child(
                Icon::default()
                    .path("icons/refresh-cw.svg")
                    .size(px(13.0))
                    .text_color(theme.status_error),
            )
            .child(SharedString::from("Retry"))
            .on_click(cx.listener(|this, _e, _window, cx| this.retry_last_turn(cx)))
    }

}
