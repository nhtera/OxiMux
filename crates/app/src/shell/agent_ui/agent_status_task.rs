//! Per-agent-tab status watcher task.
//!
//! Spawned once per agent tab inside its owning `PaneGroup`. Single
//! consumer of `AgentStatusStream::changed()` drives BOTH the badge dot
//! repaint (via `cx.notify()` on the group) and macOS notification
//! dispatch. One watcher per tab; no shared state between tabs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use gpui::{Context, SharedString, Task, WeakEntity};
use oximux_agents::AgentStatusStream;
use oximux_core::AgentStatus;

use crate::notifier::{
    NotificationKind, NotificationRequest, NotificationSource, Notifier, SuppressMap, TabId,
    notification_kind_for_transition,
};
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_view::TerminalView;

/// Spawn a watcher on `status_rx` that
///  1. Wakes the parent `PaneGroup` on every status transition (badge repaint),
///  2. Fires `notifier.notify(...)` on genuine lifecycle edges
///     (`NeedsApproval` / `WaitingForInput` / `Done` / `Failed`) that pass the
///     per-(tab, kind) `SuppressMap` rate limit. The per-kind enable + focus
///     gate live inside the notifier's shared settings, so the task always
///     passes the current focus + visibility state and lets the notifier
///     decide,
///  3. Holds the agent-awake sleep assertion while the agent is `Running`
///     (RAII — a tab closed mid-run releases via task drop).
///
/// `window_active` is a shared `Arc<AtomicBool>` updated by the owning
/// `ProjectPanes` window-activation observer, so every group watcher
/// reads the same flag without holding a back-reference up the tree.
/// `workspace_key` is the agent's worktree path — the notifier's burst
/// gate collapses same-workspace banners across tabs with it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_status_task(
    status_rx: AgentStatusStream,
    notifier: Arc<dyn Notifier>,
    window_active: Arc<AtomicBool>,
    tab_id: TabId,
    label: SharedString,
    workspace_key: String,
    view: WeakEntity<TerminalView>,
    cx: &mut Context<PaneGroup>,
) -> Task<()> {
    let mut status_rx = status_rx;
    cx.spawn(async move |weak, cx| {
        let mut prev_status: AgentStatus = status_rx.borrow_and_update().status.clone();
        let mut suppress = SuppressMap::new();
        // Sleep-assertion stake for this agent; held exactly while Running.
        // The agent may already be Running at subscribe time (e.g. a watcher
        // re-spawned for a respawned tab).
        let mut awake_hold = matches!(prev_status, AgentStatus::Running)
            .then(|| crate::agent_awake::global().acquire());
        loop {
            if status_rx.changed().await.is_err() {
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
            match (&awake_hold, matches!(new_status, AgentStatus::Running)) {
                (None, true) => awake_hold = Some(crate::agent_awake::global().acquire()),
                (Some(_), false) => awake_hold = None,
                _ => {}
            }
            // Fire on a genuine lifecycle edge; the notifier itself applies
            // the per-kind enable + focus gate from shared settings, so we
            // always pass the current focus state and let it decide.
            let window_active_now = window_active.load(Ordering::Relaxed);
            if let Some(kind) = notification_kind_for_transition(&prev_status, &new_status) {
                // Attention ring for action-required edges. Separate channel
                // from the OS banner: independent of the banner rate-limit +
                // focus gate (the view raises only when its pane is unfocused),
                // so a backgrounded agent tab lights up even when banners are
                // muted or suppressed.
                if matches!(
                    kind,
                    NotificationKind::NeedsApproval | NotificationKind::WaitingForInput
                ) {
                    let _ = view.update(cx, |v, cx| v.raise_agent_attention(cx));
                }
                if suppress.should_fire(tab_id, kind, Instant::now()) {
                    let body = match &new_status {
                        AgentStatus::NeedsApproval(reason) => reason.clone(),
                        AgentStatus::Failed(msg) => msg.clone(),
                        _ => String::new(),
                    };
                    // A pane the user can currently see never banners; the
                    // visibility sweep keeps this flag current for hidden
                    // tabs and inactive projects alike.
                    let pane_visible = view
                        .read_with(cx, |v, _| v.is_visible())
                        .unwrap_or(false);
                    notifier.notify(NotificationRequest {
                        source: NotificationSource::AgentState,
                        kind,
                        tab_id,
                        workspace_key: workspace_key.clone(),
                        label: label.to_string(),
                        body,
                        window_active: window_active_now,
                        pane_visible,
                    });
                }
                // In-app toast for terminal lifecycle edges (finished /
                // failed), independent of the OS banner's focus gate so the
                // user also sees it inside the active window. Deliberately
                // outside the `should_fire` rate limiter: the transition
                // detector already yields one edge per genuine state change, so
                // the toast can't spam, and it serves as the in-window fallback
                // exactly when the banner is being suppressed.
                if let Some((tk, text)) = toast_for_kind(kind, &label) {
                    let _ = weak.update(cx, |_group, cx| {
                        crate::shell::toast::toast(cx, tk, text);
                    });
                }
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

/// In-app toast for a terminal agent edge. Only the genuine lifecycle
/// endpoints (finished / failed) get a toast — approval / input edges already
/// raise the attention ring and OS banner, so toasting them too would be
/// noise. Pure so the mapping is unit-testable without GPUI.
fn toast_for_kind(
    kind: NotificationKind,
    label: &str,
) -> Option<(crate::shell::toast::ToastKind, String)> {
    match kind {
        NotificationKind::Done => Some((
            crate::shell::toast::ToastKind::Success,
            format!("{label} finished"),
        )),
        NotificationKind::Failed => Some((
            crate::shell::toast::ToastKind::Error,
            format!("{label} failed"),
        )),
        NotificationKind::NeedsApproval | NotificationKind::WaitingForInput => None,
        // Bell never reaches this mapper (it's not an agent lifecycle
        // edge); listed for exhaustiveness.
        NotificationKind::Bell => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::toast::ToastKind;

    #[test]
    fn toast_only_for_done_and_failed() {
        assert!(matches!(
            toast_for_kind(NotificationKind::Done, "Claude"),
            Some((ToastKind::Success, _))
        ));
        assert!(matches!(
            toast_for_kind(NotificationKind::Failed, "Claude"),
            Some((ToastKind::Error, _))
        ));
        assert!(toast_for_kind(NotificationKind::NeedsApproval, "Claude").is_none());
        assert!(toast_for_kind(NotificationKind::WaitingForInput, "Claude").is_none());
    }
}
