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

use crate::notifier::{Notifier, SuppressMap, TabId, should_notify_transition};
use crate::shell::pane_group::PaneGroup;

/// Spawn a watcher on `status_rx` that
///  1. Wakes the parent `PaneGroup` on every status transition (badge repaint),
///  2. Calls `notifier.notify_needs_approval` on the first edge into
///     `NeedsApproval(_)` when the window is not active and the per-tab
///     `SuppressMap` has not fired within the rate-limit window.
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
            if weak.update(cx, |_group, cx| cx.notify()).is_err() {
                return;
            }
            let window_active_now = window_active.load(Ordering::Relaxed);
            if !window_active_now
                && should_notify_transition(&prev_status, &new_status)
                && let AgentStatus::NeedsApproval(reason) = &new_status
                && suppress.should_fire(tab_id, Instant::now())
            {
                notifier.notify_needs_approval(tab_id, &label, reason);
            }
            if matches!(prev_status, AgentStatus::NeedsApproval(_))
                && !matches!(new_status, AgentStatus::NeedsApproval(_))
            {
                suppress.forget(tab_id);
            }
            prev_status = new_status;
        }
    })
}
