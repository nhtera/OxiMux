//! Cockpit agent tabs on a cold restore: the marker prefill and the resume
//! fallback swap. Split out of `tabs.rs` (which sits near the file-size cap)
//! and kept to the two operations the restore path needs.

use super::*;
use crate::relay_cold_restore::{RestoreMarker, marker};

impl PaneGroup {
    /// Prefill the agent tab at insertion index `idx` with a restore marker.
    /// Called in the same update closure that mounted the tab, so it lands as
    /// early as the mount allows. The grid takes bytes as the backend pumps
    /// them, so a CLI that has already painted before the mount would show
    /// the marker inside its first frame until its next repaint; in practice
    /// the spawn-to-mount gap is milliseconds and a CLI's first frame is
    /// hundreds of milliseconds out.
    /// The pane also arms the marker's off-grid notice, for a CLI that wipes
    /// the scrollback on start-up (see `terminal_view::restore_notice`).
    pub(crate) fn prefill_agent_tab(&self, idx: usize, kind: RestoreMarker, cx: &mut App) {
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        if let PaneContent::Terminal(tree) = &tab.content
            && let Some(view) = tree.active_view()
        {
            view.update(cx, |v, _| {
                v.prefill_grid(marker(kind));
                v.arm_restore_notice(kind.label());
            });
        }
    }

    /// Swap the CLI session behind the agent tab holding `old` for `new`: the
    /// resume fallback, when a restored tab's CLI rejected the persisted
    /// conversation id and a fresh CLI was spawned in its place. The tab keeps
    /// its slot, label, colour and pin; only the runtime handle, the status
    /// stream (and its watcher task) and the pane's backend change. `marker`
    /// is prefilled right after the swap, as early as the fresh session
    /// allows. Returns `false` when no tab holds `old` (closed meanwhile) —
    /// the caller then cancels `new` itself.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_agent_session(
        &mut self,
        old: AgentSessionId,
        new: AgentSessionId,
        new_status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        kind: RestoreMarker,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { session_id, .. } if *session_id == old)
        }) else {
            return false;
        };
        let notifier = self.notifier.clone();
        let window_active = self.window_active.clone();
        let tab = &mut self.tabs[idx];
        let PaneContent::Terminal(tree) = &tab.content else {
            return false;
        };
        let Some(view) = tree.active_view() else {
            return false;
        };
        view.update(cx, |v, cx| {
            v.replace_live_session(backend, term_id, cx);
            v.prefill_grid(marker(kind));
            v.arm_restore_notice(kind.label());
        });
        let PaneGroupTabKind::Agent {
            session_id,
            status_rx,
            worktree_path,
            ..
        } = &mut tab.kind
        else {
            return false;
        };
        *session_id = new;
        *status_rx = new_status_rx.clone();
        let workspace_key = worktree_path.to_string_lossy().into_owned();
        let label = tab.label.clone();
        let weak_view = view.downgrade();
        // The restore path hands the same proxy stream back (see
        // `restore_agent_tab`); respawning the watcher re-keys its toasts and
        // banners to the fresh session's tab id.
        tab._status_task = Some(spawn_status_task(
            new_status_rx,
            notifier,
            window_active,
            TabId::from(new),
            label,
            workspace_key,
            weak_view,
            cx,
        ));
        cx.notify();
        true
    }
}
