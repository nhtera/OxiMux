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

use crate::persisted_chat::PersistedRetry;

// The sibling modules (`transcript`, `assemble`, …) all glob the parent for the
// shared gpui + view vocabulary; matching them keeps this file's imports from
// drifting out of step as that vocabulary moves.
use super::*;

/// How far past its wake time a restored retry may be and still fire.
///
/// Sized for "quit the laptop lid, reopened it a few minutes later", not for
/// "came back the next morning". A retry whose reset passed while the app was
/// closed for longer than this is dropped: the turn is still there and the
/// manual Retry button still works, but nothing sends on the user's behalf for
/// a limit that expired without them.
const RESTORE_GRACE_MS: i64 = 5 * 60 * 1000;

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

    /// The armed retry as the three durable fields the transcript blob keeps.
    /// `None` when nothing is armed — the timer itself is never persisted.
    pub fn persisted(&self) -> Option<PersistedRetry> {
        self.pending.as_ref().map(|p| PersistedRetry {
            wake_at_ms: p.wake_at_ms,
            reason: p.reason.clone(),
            attempt: self.attempt,
        })
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
        self.hold_turn_until(wake_at_ms, reason_for(class), cx);
    }

    /// Arm the countdown that fires `retry_last_turn` at `wake_at_ms`.
    ///
    /// Shared by the live arming path and by restore, so a retry rebuilt from a
    /// blob counts down, repaints and fires by exactly the same code as one
    /// armed a moment ago — the two cannot drift into disagreeing about the
    /// edge cases (a wake time already past, a view closed mid-wait).
    fn hold_turn_until(&mut self, wake_at_ms: i64, reason: String, cx: &mut Context<Self>) {
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
                view.mark_retry_dirty();
                view.retry_last_turn(cx);
            });
        });
        self.retry.pending = Some(PendingRetry { wake_at_ms, reason, _task: task });
        self.mark_retry_dirty();
        cx.notify();
    }

    /// Flag the blob as out of date because the *armed retry* changed.
    ///
    /// Needed because the retry lives on the view, not in `ChatThread`, so it
    /// does not move the thread's revision counter — and the save path skips a
    /// chat whose revision has not moved. Without this an armed retry would be
    /// dropped by the very save that was supposed to carry it across a restart.
    fn mark_retry_dirty(&self) {
        self.meta_dirty.set(true);
    }

    /// Rebuild an armed retry from a restored transcript blob.
    ///
    /// Three things can refuse the rebuild, and each is a case where firing
    /// would be wrong rather than merely late:
    ///
    /// * auto-retry has since been switched off — the setting is read now, not
    ///   at the time the retry was armed;
    /// * the wake time is further past than [`RESTORE_GRACE_MS`] — the app was
    ///   closed across the whole window, and re-sending a turn the user last
    ///   saw hours or days ago is a surprise, not a resumption;
    /// * the remaining wait exceeds the current `max_automatic_wait` — the user
    ///   has since shortened how long OxiMux may hold a turn.
    ///
    /// The attempt count is restored with it, so the cap of four counts the
    /// attempts a turn has actually cost rather than resetting on every launch.
    pub fn restore_pending_retry(&mut self, saved: PersistedRetry, cx: &mut Context<Self>) {
        let settings = cx
            .try_global::<AgentRetrySettings>()
            .copied()
            .unwrap_or_else(AgentRetrySettings::shipped);
        if !settings.enabled {
            return;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remaining = saved.wake_at_ms - now_ms;
        if remaining < -RESTORE_GRACE_MS {
            return;
        }
        // `None` is "No limit" — nothing to exceed.
        if let Some(max) = settings.max_automatic_wait.duration()
            && remaining > max.as_millis() as i64
        {
            return;
        }
        self.retry.attempt = saved.attempt;
        // The blob is the on-disk state until something changes, and a restored
        // retry is not a change — `hold_turn_until` marks the chat dirty, so
        // clear that again below to keep a restored-but-untouched chat clean.
        let was_dirty = self.meta_dirty.get();
        self.hold_turn_until(saved.wake_at_ms, saved.reason, cx);
        self.meta_dirty.set(was_dirty);
    }

    /// Fire an armed retry immediately (the card's "Send now").
    pub(super) fn send_retry_now(&mut self, cx: &mut Context<Self>) {
        if self.retry.pending.take().is_none() {
            return;
        }
        self.retry.attempt += 1;
        self.mark_retry_dirty();
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
        self.mark_retry_dirty();
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
        // A restored retry can fire on a tab that has never been rendered — a
        // background chat whose reset simply came up. `ensure_connected` is what
        // consumes the dormant mark, so it has to run here rather than being
        // left to the first render: respawning directly would leave the mark
        // set, and that render would then reap this turn's fresh child and
        // spawn a second one on top of the turn the retry had just started.
        if self.dormant {
            self.ensure_connected(false, cx);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{
        ChatBackend, SessionMeta, StubConnection, ThreadEntry, Transport,
    };
    use oximux_settings::agent_retry::MaxAutomaticWait;
    use oximux_settings::{Density, Theme, Typography};

    use super::super::AgentChatView;
    use super::*;

    fn saved(offset_ms: i64, attempt: u32) -> PersistedRetry {
        PersistedRetry {
            wake_at_ms: chrono::Utc::now().timestamp_millis() + offset_ms,
            reason: "Usage limit reached".into(),
            attempt,
        }
    }

    /// Build a chat view with a stub backend and the given retry settings in
    /// place, then hand it to `f`. The settings are set as a global because
    /// that is where `restore_pending_retry` reads them from — deliberately at
    /// restore time rather than from the blob, so a user who switched the
    /// feature off while the app was closed is obeyed.
    async fn with_view<F>(cx: &mut TestAppContext, settings: AgentRetrySettings, f: F)
    where
        F: FnOnce(&mut AgentChatView, &mut Context<AgentChatView>),
    {
        cx.update(gpui_component::init);
        cx.update(|cx| cx.set_global(settings));
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
        window.update(cx, |view, _window, cx| f(view, cx)).expect("window update");
    }

    /// The point of persisting a retry at all: a five-hour window outlives the
    /// app session that hit it, so the held turn must come back armed.
    #[gpui::test]
    async fn a_retry_whose_reset_is_still_ahead_comes_back_armed(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.restore_pending_retry(saved(60_000, 2), cx);
            assert!(view.retry.is_armed(), "a reset still ahead must re-arm");
            // The attempt count rides along, so the cap of four counts what the
            // turn has actually cost. Restoring it as 0 would let a chat that
            // relaunched between attempts exceed the cap without ever showing a
            // fifth attempt in any single session.
            assert_eq!(view.retry.attempt, 2, "attempts spent must survive the restart");
        })
        .await;
    }

    /// The half that keeps this feature from being a surprise: an app closed
    /// across the whole window must not wake up and send on the user's behalf
    /// for a limit that expired hours ago. The turn is still there and the
    /// manual Retry button still works — nothing fires by itself.
    #[gpui::test]
    async fn a_retry_the_app_slept_through_is_dropped(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.restore_pending_retry(saved(-60 * 60 * 1000, 1), cx);
            assert!(!view.retry.is_armed(), "a long-past reset must not fire on launch");
            assert_eq!(view.retry.attempt, 0, "a dropped retry spends nothing");
        })
        .await;
    }

    /// Just inside the grace window — a lid closed over the reset, not a night
    /// away — still fires. This is the boundary the test above only bounds from
    /// the other side; without it, `RESTORE_GRACE_MS` could be zero and both
    /// the drop test and this one would still look satisfied by one of them.
    #[gpui::test]
    async fn a_reset_that_passed_moments_ago_still_fires(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.restore_pending_retry(saved(-30_000, 0), cx);
            assert!(view.retry.is_armed(), "just past the reset is late, not stale");
        })
        .await;
    }

    /// The setting is read at restore, not baked into the blob: switching the
    /// feature off must take effect on the next launch for retries armed before
    /// it was switched off.
    #[gpui::test]
    async fn a_disabled_setting_refuses_the_rebuild(cx: &mut TestAppContext) {
        let off = AgentRetrySettings { enabled: false, ..AgentRetrySettings::shipped() };
        with_view(cx, off, |view, cx| {
            view.restore_pending_retry(saved(60_000, 0), cx);
            assert!(!view.retry.is_armed(), "auto-retry off means nothing re-arms");
        })
        .await;
    }

    /// Shortening `max_automatic_wait` while the app was closed also applies:
    /// a seven-day reset armed under "No limit" must not be honoured by a
    /// session that now allows at most six hours.
    #[gpui::test]
    async fn a_wait_longer_than_the_current_setting_is_refused(cx: &mut TestAppContext) {
        let short =
            AgentRetrySettings { enabled: true, max_automatic_wait: MaxAutomaticWait::SixHours };
        with_view(cx, short, |view, cx| {
            view.restore_pending_retry(saved(7 * 24 * 60 * 60 * 1000, 0), cx);
            assert!(!view.retry.is_armed(), "a wait past the current cap must be refused");
        })
        .await;
    }

    /// A restore is not a change: the blob a chat was just built from is still
    /// the on-disk state, so a quit immediately after launch must not rewrite
    /// every restored transcript.
    #[gpui::test]
    async fn restoring_a_retry_leaves_the_blob_clean(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.last_saved_revision.set(view.thread.revision());
            view.meta_dirty.set(false);
            view.restore_pending_retry(saved(60_000, 1), cx);
            assert!(view.retry.is_armed());
            assert!(!view.transcript_out_of_date(), "a restored retry is not a mutation");
        })
        .await;
    }

    /// Arming one *is* a change, though — the retry lives on the view, not in
    /// `ChatThread`, so it moves no revision counter. Without an explicit dirty
    /// mark the save path would skip the very chat holding the retry, and the
    /// blob written at quit would not contain it.
    #[gpui::test]
    async fn arming_a_retry_marks_the_blob_for_saving(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.thread.session_id = Some("sid-retry".into());
            view.thread.push_user_message("hello");
            view.last_saved_revision.set(view.thread.revision());
            view.meta_dirty.set(false);
            view.hold_turn_until(
                chrono::Utc::now().timestamp_millis() + 60_000,
                "Usage limit reached".into(),
                cx,
            );
            assert!(view.transcript_out_of_date(), "an armed retry must reach the next save");
            let blob = view.transcript_snapshot().expect("persistable transcript");
            let held = blob.pending_retry.expect("armed retry rides the blob");
            assert_eq!(held.reason, "Usage limit reached");
            assert_eq!(held.attempt, 0, "nothing has fired yet");
        })
        .await;
    }

    /// A restored retry can come due on a tab the user never opened, and firing
    /// it is the whole point — but the chat is still marked dormant, and the
    /// dormant mark is what the first render uses to decide it must connect.
    /// Firing without consuming it means that render reaps the child this retry
    /// just spawned and starts another on top of the in-flight turn.
    ///
    /// The ACP backend carries an empty command so the connect refuses
    /// synchronously: the assertion is about the mark being consumed, and this
    /// keeps the test from spawning a process to prove it.
    #[gpui::test]
    async fn a_retry_firing_before_first_render_consumes_the_dormant_mark(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        cx.update(|cx| cx.set_global(AgentRetrySettings::shipped()));
        let backend = ChatBackend {
            transport: Transport::Acp,
            acp_command: Some(String::new()),
            acp_args: Vec::new(),
            env: Vec::new(),
            adapter_id: None,
            profile: None,
        };
        let window = cx.add_window(|window, cx| {
            let mut view = AgentChatView::new_resumed(
                std::env::temp_dir(),
                None,
                backend,
                Some("sid-retry-dormant".into()),
                vec![ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None }],
                Vec::new(),
                SessionMeta::default(),
                super::super::ThinkingLevel::default(),
                super::super::RestoredPosture::default(),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            );
            assert!(view.dormant, "a restored chat starts dormant");
            view.retry_last_turn(cx);
            assert!(
                !view.dormant,
                "a retry that connects must consume the mark, or the first \
                 render spawns a second child over this turn"
            );
            view
        });
        cx.run_until_parked();
        window.update(cx, |_, _, _| {}).expect("window update");
    }

    /// A chat with nothing armed writes no retry, so a blob can never resurrect
    /// one the user cancelled.
    #[gpui::test]
    async fn a_cancelled_retry_leaves_nothing_in_the_blob(cx: &mut TestAppContext) {
        with_view(cx, AgentRetrySettings::shipped(), |view, cx| {
            view.thread.session_id = Some("sid-cancel".into());
            view.thread.push_user_message("hello");
            view.restore_pending_retry(saved(60_000, 1), cx);
            view.cancel_retry(cx);
            let blob = view.transcript_snapshot().expect("persistable transcript");
            assert!(blob.pending_retry.is_none(), "a cancelled retry must not persist");
            assert!(view.transcript_out_of_date(), "the cancellation must reach disk");
        })
        .await;
    }
}
