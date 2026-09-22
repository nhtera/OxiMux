//! Cockpit agent tabs on a cold restore: the marker prefill and the resume
//! fallback swap. Split out of `tabs.rs` (which sits near the file-size cap)
//! and kept to the two operations the restore path needs.

use super::*;

impl PaneGroup {
    /// Prefill the agent tab at insertion index `idx` with a restore marker.
    /// Called in the same update closure that mounted the tab, so it lands as
    /// early as the mount allows. The grid takes bytes as the backend pumps
    /// them, so a CLI that has already painted before the mount would show
    /// the marker inside its first frame until its next repaint; in practice
    /// the spawn-to-mount gap is milliseconds and a CLI's first frame is
    /// hundreds of milliseconds out.
    pub(crate) fn prefill_agent_tab(&self, idx: usize, bytes: &[u8], cx: &App) {
        let Some(tab) = self.tabs.get(idx) else {
            return;
        };
        if let PaneContent::Terminal(tree) = &tab.content
            && let Some(view) = tree.active_view()
        {
            view.read(cx).prefill_grid(bytes);
        }
    }

    /// Arm or disarm the status watcher of the agent tab holding `session`.
    /// A cold-restored tab spawned on a resumed conversation is mounted with
    /// its watcher disarmed for the 5 s verdict window: a CLI that rejects the
    /// id exits failed, and the watcher would toast "failed" and post an OS
    /// banner a beat before the fallback swaps a fresh session in. Re-armed
    /// once the resume is known to have taken; the swap arms its own. The tab
    /// chip reads the status stream directly, so the badge stays live either
    /// way. Returns `false` when no tab holds `session`.
    pub(crate) fn set_agent_status_watch(
        &mut self,
        session: AgentSessionId,
        armed: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { session_id, .. } if *session_id == session)
        }) else {
            return false;
        };
        let notifier = self.notifier.clone();
        let window_active = self.window_active.clone();
        let tab = &mut self.tabs[idx];
        if !armed {
            tab._status_task = None;
            return true;
        }
        let PaneGroupTabKind::Agent {
            status_rx,
            worktree_path,
            ..
        } = &tab.kind
        else {
            return false;
        };
        let PaneContent::Terminal(tree) = &tab.content else {
            return false;
        };
        let Some(view) = tree.active_view() else {
            return false;
        };
        tab._status_task = Some(spawn_status_task(
            status_rx.clone(),
            notifier,
            window_active,
            TabId::from(session),
            tab.label.clone(),
            worktree_path.to_string_lossy().into_owned(),
            view.downgrade(),
            cx,
        ));
        true
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
        marker: &[u8],
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
            v.prefill_grid(marker);
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
        // Dropping the old task ends the watcher on the rejected session's
        // stream; the new one carries the tab's badge and notifications.
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
