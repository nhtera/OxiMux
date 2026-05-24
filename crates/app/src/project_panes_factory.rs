//! Per-project `ProjectPanes` construction + restore + persistence
//! helpers. Replaces the legacy `workspace_tabs_factory` after the
//! pane-groups cutover (phase 5 / step 6).
//!
//! Public surface:
//! - `build_project_panes` — constructs a `ProjectPanes` entity, restoring
//!   from a `PersistedTabs` snapshot when one is supplied.
//! - `load_persisted_tabs` / `save_persisted_tabs` — JSON blob round-trip
//!   against `SettingsRepo` under the `terminal_tabs:<project_id>` key.
//! - `compute_attach_hints` — phase-06 daemon-side pty reconciliation
//!   helper, unchanged from the legacy module.
//! - `load_pane_buffers` — captured scrollback bytes for a project, in
//!   ordinal-ascending order.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, WeakEntity, Window};
use oximux_agents::{AgentRuntime, AgentSessionConfig, CliRuntime};
use oximux_core::AgentAdapter;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{PaneBufferRepo, SettingsRepo};

use crate::notifier::Notifier;
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedTabKind, PersistedTabs, settings_key,
};
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;
use crate::shell::terminal_view::{TerminalView, attach_pty_existing, spawn_local_pty};
use crate::workspace_root::WorkspaceRoot;

const DEFAULT_AGENT_COLS: u16 = 120;
const DEFAULT_AGENT_ROWS: u16 = 32;

/// Per-pane cap on captured scrollback bytes. 512 KiB matches the
/// reference cockpit; covers ~5k rows × 80 cols of typical output even
/// with SGR runs.
pub const PANE_BUFFER_MAX_BYTES: usize = 512 * 1024;

/// Per-ordinal hint: "this leaf should attach to this surviving daemon
/// PTY id instead of spawning fresh."
pub type AttachHints = std::collections::HashMap<u32, String>;

/// Pre-filter persisted (ordinal, relay_pty_id, relay_session) rows
/// down to the ordinals that should re-attach.
pub fn compute_attach_hints(
    pane_relay_ids: Vec<(u32, String, String)>,
    live_external_ids: &HashSet<String>,
    current_external_session: Option<&str>,
) -> AttachHints {
    pane_relay_ids
        .into_iter()
        .filter_map(|(ord, pty_id, session)| {
            let session_ok = current_external_session
                .map(|s| s == session)
                .unwrap_or(false);
            (session_ok && live_external_ids.contains(&pty_id)).then_some((ord, pty_id))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_project_panes(
    cwd: PathBuf,
    snapshot: Option<PersistedTabs>,
    pane_buffers: Vec<Vec<u8>>,
    attach_hints: AttachHints,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Entity<ProjectPanes> {
    let panes_entity = cx.new(|cx| {
        ProjectPanes::new(
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

    let restore = snapshot.filter(|s| !s.tabs.is_empty());
    let Some(snap) = restore else {
        panes_entity.update(cx, |p, cx| p.seed_default_terminal(window, cx));
        return panes_entity;
    };

    // v3 path: multi-group restore when `group_tree` + `groups` are
    // present. Falls through to the legacy single-group path otherwise.
    // Clone the tree out so we can hand it (by reference) into the
    // helper while still moving `snap` for the per-group iteration.
    if snap.group_tree.is_some() && !snap.groups.is_empty() {
        let tree = snap.group_tree.clone().expect("group_tree just checked");
        return restore_multi_group(
            panes_entity,
            snap,
            &tree,
            pane_buffers,
            attach_hints,
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            window,
            cx,
        );
    }

    // Legacy v2 single-group path. Flat tab list, every tab lands in
    // the placeholder initial group. Editor tabs whose file is missing
    // are skipped with a warn. Pane buffers align DFS-flat with the
    // saved tab list.
    let mut buf_iter = pane_buffers.into_iter();
    let mut ordinal: u32 = 0;
    for tab in &snap.tabs {
        match &tab.kind {
            PersistedTabKind::Editor { path } => {
                let path_buf = PathBuf::from(path);
                if !path_buf.exists() {
                    // v1 placeholder strategy: drop the tab + log. Full
                    // "missing-file placeholder tab" is a follow-up; the
                    // user can reopen via Cmd+P. Don't fail the entire
                    // restore — just skip this one entry.
                    //
                    // KNOWN limitation: when a dropped editor tab's flat
                    // index is < `snap.active`, the restored active tab
                    // points to the wrong slot (snap.active stays as the
                    // saved value; the indices behind it shift down by
                    // one per skip). `apply_restored_state` clamps to
                    // `tab_count()` so this never panics, but the
                    // wrong tab may end up focused on the first paint.
                    // Acceptable for v1 — the dropped-tab case is rare.
                    tracing::warn!(
                        ?path,
                        "editor tab restore: file no longer exists; skipping tab"
                    );
                    continue;
                }
                panes_entity.update(cx, |p, cx| {
                    p.open_or_activate_editor_tab(path_buf, window, cx);
                });
            }
            PersistedTabKind::Terminal => {
                if let Some(agent) = &tab.agent {
                    restore_agent_tab(
                        agent,
                        tab.label.clone(),
                        None,
                        cli_runtime.clone(),
                        panes_entity.downgrade(),
                        window,
                        cx,
                    );
                } else if let Some(view) = build_terminal_view_for_tab(
                    cwd.clone(),
                    &attach_hints,
                    ordinal,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                ) {
                    let bytes = buf_iter.next().unwrap_or_default();
                    if !bytes.is_empty() && !attach_hints.contains_key(&ordinal) {
                        view.read(cx).prefill_grid(&bytes);
                    }
                    panes_entity.update(cx, |p, cx| {
                        p.push_restored_terminal_tab(tab.label.clone(), view, cx);
                    });
                    ordinal += 1;
                }
            }
        }
    }
    let tab_order = snap.tab_order.clone();
    panes_entity.update(cx, |p, cx| {
        p.apply_restored_state(snap.active, snap.next_label_n, tab_order, window, cx);
    });
    panes_entity
}

/// v3 multi-group restore. Walks `snap.groups` in DFS order, distributing
/// the flat `snap.tabs` across the freshly-allocated groups using each
/// group's `tab_count`. Pane buffers + agent restores plumb the target
/// `PaneGroupId` so async completions don't race the active-group pointer.
#[allow(clippy::too_many_arguments)]
fn restore_multi_group(
    panes_entity: Entity<ProjectPanes>,
    snap: PersistedTabs,
    tree: &crate::persisted_terminals::PersistedTree,
    pane_buffers: Vec<Vec<u8>>,
    attach_hints: AttachHints,
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Entity<ProjectPanes> {
    // 1. Allocate group ids in DFS order; replaces the placeholder
    // initial group inside `ProjectPanes`.
    let allocated: Vec<PaneGroupId> = panes_entity.update(cx, |p, cx| {
        p.rebuild_groups_from_tree(tree, snap.active_group, window, cx)
    });
    if allocated.is_empty() {
        // Defensive: rebuild_groups_from_tree always returns at least one
        // leaf for a well-formed tree, so this only fires on a malformed
        // blob. Fall back to a default terminal so the user sees something.
        panes_entity.update(cx, |p, cx| p.seed_default_terminal(window, cx));
        return panes_entity;
    }
    // 2. Walk flat `snap.tabs`, distribute across groups by `tab_count`.
    let mut buf_iter = pane_buffers.into_iter();
    let mut ordinal: u32 = 0;
    let mut flat_iter = snap.tabs.iter();
    for (g_idx, group_snap) in snap.groups.iter().enumerate() {
        let Some(&group_id) = allocated.get(g_idx) else {
            // Tree leaf count > groups vec — malformed blob. Stop.
            tracing::warn!(
                g_idx,
                allocated_len = allocated.len(),
                "multi-group restore: group index past allocated tree leaves"
            );
            break;
        };
        for _ in 0..group_snap.tab_count {
            let Some(tab) = flat_iter.next() else {
                tracing::warn!("multi-group restore: tabs exhausted mid-group");
                break;
            };
            match &tab.kind {
                PersistedTabKind::Editor { path } => {
                    let path_buf = PathBuf::from(path);
                    if !path_buf.exists() {
                        tracing::warn!(
                            ?path,
                            "editor tab restore: file no longer exists; skipping tab"
                        );
                        continue;
                    }
                    panes_entity.update(cx, |p, cx| {
                        p.open_editor_in_group_restore(group_id, path_buf, window, cx);
                    });
                }
                PersistedTabKind::Terminal => {
                    if let Some(agent) = &tab.agent {
                        restore_agent_tab(
                            agent,
                            tab.label.clone(),
                            Some(group_id),
                            cli_runtime.clone(),
                            panes_entity.downgrade(),
                            window,
                            cx,
                        );
                    } else if let Some(view) = build_terminal_view_for_tab(
                        cwd.clone(),
                        &attach_hints,
                        ordinal,
                        theme,
                        density,
                        typography.clone(),
                        window,
                        cx,
                    ) {
                        let bytes = buf_iter.next().unwrap_or_default();
                        if !bytes.is_empty() && !attach_hints.contains_key(&ordinal) {
                            view.read(cx).prefill_grid(&bytes);
                        }
                        panes_entity.update(cx, |p, cx| {
                            p.push_restored_terminal_tab_in(
                                group_id,
                                tab.label.clone(),
                                view,
                                cx,
                            );
                        });
                        ordinal += 1;
                    }
                }
            }
        }
    }
    // 3. Apply per-group active + tab_order; activate saved focused group.
    let per_group: Vec<(PaneGroupId, Vec<usize>, usize)> = snap
        .groups
        .iter()
        .enumerate()
        .filter_map(|(g_idx, group_snap)| {
            allocated
                .get(g_idx)
                .copied()
                .map(|gid| (gid, group_snap.tab_order.clone(), group_snap.active))
        })
        .collect();
    let active_group_id = allocated
        .get(snap.active_group)
        .copied()
        .unwrap_or(allocated[0]);
    panes_entity.update(cx, |p, cx| {
        p.apply_restored_state_multi(per_group, active_group_id, window, cx);
    });
    panes_entity
}

/// Spawn-and-mount an agent tab during restore. `target_group` is
/// `None` for legacy single-group restores (mount lands in the active
/// group at completion) and `Some(group_id)` for v3 multi-group
/// restores (mount lands in the named group regardless of which group
/// holds focus when the async work finishes).
fn restore_agent_tab(
    persisted: &PersistedAgentTab,
    label: String,
    target_group: Option<PaneGroupId>,
    cli_runtime: Arc<CliRuntime>,
    panes: WeakEntity<ProjectPanes>,
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
        let mount = panes.update_in(cx, |p, window, cx| match target_group {
            Some(group_id) => p.push_restored_agent_tab_in(
                group_id,
                &persisted_clone,
                adapter_id,
                label,
                session_id,
                status_rx,
                backend,
                term_id,
                window,
                cx,
            ),
            None => p.push_restored_agent_tab(
                &persisted_clone,
                adapter_id,
                label,
                session_id,
                status_rx,
                backend,
                term_id,
                window,
                cx,
            ),
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

fn static_adapter_id(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

#[allow(clippy::too_many_arguments)]
fn build_terminal_view_for_tab(
    cwd: PathBuf,
    attach_hints: &AttachHints,
    ordinal: u32,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<TerminalView>> {
    let (backend, session_id) = if let Some(relay_pty_id) = attach_hints.get(&ordinal)
        && let Some(result) = attach_pty_existing(relay_pty_id)
    {
        result
    } else {
        spawn_local_pty(cwd)?
    };
    Some(cx.new(|cx| {
        TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
    }))
}

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

pub(crate) fn load_pane_buffers(repo: &PaneBufferRepo, project_id: &str) -> Vec<Vec<u8>> {
    match repo.get_all_for_project(project_id) {
        Ok(rows) => {
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

pub(crate) fn save_persisted_tabs(repo: &SettingsRepo, project_id: &str, snap: &PersistedTabs) {
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
