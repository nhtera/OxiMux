//! Rate-limits the remote transcript snapshot.
//!
//! [`AgentChatView::publish_remote_transcript`] serializes the *whole* fold, and
//! it used to run on every settled batch — so a turn that made twenty tool
//! calls re-serialized the entire conversation twenty times, each pass O(all
//! entries ever). In a long chat that is the dominant cost on the settled path.
//!
//! The obvious fix is not the right one. A revision gate — the shape
//! persistence uses, comparing `ChatThread::revision()` against the last
//! published value — cannot skip anything here: `ChatThread::apply` bumps the
//! revision unconditionally on entry (deliberately, "an event that folds to
//! nothing still bumps"), and this path is reached only after a non-empty
//! non-delta batch has been applied. Every call therefore arrives at a fresh
//! revision. The gate would compile, read as a fix, and never once skip.
//!
//! What actually removes the work is coalescing, and the design already permits
//! it. The snapshot exists so a client *opening* the session sees full history;
//! one that opens mid-stream folds live deltas from the backlog on top of the
//! last settled snapshot. That contract tolerates a snapshot a moment behind —
//! so publish at most once per [`PUBLISH_INTERVAL`], with a trailing publish so
//! staleness stays bounded even when the events stop.
//!
//! Bind and rekey deliberately do NOT come through here. A freshly bound peer
//! has no snapshot at all, so it needs one immediately rather than up to an
//! interval later; those sites call [`AgentChatView::publish_remote_transcript`]
//! directly.
//!
//! Same shape as `notify_throttled` one screen up, on purpose — leading edge,
//! single queued trailing call, flag cleared by whoever fires.

use std::cell::Cell;
use std::time::{Duration, Instant};

use gpui::Context;

use super::AgentChatView;

/// Longest a remote peer's snapshot may lag the fold.
///
/// Five times the repaint throttle: a human watching the desktop needs 50ms
/// smoothness, whereas this only has to be current by the time someone opens
/// the session on a phone. Larger would coalesce more; this is already well
/// past the point where a tool-heavy turn collapses to a handful of publishes.
pub(super) const PUBLISH_INTERVAL: Duration = Duration::from_millis(250);

/// Leading-edge throttle state for the transcript snapshot.
///
/// `Cell`, matching `last_saved_revision` next door: the publish path runs from
/// `&self` in places and none of this is shared across threads.
pub(super) struct PublishThrottle {
    last: Cell<Instant>,
    /// A trailing publish is queued. Guards against stacking one timer per
    /// settled batch, and is cleared by whichever publish actually runs.
    scheduled: Cell<bool>,
}

impl PublishThrottle {
    pub(super) fn new() -> Self {
        // `now`, not "the epoch": seeding this far in the past would make the
        // first settled batch of every session publish on the leading edge and
        // then a second time on the trailing one.
        Self { last: Cell::new(Instant::now()), scheduled: Cell::new(false) }
    }
}

impl AgentChatView {
    /// Publish the folded transcript to the registry so a remote client opening
    /// this session renders full history.
    ///
    /// Immediate and ungated — call it from bind/rekey, where a peer is waiting
    /// on a snapshot it does not have. The settled-batch path wants
    /// [`Self::publish_remote_transcript_throttled`] instead.
    ///
    /// No-op when remote is disabled, or when the transcript will not serialize
    /// (a half-serialized snapshot is worse than keeping the prior one).
    pub(super) fn publish_remote_transcript(&self) {
        let Some(binding) = &self.remote else {
            return;
        };
        let Ok(entries_json) = serde_json::to_string(&self.thread.entries) else {
            return;
        };
        let model = self.thread.model.clone().or_else(|| self.model.clone());
        binding.publish_transcript(entries_json, model);
        // After the serialize, not before: a publish that bailed on an
        // unserializable transcript has not refreshed anything, and must not
        // start an interval as though it had.
        self.publish_throttle.last.set(Instant::now());
        self.publish_throttle.scheduled.set(false);
    }

    /// Publish at most once per [`PUBLISH_INTERVAL`], with a trailing publish so
    /// the snapshot still settles when the events stop.
    pub(super) fn publish_remote_transcript_throttled(&mut self, cx: &mut Context<Self>) {
        // Before any bookkeeping. With remote control off — the default —
        // `remote` is `None` and this whole path must cost nothing; starting an
        // interval or spawning a timer for a publish that will early-return is
        // work on behalf of a peer that does not exist.
        if self.remote.is_none() {
            return;
        }
        let since = self.publish_throttle.last.get().elapsed();
        if since >= PUBLISH_INTERVAL {
            self.publish_remote_transcript();
            return;
        }
        if self.publish_throttle.scheduled.get() {
            return; // a trailing publish is already queued; it will carry this too
        }
        self.publish_throttle.scheduled.set(true);
        let delay = PUBLISH_INTERVAL.saturating_sub(since);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |view, _cx| {
                // Cleared if a publish happened in the meantime — that publish
                // already carried these entries.
                if view.publish_throttle.scheduled.get() {
                    view.publish_remote_transcript();
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use oximux_settings::{Density, Theme, Typography};

    use super::super::AgentChatView;
    use super::*;

    /// The property the whole throttle has to preserve: a burst of settled
    /// events coalesces into one publish, and the snapshot still converges on
    /// the final state once the interval elapses.
    ///
    /// Both halves matter and neither alone is enough. Without the first
    /// assertion a per-event publish would pass — which is the bug. Without the
    /// second, a throttle that dropped the trailing publish would pass, and a
    /// remote peer would sit on a transcript permanently missing the tail of
    /// every burst. That second failure is the dangerous one: it looks like a
    /// sync bug in the phone client, far from this file.
    #[gpui::test]
    async fn a_burst_coalesces_and_still_converges(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = crate::remote_control::RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
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

        let session = window
            .update(cx, |view, _window, _cx| view.remote_session_id().to_string())
            .unwrap();

        let published = |cx: &mut TestAppContext| -> String {
            cx.update(|cx| {
                cx.global::<crate::remote_control::RemoteControl>()
                    .registry()
                    .get(&session)
                    .and_then(|h| h.transcript_snapshot())
                    .map(|s| s.entries_json)
                    .unwrap_or_default()
            })
        };

        // A burst, all inside one interval. The view was constructed moments
        // ago, so the throttle is mid-interval and none of these may publish on
        // the leading edge.
        window
            .update(cx, |view, _window, cx| {
                view.on_event(ThreadEvent::AssistantText("first".into()), cx);
                view.on_event(ThreadEvent::AssistantText("second".into()), cx);
                view.on_event(ThreadEvent::AssistantText("third".into()), cx);
            })
            .unwrap();
        cx.run_until_parked();

        let mid_burst = published(cx);
        assert!(
            !mid_burst.contains("third"),
            "a settled batch inside the interval must not re-serialize the fold; got {mid_burst}",
        );

        // The trailing publish fires on its own, with no further events.
        cx.executor().advance_clock(PUBLISH_INTERVAL * 2);
        cx.run_until_parked();

        let settled = published(cx);
        for text in ["first", "second", "third"] {
            assert!(
                settled.contains(text),
                "the trailing publish must carry the whole burst; {text} missing from {settled}",
            );
        }
    }

    /// Bind does not go through the throttle, and must not: a peer that has
    /// just bound holds no snapshot at all, so making it wait up to an interval
    /// would show it an empty conversation. Asserted by binding a view whose
    /// fold already has history — the restored-after-restart shape — and
    /// requiring the transcript to be there immediately.
    #[gpui::test]
    async fn binding_publishes_immediately(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = crate::remote_control::RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
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

        let session = window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("restored history");
                // No clock advance anywhere in this test: whatever bind
                // publishes has to be visible on the spot.
                view.bind_remote(cx);
                view.remote_session_id().to_string()
            })
            .unwrap();

        let snapshot = cx.update(|cx| {
            cx.global::<crate::remote_control::RemoteControl>()
                .registry()
                .get(&session)
                .and_then(|h| h.transcript_snapshot())
                .map(|s| s.entries_json)
                .unwrap_or_default()
        });
        assert!(
            snapshot.contains("restored history"),
            "bind must publish without waiting for the throttle; got {snapshot}",
        );
    }

    /// The leading edge fires, and the interval that follows is measured from
    /// the publish rather than from the request that triggered it.
    #[test]
    fn first_call_is_on_the_leading_edge() {
        let t = PublishThrottle::new();
        t.last.set(Instant::now() - PUBLISH_INTERVAL * 2);
        assert!(t.last.get().elapsed() >= PUBLISH_INTERVAL, "an idle throttle admits at once");
    }

    /// A fresh throttle must NOT admit immediately. Seeding `last` at the epoch
    /// would make every session's first settled batch publish twice — once on
    /// the leading edge, once on the trailing timer it also armed.
    #[test]
    fn a_fresh_throttle_does_not_admit_immediately() {
        let t = PublishThrottle::new();
        assert!(
            t.last.get().elapsed() < PUBLISH_INTERVAL,
            "a just-constructed throttle is inside its interval"
        );
        assert!(!t.scheduled.get(), "and has nothing queued");
    }

    /// One queued trailing publish, however many batches land inside the
    /// interval — this is the flag that stops a timer stacking per batch.
    #[test]
    fn only_one_trailing_publish_is_queued() {
        let t = PublishThrottle::new();
        let mut spawned = 0;
        for _ in 0..10 {
            if !t.scheduled.get() {
                t.scheduled.set(true);
                spawned += 1;
            }
        }
        assert_eq!(spawned, 1, "ten settled batches arm one timer");
    }
}
