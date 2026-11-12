//! Per-agent-tab status watcher task.
//!
//! Spawned once per agent tab inside its owning `PaneGroup`. Single
//! consumer of `AgentStatusStream::changed()` drives the badge dot repaint
//! (via `cx.notify()` on the group), the tab's attention ring, the in-app
//! toast and the keep-awake stake. One watcher per tab; no shared state
//! between tabs.
//!
//! **It no longer notifies.** OS banners come from the attention bridge,
//! which reads the host's one definition of "needs a person" and so knows
//! what this task cannot: whether the person has already dealt with the
//! thing. Deciding from a status alone re-announced a failure that had been
//! dismissed, and the 30-second window swallowed a second genuine request
//! inside it while letting the same one through just after. What stays here
//! is in-app and immediate: the ring and the toast are about the tab in
//! front of you, not about interrupting you elsewhere.

use std::pin::pin;
use std::time::Instant;

use futures::future::{Either, select};
use gpui::{Context, SharedString, Task, WeakEntity};
use oximux_agents::AgentStatusStream;
use oximux_core::AgentStatus;

use oximux_agents::attention::{AttentionKind, kind_of_agent_status};

use crate::agent_awake_lease::{
    AGENT_AWAKE_OUTPUT_RECHECK, AGENT_AWAKE_STALE_AFTER, AgentHoldLease,
};
use crate::notifier::TabId;
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_view::TerminalView;

/// Spawn a watcher on `status_rx` that
///  1. Wakes the parent `PaneGroup` on every status transition (badge repaint),
///  2. Raises the tab's attention ring when the agent starts wanting
///     something, and toasts a finish or a failure in-window,
///  3. Holds the agent-awake sleep assertion while the agent is `Running`
///     (RAII — a tab closed mid-run releases via task drop), capped by
///     [`AgentHoldLease`] once neither the status nor the PTY output has
///     moved for [`AGENT_AWAKE_STALE_AFTER`].
///
/// Both of (2)'s rules read `attention::kind_of_agent_status`, the same
/// mapping the rail and the host use, so the ring and the row cannot
/// disagree about what the agent is doing.
pub fn spawn_status_task(
    status_rx: AgentStatusStream,
    tab_id: TabId,
    label: SharedString,
    view: WeakEntity<TerminalView>,
    cx: &mut Context<PaneGroup>,
) -> Task<()> {
    let mut status_rx = status_rx;
    cx.spawn(async move |weak, cx| {
        let mut prev_status: AgentStatus = status_rx.borrow_and_update().status.clone();
        // Sleep-assertion stake for this agent; held while Running unless the
        // tab has gone silent past the cap. The agent may already be Running
        // at subscribe time (e.g. a watcher re-spawned for a respawned tab).
        let mut lease = AgentHoldLease::new(crate::agent_awake::global().clone(), Instant::now());
        lease.observe(matches!(prev_status, AgentStatus::Running), Instant::now());
        let mut stale_wait = AGENT_AWAKE_STALE_AFTER;
        loop {
            // Only a held stake needs the staleness timer; `changed()` is
            // cancel-safe, so losing the race to the timer drops nothing.
            let changed = if lease.holding() {
                let changed = pin!(status_rx.changed());
                let stale = pin!(cx.background_executor().timer(stale_wait));
                match select(changed, stale).await {
                    Either::Left((changed, _)) => {
                        stale_wait = AGENT_AWAKE_STALE_AFTER;
                        changed
                    }
                    Either::Right(_) => {
                        let last_output =
                            view.read_with(cx, |v, _| v.last_output_at()).ok().flatten();
                        match lease.on_deadline(last_output, Instant::now()) {
                            Some(remaining) => stale_wait = remaining,
                            None => {
                                // A later re-acquire starts a fresh cap.
                                stale_wait = AGENT_AWAKE_STALE_AFTER;
                                tracing::info!(
                                    ?tab_id,
                                    "agent keep-awake hold expired: no status or output for 2h"
                                );
                            }
                        }
                        continue;
                    }
                }
            } else if lease.expired() {
                // Expired while still Running: output alone may bring it back.
                let changed = pin!(status_rx.changed());
                let recheck = pin!(cx.background_executor().timer(AGENT_AWAKE_OUTPUT_RECHECK));
                match select(changed, recheck).await {
                    Either::Left((changed, _)) => changed,
                    Either::Right(_) => {
                        let last_output =
                            view.read_with(cx, |v, _| v.last_output_at()).ok().flatten();
                        if lease.on_output(last_output) {
                            tracing::info!(?tab_id, "agent keep-awake hold re-acquired: output resumed");
                        }
                        continue;
                    }
                }
            } else {
                status_rx.changed().await
            };
            if changed.is_err() {
                return;
            }
            let new_status: AgentStatus = status_rx.borrow_and_update().status.clone();
            // Use the label-change-aware notify: the agent status badge
            // appears/disappears with these transitions, which can change
            // chip width. The PaneGroup helper snapshots pin state +
            // schedules a one-frame-deferred re-pin so the active tab
            // stays anchored to the strip's right edge.
            if weak
                .update(cx, |group, cx| group.notify_with_label_change_check(cx))
                .is_err()
            {
                return;
            }
            // Track the Running stake on every transition, not just
            // notify-worthy edges (Running itself is a transient state the
            // edge detector ignores).
            lease.observe(matches!(new_status, AgentStatus::Running), Instant::now());
            // One edge per genuine change of what the agent wants, by the
            // one definition. A repeat — an adapter re-printing its prompt on
            // every output chunk — is the same kind and says nothing.
            if let Some(kind) = wanted_edge(&prev_status, &new_status) {
                // Attention ring for the kinds that block the agent. Its own
                // channel, not the OS banner's: the ring is about a tab the
                // person can switch to, so it lights up even when banners are
                // muted, and no focus gate has a say in it.
                if kind.is_needs_you() {
                    let _ = view.update(cx, |v, cx| v.raise_agent_attention(cx));
                }
                // In-app toast for the lifecycle endpoints, so the person
                // sees them inside the active window too — which is exactly
                // where the OS banner is suppressed.
                if let Some((tk, text)) = toast_for_kind(kind, &label) {
                    let _ = weak.update(cx, |_group, cx| {
                        crate::shell::toast::toast(cx, tk, text);
                    });
                }
            }

            prev_status = new_status;
        }
    })
}

/// What this status change means, or `None` when it means nothing new.
///
/// `completed_turn` is `false` because this task has no reply to read: an
/// `Idle` agent here is parked, and whether it left a reply unread is the
/// rail's question, not the ring's. Pure, so the edge rule is testable.
fn wanted_edge(prev: &AgentStatus, new: &AgentStatus) -> Option<AttentionKind> {
    let kind = kind_of_agent_status(new, false).map(|(kind, _)| kind)?;
    if kind_of_agent_status(prev, false).map(|(prev, _)| prev) == Some(kind) {
        return None;
    }
    Some(kind)
}

/// In-app toast for a terminal agent edge. Only the genuine lifecycle
/// endpoints (finished / failed) get a toast — a prompt raises the attention
/// ring instead, so toasting it too would be noise. Pure so the mapping is
/// unit-testable without GPUI.
fn toast_for_kind(
    kind: AttentionKind,
    label: &str,
) -> Option<(crate::shell::toast::ToastKind, String)> {
    match kind {
        AttentionKind::FinishedUnread => Some((
            crate::shell::toast::ToastKind::Success,
            format!("{label} finished"),
        )),
        AttentionKind::Failed => Some((
            crate::shell::toast::ToastKind::Error,
            format!("{label} failed"),
        )),
        // A prompt raises the ring; a stall or a rate limit is not an event
        // the window should announce — the row says it.
        AttentionKind::NeedsPermission
        | AttentionKind::NeedsInput
        | AttentionKind::Stalled
        | AttentionKind::Limited => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::toast::ToastKind;

    #[test]
    fn toast_only_for_a_finish_and_a_failure() {
        assert!(matches!(
            toast_for_kind(AttentionKind::FinishedUnread, "Claude"),
            Some((ToastKind::Success, _))
        ));
        assert!(matches!(
            toast_for_kind(AttentionKind::Failed, "Claude"),
            Some((ToastKind::Error, _))
        ));
        assert!(toast_for_kind(AttentionKind::NeedsPermission, "Claude").is_none());
        assert!(toast_for_kind(AttentionKind::NeedsInput, "Claude").is_none());
        assert!(toast_for_kind(AttentionKind::Stalled, "Claude").is_none());
    }

    #[test]
    fn one_edge_per_genuine_change_of_what_the_agent_wants() {
        let approval = |what: &str| AgentStatus::NeedsApproval(what.into());
        assert_eq!(
            wanted_edge(&AgentStatus::Idle, &approval("write?")),
            Some(AttentionKind::NeedsPermission)
        );
        assert_eq!(
            wanted_edge(&approval("first?"), &approval("second?")),
            None,
            "an adapter re-printing its prompt is not a new edge for the ring"
        );
        assert_eq!(wanted_edge(&AgentStatus::Idle, &AgentStatus::Running), None);
        assert_eq!(
            wanted_edge(&AgentStatus::Running, &AgentStatus::Done { code: Some(0) }),
            Some(AttentionKind::FinishedUnread)
        );
        assert_eq!(
            wanted_edge(&AgentStatus::Running, &AgentStatus::Done { code: Some(2) }),
            Some(AttentionKind::Failed),
            "a non-zero exit is a failure, which is what the rail says too"
        );
        assert_eq!(
            wanted_edge(&approval("a"), &AgentStatus::Running),
            None,
            "going back to work wants nothing"
        );
    }
}
