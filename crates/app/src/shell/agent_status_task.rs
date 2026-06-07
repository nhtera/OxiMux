//! Per-agent-tab status watcher task.
//!
//! Spawned once per agent tab inside its owning `PaneGroup`. Single
//! consumer of `AgentStatusStream::changed()` drives BOTH the badge dot
//! repaint (via `cx.notify()` on the group) and macOS notification
//! dispatch. One watcher per tab; no shared state between tabs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui::{Context, SharedString, Task};
use oximux_agents::AgentStatusStream;
use oximux_core::AgentStatus;

use crate::notifier::{
    NotificationKind, Notifier, SuppressMap, TabId, notification_kind_for_transition,
};
use crate::shell::pane_group::PaneGroup;

/// Spawn a watcher on `status_rx` that
///  1. Wakes the parent `PaneGroup` on every status transition (badge repaint),
///  2. Fires `notifier.notify(...)` on genuine lifecycle edges
///     (`NeedsApproval` / `WaitingForInput` / `Done` / `Failed`) that pass the
///     per-(tab, kind) `SuppressMap` rate limit. The per-kind enable + focus
///     gate live inside the notifier's shared settings, so the task always
///     passes the current focus state and lets the notifier decide.
///
/// `window_active` is a shared `Arc<AtomicBool>` updated by the owning
/// `ProjectPanes` window-activation observer, so every group watcher
/// reads the same flag without holding a back-reference up the tree.
pub fn spawn_status_task(
    status_rx: AgentStatusStream,
    notifier: Arc<dyn Notifier>,
    window_active: Arc<AtomicBool>,
    tab_id: TabId,
    label: SharedString,
    cx: &mut Context<PaneGroup>,
) -> Task<()> {
    let mut status_rx = status_rx;
    cx.spawn(async move |weak, cx| {
        let mut prev_status: AgentStatus = status_rx.borrow_and_update().clone();
        let mut suppress = SuppressMap::new();
        loop {
            if status_rx.changed().await.is_err() {
                return;
            }
            let new_status: AgentStatus = status_rx.borrow_and_update().clone();
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
            // Fire on a genuine lifecycle edge; the notifier itself applies
            // the per-kind enable + focus gate from shared settings, so we
            // always pass the current focus state and let it decide.
            let window_active_now = window_active.load(Ordering::Relaxed);
            if let Some(kind) = notification_kind_for_transition(&prev_status, &new_status)
                && suppress.should_fire(tab_id, kind, Instant::now())
            {
                let body = match &new_status {
                    AgentStatus::NeedsApproval(reason) => reason.clone(),
                    AgentStatus::Failed(msg) => msg.clone(),
                    _ => String::new(),
                };
                notifier.notify(tab_id, kind, &label, &body, window_active_now);
            }
            // Reset dedupe when leaving a notify-worthy state so re-entry can
            // fire immediately rather than waiting out the rate-limit window.
            if let Some(prev_kind) = NotificationKind::from_status(&prev_status)
                && NotificationKind::from_status(&new_status) != Some(prev_kind)
            {
                suppress.forget(tab_id, prev_kind);
            }
            prev_status = new_status;
        }
    })
}
