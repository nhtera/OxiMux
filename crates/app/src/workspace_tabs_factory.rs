//! Per-project `WorkspaceTabs` construction + restore + persistence
//! helpers. Extracted from `workspace_root.rs` + `workspace_ops.rs` to
//! keep both files under the 800-LOC hard cap.
//!
//! Public surface:
//! - `build_workspace_tabs` — constructs a tabs entity, restoring from a
//!   `PersistedTabs` snapshot when one is supplied.
//! - `load_persisted_tabs` / `save_persisted_tabs` — JSON blob round-trip
//!   against `SettingsRepo` under the `terminal_tabs:<project_id>` key.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, WeakEntity, Window};
use oximux_agents::{AgentRuntime, AgentSessionConfig, CliRuntime};
use oximux_core::AgentAdapter;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{PaneBufferRepo, SettingsRepo};

use crate::notifier::Notifier;
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedTabs, PersistedTree, count_leaves, restore_tree, settings_key,
};
use crate::shell::main_pane::MainPane;
use crate::shell::pane_tree::PaneId;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};
use crate::shell::workspace_tabs::WorkspaceTabs;
use crate::workspace_root::WorkspaceRoot;

const DEFAULT_AGENT_COLS: u16 = 120;
const DEFAULT_AGENT_ROWS: u16 = 32;

/// Per-pane cap on captured scrollback bytes. 512 KiB matches the
/// reference cockpit; covers ~5k rows × 80 cols of typical output even
/// with SGR runs. Stored as raw BLOB so JSON encoding overhead is nil.
pub const PANE_BUFFER_MAX_BYTES: usize = 512 * 1024;

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_workspace_tabs(
    cwd: PathBuf,
    snapshot: Option<PersistedTabs>,
    pane_buffers: Vec<Vec<u8>>,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<WorkspaceTabs>> {
    let restore: Option<PersistedTabs> = snapshot.filter(|s| !s.tabs.is_empty());

    let tabs_entity = cx.new(|cx| {
        WorkspaceTabs::new(
            cwd.clone(),
            theme,
            density,
            typography.clone(),
            cli_runtime.clone(),
            notifier.clone(),
            window,
            cx,
        )
    });

    let Some(snap) = restore else {
        tabs_entity.update(cx, |tabs, cx| tabs.seed_default_terminal(window, cx));
        return Some(tabs_entity);
    };

    // Snapshot path. Walk tabs in saved order; terminals build synchronously,
    // agents kick off an async `start_session` + push when ready.
    // `pane_buffers_iter` cursors through the captured scrollback bytes in
    // the same DFS leaf order produced by capture so each restored leaf gets
    // its own buffer (when one exists).
    let mut pane_buffers_iter = pane_buffers.into_iter();
    for tab in &snap.tabs {
        if let Some(agent) = &tab.agent {
            restore_agent_tab(
                agent,
                tab.label.clone(),
                cli_runtime.clone(),
                tabs_entity.downgrade(),
                window,
                cx,
            );
        } else if let Some(pane) = build_pane_for_tree(
            &tab.tree,
            cwd.clone(),
            theme,
            density,
            typography.clone(),
            window,
            cx,
        ) {
            // Pre-paint scrollback into each leaf BEFORE the live PTY's
            // poll task ticks for the first time so restored output lands
            // above the fresh shell prompt.
            let leaf_count = pane.read(cx).leaf_count();
            let chunk: Vec<Vec<u8>> = (0..leaf_count)
                .map(|_| pane_buffers_iter.next().unwrap_or_default())
                .collect();
            pane.update(cx, |main, cx| {
                main.prefill_leaves(&chunk, cx);
            });
            tabs_entity.update(cx, |tabs, cx| {
                tabs.push_restored_terminal_tab(tab.label.clone(), pane, cx);
            });
        }
    }
    tabs_entity.update(cx, |tabs, cx| {
        tabs.apply_restored_state(snap.active, snap.next_label_n, window, cx);
    });
    Some(tabs_entity)
}

/// Async agent respawn. Mirrors `WorkspaceRoot::spawn_agent_tab` but
/// hands the persisted label through `push_restored_agent_tab` so the
/// strip keeps the user's pre-quit label and skips the save-on-mount
/// (the snapshot is already on disk).
fn restore_agent_tab(
    persisted: &PersistedAgentTab,
    label: String,
    cli_runtime: Arc<CliRuntime>,
    ws: WeakEntity<WorkspaceTabs>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) {
    if matches!(persisted.adapter, AgentAdapter::Custom) {
        tracing::info!("agent restore: skipping Custom adapter (non-deterministic argv)");
        return;
    }
    let cfg = AgentSessionConfig {
        adapter: persisted.adapter,
        worktree_path: PathBuf::from(&persisted.worktree_path),
        prompt: None,
        model: persisted.model.clone(),
        effort: persisted.effort.clone(),
        env: Vec::new(),
        cols: DEFAULT_AGENT_COLS,
        rows: DEFAULT_AGENT_ROWS,
        custom_command: None,
    };
    let adapter_id: &'static str = static_adapter_id(persisted.adapter);
    let persisted_clone = persisted.clone();
    cx.spawn_in(window, async move |_root, cx| {
        let session_id = match cli_runtime.start_session(cfg).await {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(?err, adapter = adapter_id, "agent restore: start_session failed");
                return;
            }
        };
        let backend = match cli_runtime.backend_for(session_id) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(?err, "agent restore: backend_for failed");
                let _ = cli_runtime.cancel(session_id).await;
                return;
            }
        };
        let term_id = match cli_runtime.terminal_session_id(session_id) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(?err, "agent restore: terminal_session_id failed");
                let _ = cli_runtime.cancel(session_id).await;
                return;
            }
        };
        let status_rx = match cli_runtime.subscribe_status(session_id) {
            Ok(rx) => rx,
            Err(err) => {
                tracing::warn!(?err, "agent restore: subscribe_status failed");
                let _ = cli_runtime.cancel(session_id).await;
                return;
            }
        };
        let mount = ws.update_in(cx, |tabs, window, cx| {
            tabs.push_restored_agent_tab(
                &persisted_clone,
                adapter_id,
                label,
                session_id,
                status_rx,
                backend,
                term_id,
                window,
                cx,
            );
        });
        if mount.is_err() {
            tracing::warn!(
                ?session_id,
                "agent restore: workspace dropped mid-spawn; cancelling orphan"
            );
            let _ = cli_runtime.cancel(session_id).await;
        }
    })
    .detach();
}

/// Map an `AgentAdapter` to the `'static` slug `push_restored_agent_tab`
/// expects. Mirrors `workspace_ops::agent_adapter_id` (not re-exported to
/// keep crate-internal visibility tight).
fn static_adapter_id(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

/// Spawn one MainPane whose pane tree mirrors `tree_snapshot`. Each leaf
/// gets a fresh PTY at `cwd`; axes + weights are preserved.
fn build_pane_for_tree(
    tree_snapshot: &PersistedTree,
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<MainPane>> {
    let leaf_count = count_leaves(tree_snapshot);
    if leaf_count == 0 {
        return None;
    }
    // Allocate PaneIds in DFS order; spawn a TerminalView per leaf in the
    // same order so the HashMap keys align with the rebuilt tree.
    let mut next_id: u64 = 0;
    let mut alloc = || {
        let id = PaneId(next_id);
        next_id += 1;
        id
    };
    let tree = restore_tree(tree_snapshot, &mut alloc);
    let mut panes: HashMap<PaneId, Entity<TerminalView>> = HashMap::with_capacity(leaf_count);
    for leaf_id in tree.in_order_leaves() {
        let (backend, session_id) = spawn_local_pty(cwd.clone())?;
        let typography_for_view = typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend,
                session_id,
                theme,
                density,
                typography_for_view,
                window,
                cx,
            )
        });
        panes.insert(leaf_id, view);
    }
    let focused = tree.in_order_leaves().first().copied()?;
    let next_id_seed = next_id;
    Some(cx.new(|cx| {
        MainPane::new_with_tree(
            tree,
            panes,
            focused,
            next_id_seed,
            cwd,
            theme,
            density,
            typography,
            cx,
        )
    }))
}

/// Read the persisted-tabs JSON blob for a project. Missing key or invalid
/// JSON returns `None` (caller falls back to the default 1-tab layout).
pub(crate) fn load_persisted_tabs(repo: &SettingsRepo, project_id: &str) -> Option<PersistedTabs> {
    let key = settings_key(project_id);
    let raw = match repo.get(&key) {
        Ok(v) => v?,
        Err(err) => {
            tracing::warn!(?err, project_id, "load_persisted_tabs: settings.get failed");
            return None;
        }
    };
    match serde_json::from_str::<PersistedTabs>(&raw) {
        Ok(snap) => Some(snap),
        Err(err) => {
            tracing::warn!(?err, project_id, "load_persisted_tabs: parse failed; ignoring");
            None
        }
    }
}

/// Fetch every captured pane buffer for a project, in ordinal-ascending
/// order, returning just the bytes (caller pairs them with leaves in DFS
/// order via the same indexing the capture path used). Missing project
/// or fetch failure yields an empty Vec.
pub(crate) fn load_pane_buffers(repo: &PaneBufferRepo, project_id: &str) -> Vec<Vec<u8>> {
    match repo.get_all_for_project(project_id) {
        Ok(rows) => {
            // Rows arrive sorted by ordinal; the highest ordinal sets the
            // Vec length so a missing middle ordinal becomes an empty
            // buffer rather than a misaligned pairing.
            let cap = rows.last().map(|(o, _)| *o as usize + 1).unwrap_or(0);
            let mut out: Vec<Vec<u8>> = vec![Vec::new(); cap];
            for (ord, bytes) in rows {
                let idx = ord as usize;
                if idx < out.len() {
                    out[idx] = bytes;
                }
            }
            out
        }
        Err(err) => {
            tracing::warn!(?err, project_id, "load_pane_buffers: get failed");
            Vec::new()
        }
    }
}

/// Serialize + upsert the snapshot. Best-effort; failure leaves the
/// in-memory state intact and is logged at warn.
pub(crate) fn save_persisted_tabs(
    repo: &SettingsRepo,
    project_id: &str,
    snap: &PersistedTabs,
) {
    let key = settings_key(project_id);
    let json = match serde_json::to_string(snap) {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(?err, project_id, "save_persisted_tabs: serialize failed");
            return;
        }
    };
    if let Err(err) = repo.set(&key, &json) {
        tracing::warn!(?err, project_id, "save_persisted_tabs: settings.set failed");
    }
}
