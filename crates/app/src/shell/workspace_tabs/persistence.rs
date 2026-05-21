//! Snapshot + restore methods for `WorkspaceTabs`. Split out from
//! `mod.rs` to keep the parent file under the 800-LOC hard cap. Fields
//! touched here are crate-private on the struct definition in `mod.rs`.

use gpui::{Context, Entity, SharedString, Window};

use crate::persisted_terminals::{PersistedTab, PersistedTabs, snapshot_tree};
use crate::shell::main_pane::MainPane;

use super::{SaveCallback, WorkspaceTab, WorkspaceTabKind, WorkspaceTabs};

impl WorkspaceTabs {
    /// Install the persistence sink. Called by `build_workspace_tabs` right
    /// after construction; `None` during tests + initial construction.
    pub fn set_save_callback(&mut self, cb: SaveCallback) {
        self.save_callback = Some(cb);
    }

    /// Build a snapshot of every Terminal tab. Agent tabs are skipped —
    /// the `CliRuntime` session is per-process and can't be revived.
    pub fn snapshot(&self, cx: &gpui::App) -> PersistedTabs {
        let mut tabs = Vec::with_capacity(self.tabs.len());
        let mut active_offset: Option<usize> = None;
        for (idx, tab) in self.tabs.iter().enumerate() {
            if !matches!(tab.kind, WorkspaceTabKind::Terminal) {
                continue;
            }
            if idx == self.active {
                active_offset = Some(tabs.len());
            }
            tabs.push(PersistedTab {
                label: tab.label.to_string(),
                tree: snapshot_tree(tab.pane.read(cx).tree()),
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
