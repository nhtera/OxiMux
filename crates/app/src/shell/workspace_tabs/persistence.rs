//! Snapshot + restore methods for `WorkspaceTabs`. Split out from
//! `mod.rs` to keep the parent file under the 800-LOC hard cap. Fields
//! touched here are crate-private on the struct definition in `mod.rs`.

use gpui::{AppContext, Context, Entity, SharedString, Task, Window};
use oximux_agents::{AgentStatusStream, SharedBackend};
use oximux_core::AgentSessionId;
use oximux_pty::TerminalSessionId;

use oximux_storage::{PaneBufferRepo, PaneRelayIdRepo};

use crate::notifier::TabId;
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedTab, PersistedTabs, snapshot_tree,
};
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::main_pane::MainPane;
use crate::shell::terminal_view::TerminalView;

use super::{SaveCallback, WorkspaceTab, WorkspaceTabKind, WorkspaceTabs};

impl WorkspaceTabs {
    /// Install the persistence sink. Called by `build_workspace_tabs` right
    /// after construction; `None` during tests + initial construction.
    pub fn set_save_callback(&mut self, cb: SaveCallback) {
        self.save_callback = Some(cb);
    }

    /// Build a snapshot of every tab. Plain terminals capture topology only;
    /// agent tabs additionally carry the spawn-config metadata so restore
    /// can hand `CliRuntime::start_session` the same shape (step 15).
    /// `Custom` adapters are excluded — their argv is non-deterministic.
    pub fn snapshot(&self, cx: &gpui::App) -> PersistedTabs {
        let mut tabs = Vec::with_capacity(self.tabs.len());
        let mut active_offset: Option<usize> = None;
        for (idx, tab) in self.tabs.iter().enumerate() {
            let agent = match &tab.kind {
                WorkspaceTabKind::Terminal => None,
                WorkspaceTabKind::Agent {
                    adapter,
                    adapter_id,
                    worktree_path,
                    model,
                    effort,
                    ..
                } => {
                    if matches!(adapter, oximux_core::AgentAdapter::Custom) {
                        continue;
                    }
                    Some(PersistedAgentTab {
                        adapter: *adapter,
                        adapter_id: (*adapter_id).to_string(),
                        worktree_path: worktree_path.display().to_string(),
                        model: model.clone(),
                        effort: effort.clone(),
                    })
                }
            };
            if idx == self.active {
                active_offset = Some(tabs.len());
            }
            tabs.push(PersistedTab {
                label: tab.label.to_string(),
                tree: snapshot_tree(tab.pane.read(cx).tree()),
                agent,
            });
        }
        PersistedTabs {
            tabs,
            active: active_offset.unwrap_or(0),
            next_label_n: self.next_label_n,
        }
    }

    /// Recompute the topology signature; persist if it advanced. Called
    /// from per-pane `cx.observe` so the 60Hz PTY-output tick doesn't
    /// trigger a settings write — only real split / weight / leaf changes
    /// bump `topology_version`.
    pub(super) fn maybe_save_on_topology_change(&mut self, cx: &mut Context<Self>) {
        let sig: u64 = self
            .tabs
            .iter()
            .map(|t| t.pane.read(cx).topology_version())
            .sum();
        if sig != self.last_topology_signature {
            self.last_topology_signature = sig;
            self.save_now(cx);
        }
    }

    /// Build a snapshot + hand it to the save callback. No-op when no
    /// callback is registered (test path).
    pub(super) fn save_now(&self, cx: &mut Context<Self>) {
        if let Some(cb) = self.save_callback.clone() {
            let snap = self.snapshot(cx);
            cb(snap);
        }
    }

    /// Append a Terminal tab restored from persistence. Mirrors
    /// `open_tab`'s observer wiring exactly; skips the label-counter bump
    /// + save (caller restores both via `apply_restored_state`).
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        pane: Entity<MainPane>,
        cx: &mut Context<Self>,
    ) {
        let observer = cx.observe(&pane, |this, _pane, cx| {
            cx.notify();
            this.maybe_save_on_topology_change(cx);
        });
        self.tabs.push(WorkspaceTab {
            label: SharedString::from(label),
            pane,
            kind: WorkspaceTabKind::Terminal,
            _observer: observer,
            _status_task: None,
        });
    }

    /// Append an Agent tab restored from persistence. Caller has already
    /// driven `start_session` + assembled the handles; this mirrors
    /// `push_restored_terminal_tab` (no label-counter bump, no save —
    /// caller restores active + counter via `apply_restored_state`).
    #[allow(clippy::too_many_arguments)]
    pub fn push_restored_agent_tab(
        &mut self,
        persisted: &PersistedAgentTab,
        adapter_id: &'static str,
        label: String,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let typography_for_view = typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend,
                term_id,
                theme,
                density,
                typography_for_view,
                window,
                cx,
            )
        });
        let pane = cx.new(|cx| {
            MainPane::new(
                view,
                std::path::PathBuf::from(&persisted.worktree_path),
                theme,
                density,
                typography.clone(),
                cx,
            )
        });
        let observer = cx.observe(&pane, |this, _pane, cx| {
            cx.notify();
            this.maybe_save_on_topology_change(cx);
        });
        let label_shared = SharedString::from(label.clone());
        let status_task: Task<()> = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            TabId::from(session_id),
            label_shared.clone(),
            cx,
        );
        self.tabs.push(WorkspaceTab {
            label: label_shared,
            pane,
            kind: WorkspaceTabKind::Agent {
                adapter: persisted.adapter,
                adapter_id,
                worktree_path: std::path::PathBuf::from(&persisted.worktree_path),
                model: persisted.model.clone(),
                effort: persisted.effort.clone(),
                session_id,
                status_rx,
            },
            _observer: observer,
            _status_task: Some(status_task),
        });
        cx.notify();
    }

    /// Capture every plain-terminal leaf's scrollback to `pane_buffers`
    /// keyed by `(project_id, ordinal)`. Ordinal counts plain-terminal
    /// leaves in DFS tab+leaf order, skipping agent tabs (agent restore
    /// goes through `CliRuntime::start_session` and the CLI reloads its
    /// own history).
    ///
    /// Best-effort: failures are logged but do not abort. Called at
    /// project-switch and app-quit checkpoints — never on the per-frame
    /// `cx.observe` path (full grid serialization is too expensive for
    /// that cadence).
    pub fn capture_pane_buffers(
        &self,
        repo: &PaneBufferRepo,
        project_id: &str,
        max_bytes_per_pane: usize,
        cx: &gpui::App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id) {
            tracing::warn!(?err, project_id, "pane_buffers: delete_for_project failed");
            return;
        }
        let mut ordinal: u32 = 0;
        for tab in &self.tabs {
            if !matches!(tab.kind, WorkspaceTabKind::Terminal) {
                continue;
            }
            let buffers = tab.pane.read(cx).collect_pane_buffers(max_bytes_per_pane, cx);
            for bytes in buffers {
                if bytes.is_empty() {
                    ordinal += 1;
                    continue;
                }
                if let Err(err) = repo.set(project_id, ordinal, &bytes) {
                    tracing::warn!(?err, project_id, ordinal, "pane_buffers: set failed");
                }
                ordinal += 1;
            }
        }
    }

    /// Walk every plain-terminal leaf in DFS order and persist each
    /// leaf's relay-side PTY id (if any) keyed by
    /// `(project_id, ordinal)`. Agent tabs are skipped because their
    /// restore path goes through `CliRuntime::start_session`, not the
    /// pane factory's attach-or-spawn fork. Same ordinal-counting
    /// rules as `capture_pane_buffers` so the two tables stay
    /// aligned by index.
    ///
    /// `relay_session_id` is the daemon's `HelloAck.session_id` at
    /// capture time; phase-06 reconciliation uses it to detect
    /// "daemon restarted, all persisted ids stale."
    pub fn capture_pane_relay_ids(
        &self,
        repo: &PaneRelayIdRepo,
        project_id: &str,
        relay_session_id: &str,
        cx: &gpui::App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id) {
            tracing::warn!(?err, project_id, "pane_relay_ids: delete_for_project failed");
            return;
        }
        let mut ordinal: u32 = 0;
        for tab in &self.tabs {
            if !matches!(tab.kind, WorkspaceTabKind::Terminal) {
                continue;
            }
            let ids = tab.pane.read(cx).collect_pane_external_ids(cx);
            for id in ids {
                if let Some(pty_id) = id
                    && let Err(err) = repo.set(project_id, ordinal, &pty_id, relay_session_id)
                {
                    tracing::warn!(?err, project_id, ordinal, "pane_relay_ids: set failed");
                }
                ordinal += 1;
            }
        }
    }

    /// Finalize the restore: set active tab + label counter, focus the
    /// active tab, prime the topology signature so the first PTY tick
    /// after restore doesn't trigger a redundant save.
    pub fn apply_restored_state(
        &mut self,
        active: usize,
        next_label_n: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if active < self.tabs.len() {
            self.active = active;
        }
        if next_label_n > self.next_label_n {
            self.next_label_n = next_label_n;
        }
        self.last_topology_signature = self
            .tabs
            .iter()
            .map(|t| t.pane.read(cx).topology_version())
            .sum();
        self.focus_active(window, cx);
        cx.notify();
    }
}
