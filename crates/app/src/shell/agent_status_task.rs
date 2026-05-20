//! Per-agent-tab status watcher task.
//!
//! Spawned once per agent tab in `WorkspaceTabs::push_agent_tab`. Single
//! consumer of `AgentStatusStream::changed()` drives BOTH the badge dot
//! repaint (via `cx.notify()`) and macOS notification dispatch. One
//! watcher per tab; no shared state between tabs.
//!
//! Lives in its own module so `workspace_tabs.rs` stays under the
//! file-size hard cap. The spawn closure is the only non-trivial caller
//! of the notifier dispatch path, so co-locating it with the badge code
//! does not improve cohesion — keep it where the wiring lives.

use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, SharedString, Task};
use oximux_agents::AgentStatusStream;
use oximux_core::AgentStatus;

use crate::notifier::{Notifier, SuppressMap, TabId, should_notify_transition};
use crate::shell::workspace_tabs::WorkspaceTabs;

/// Spawn a watcher on `status_rx` that
///  1. Wakes the parent entity on every status transition (badge repaint),
///  2. Calls `notifier.notify_needs_approval` on the first edge into
///     `NeedsApproval(_)` when the window is not active and the per-tab
///     `SuppressMap` has not fired within the rate-limit window.
///
/// Dropping the returned `Task` cancels the loop. `WorkspaceTab` holds
/// the task in `_status_task`, so close_tab's `tabs.remove(idx)` drop
/// cancels naturally — no explicit shutdown handshake required.
pub fn spawn_status_task(
    status_rx: AgentStatusStream,
    notifier: Arc<dyn Notifier>,
    tab_id: TabId,
    label: SharedString,
    cx: &mut Context<WorkspaceTabs>,
) -> Task<()> {
    let mut status_rx = status_rx;
    cx.spawn(async move |weak, cx| {
        // Mark the initial value as seen so `changed()` waits for the
        // first real transition. `watch::Sender::send` bumps the version
        // counter unconditionally; without `borrow_and_update` a future
        // duplicate send would double-fire `cx.notify()`.
        let mut prev_status: AgentStatus = status_rx.borrow_and_update().clone();
        let mut suppress = SuppressMap::new();
        loop {
            if status_rx.changed().await.is_err() {
                return;
            }
            let new_status: AgentStatus = status_rx.borrow_and_update().clone();
            // Coalesce notify + window_active read into one entity update
            // so the task crosses the GPUI boundary once per transition.
            let window_active = match weak.update(cx, |tabs, cx| {
                cx.notify();
                tabs.window_active()
            }) {
                Ok(v) => v,
                Err(_) => return,
            };
            if !window_active
                && should_notify_transition(&prev_status, &new_status)
                && let AgentStatus::NeedsApproval(reason) = &new_status
                && suppress.should_fire(tab_id, Instant::now())
            {
                notifier.notify_needs_approval(tab_id, &label, reason);
            }
            // Clear the dedupe window when the tab leaves NeedsApproval —
            // a fresh approval prompt arriving inside the original 30 s
            // window would otherwise be silently dropped (M2 review-260521).
            if matches!(prev_status, AgentStatus::NeedsApproval(_))
                && !matches!(new_status, AgentStatus::NeedsApproval(_))
            {
                suppress.forget(tab_id);
            }
            prev_status = new_status;
        }
    })
}
