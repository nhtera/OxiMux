//! Cockpit agent tabs on a cold restore: the marker prefill and the resume
//! fallback swap — and, after a daemon restart, handing a lost tab to the
//! workspace to resume in place. Split out of `tabs.rs` (which sits near the
//! file-size cap).

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
    /// the caller then cancels `new` itself. `view_term` names the pane that
    /// shows the agent (its current terminal session); `None` takes the tab's
    /// active pane, which for a freshly restored tab is its only one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_agent_session(
        &mut self,
        old: AgentSessionId,
        new: AgentSessionId,
        new_status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        kind: RestoreMarker,
        view_term: Option<TerminalSessionId>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { session_id, .. } if *session_id == old)
        }) else {
            return false;
        };
        let tab = &mut self.tabs[idx];
        let PaneContent::Terminal(tree) = &tab.content else {
            return false;
        };
        let view = match view_term {
            Some(term) => tree
                .iter_all_views()
                .map(|(_, _, v)| v)
                .find(|v| v.read(cx).session_id() == term)
                .cloned(),
            None => tree.active_view().cloned(),
        };
        let Some(view) = view else {
            return false;
        };
        view.update(cx, |v, cx| {
            v.replace_live_session(backend, term_id, cx);
            v.prefill_grid(marker(kind));
            v.arm_restore_notice(kind.label());
        });
        let PaneGroupTabKind::Agent { session_id, status_rx, .. } = &mut tab.kind
        else {
            return false;
        };
        *session_id = new;
        *status_rx = new_status_rx.clone();
        let label = tab.label.clone();
        let weak_view = view.downgrade();
        // The restore path hands the same proxy stream back (see
        // `restore_agent_tab`); respawning the watcher re-keys its ring and
        // toasts to the fresh session's tab id.
        tab._status_task = Some(spawn_status_task(
            new_status_rx,
            TabId::from(new),
            label,
            weak_view,
            cx,
        ));
        cx.notify();
        true
    }

    /// Drain the lost-agent queue now, through the window this group last
    /// rendered in, rather than waiting for its next render — which for a
    /// project not on screen may be much later. The render drain stays as the
    /// fallback for a group that has not rendered yet.
    pub(super) fn defer_resume_lost_agents(&self, cx: &mut Context<Self>) {
        let Some(handle) = self.window else {
            return;
        };
        let group = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = group.update(cx, |g, cx| g.resume_lost_agents(window, cx));
            });
        });
    }

    /// Hand every queued lost agent tab to this window's workspace, which
    /// resumes its conversation in place (`agent_mount::resume_agent_in_place`).
    /// Deferred to after this frame: the workspace is not updated from inside
    /// a render.
    pub(crate) fn resume_lost_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_lost_agents.is_empty() {
            return;
        }
        // Left queued until the window's workspace is registered.
        let window_id = window.window_handle().window_id();
        let Some(root) = crate::window_registry::workspace_for_window(cx, window_id) else {
            return;
        };
        let lost = std::mem::take(&mut self.pending_lost_agents);
        let group = cx.weak_entity();
        for (old, terminal) in lost {
            let Some(lost) = self.lost_agent(old, terminal, group.clone(), cx) else {
                continue;
            };
            let root = root.clone();
            cx.spawn_in(window, async move |_, cx| {
                let _ = root.update_in(cx, |root, window, cx| {
                    crate::session_restore::agent_mount::resume_agent_in_place(root, lost, window, cx);
                });
            })
            .detach();
        }
    }

    /// The agent tab holding `session`, as it would be persisted right now —
    /// its conversation id from the latest status snapshot — plus the lost
    /// PTY of its own pane (`terminal`). `None` when the tab is gone or is not
    /// an agent tab.
    fn lost_agent(
        &self,
        session: AgentSessionId,
        terminal: TerminalSessionId,
        group: WeakEntity<Self>,
        cx: &App,
    ) -> Option<crate::session_restore::agent_mount::LostAgent> {
        let tab = self.tabs.iter().find(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { session_id, .. } if *session_id == session)
        })?;
        let PaneGroupTabKind::Agent {
            adapter,
            adapter_id,
            worktree_path,
            model,
            effort,
            profile,
            status_rx,
            ..
        } = &tab.kind
        else {
            return None;
        };
        let dead_pty = match &tab.content {
            PaneContent::Terminal(tree) => tree
                .iter_all_views()
                .map(|(_, _, v)| v.read(cx))
                .find(|v| v.session_id() == terminal)
                .and_then(|v| v.relay_pty_id()),
            _ => None,
        };
        let persisted = crate::persisted_terminals::PersistedAgentTab {
            adapter: *adapter,
            adapter_id: (*adapter_id).to_string(),
            worktree_path: worktree_path.display().to_string(),
            model: model.clone(),
            effort: effort.clone(),
            relay_external_id: dead_pty.clone(),
            relay_session: None,
            profile: profile.clone(),
            provider_session: crate::session_restore::agent_resume::provider_session_from_snapshot(
                &status_rx.borrow(),
            ),
        };
        Some(crate::session_restore::agent_mount::LostAgent {
            persisted,
            old_session: session,
            terminal,
            dead_pty,
            group,
        })
    }
}
