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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, WeakEntity, Window};
use oximux_agents::{AgentRuntime, AgentSessionConfig, CliRuntime};
use oximux_core::AgentAdapter;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{PaneBufferRepo, SettingsRepo};

use crate::notifier::Notifier;
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedTab, PersistedTabKind, PersistedTabs, WINDOWS_MANIFEST_KEY,
    WindowsManifest, legacy_settings_key, settings_key,
};
use crate::shell::context_env::SurfaceIds;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;
use crate::shell::terminal_view::{
    TerminalView, attach_pty_existing, spawn_local_pty, spawn_local_pty_dormant,
};
use crate::workspace_root::WorkspaceRoot;

const DEFAULT_AGENT_COLS: u16 = 120;
const DEFAULT_AGENT_ROWS: u16 = 32;

/// Per-pane cap on captured scrollback bytes. 512 KiB matches the
/// reference cockpit; covers ~5k rows × 80 cols of typical output even
/// with SGR runs.
pub const PANE_BUFFER_MAX_BYTES: usize = 512 * 1024;

/// Per-ordinal hint: "this leaf should attach to this surviving daemon
/// PTY id instead of spawning fresh."
pub type AttachHints = HashMap<u32, String>;

/// F3.4 — per-sub-pane scrollback bytes keyed by `(tab_ordinal,
/// sub_pane_ordinal)`. Replaces the pre-v4 flat `Vec<Vec<u8>>` so
/// multi-sub-pane tabs prefill every leaf's grid independently. Bytes
/// for pre-v4 rows resolve to `sub_pane_ordinal = 0` (via the V004
/// migration), keeping single-sub-pane tabs identical to the old path.
pub type PaneBuffersMap = HashMap<(u32, u32), Vec<u8>>;

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
    pane_buffers: PaneBuffersMap,
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
    // are skipped with a warn. Pane buffers align by `(tab_ordinal,
    // sub_pane_ordinal)` — single-sub-pane tabs read at sub_pane_ord=0
    // which is exactly how V004 stores legacy rows.
    let mut pane_buffers = pane_buffers;
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
                } else if needs_tree_restore(tab) {
                    // Multi-sub-pane restore — spawn one fresh PTY per
                    // leaf at the captured cwd (no relay attach). Every
                    // sub-pane gets its own scrollback bytes from the
                    // pane_buffers map, keyed by `(ordinal, sub_pane_ord)`.
                    if let Some(tree) = build_multi_sub_pane_tree(
                        tab,
                        ordinal,
                        &mut pane_buffers,
                        cwd.clone(),
                        theme,
                        density,
                        typography.clone(),
                        window,
                        cx,
                    ) {
                        panes_entity.update(cx, |p, cx| {
                            p.push_restored_terminal_tab_with_tree(tab.label.clone(), tree, cx);
                        });
                        ordinal += 1;
                    }
                } else if let Some(view) = build_terminal_view_for_tab(
                    cwd.clone(),
                    &attach_hints,
                    ordinal,
                    single_leaf_ids(tab, &cwd),
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                ) {
                    // Content restore comes ONLY from a live daemon reattach
                    // (handled inside `build_terminal_view_for_tab`). When no
                    // live PTY exists, the tab opens a clean fresh shell — we
                    // do NOT replay a serialized grid here. Grid replay had to
                    // reflow to the new pane size and scrambled full-screen
                    // TUIs; the reference app likewise prunes local scrollback
                    // and leans entirely on the daemon's raw-byte reattach.
                    let _ = pane_buffers.remove(&(ordinal, 0));
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
    pane_buffers: PaneBuffersMap,
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
    let mut pane_buffers = pane_buffers;
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
                    } else if needs_tree_restore(tab) {
                        // Multi-sub-pane restore into a specific group.
                        // Same machinery as the single-group path; only
                        // the push API differs.
                        if let Some(tree) = build_multi_sub_pane_tree(
                            tab,
                            ordinal,
                            &mut pane_buffers,
                            cwd.clone(),
                            theme,
                            density,
                            typography.clone(),
                            window,
                            cx,
                        ) {
                            panes_entity.update(cx, |p, cx| {
                                p.push_restored_terminal_tab_with_tree_in(
                                    group_id,
                                    tab.label.clone(),
                                    tree,
                                    cx,
                                );
                            });
                            ordinal += 1;
                        }
                    } else if let Some(view) = build_terminal_view_for_tab(
                        cwd.clone(),
                        &attach_hints,
                        ordinal,
                        single_leaf_ids(tab, &cwd),
                        theme,
                        density,
                        typography.clone(),
                        window,
                        cx,
                    ) {
                        // See the single-group path: content restore is the
                        // daemon reattach only; no lossy serialized-grid replay.
                        let _ = pane_buffers.remove(&(ordinal, 0));
                        panes_entity.update(cx, |p, cx| {
                            p.push_restored_terminal_tab_in(group_id, tab.label.clone(), view, cx);
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
                tracing::warn!(
                    ?err,
                    adapter = adapter_id,
                    "agent restore: start_session failed"
                );
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

/// Spawn one fresh PTY + TerminalView per leaf in `tab.sub_panes`, then
/// fold them into a `TerminalSplitTree` matching the persisted shape.
/// Each leaf inherits the cwd captured at snapshot time (or
/// `project_cwd` when missing/invalid) and prefills its grid with any
/// scrollback bytes saved at `(tab_ordinal, sub_pane_ordinal)`. Returns
/// `None` when at least one PTY spawn fails — the whole tab is dropped
/// rather than leaving a partially-restored tree with phantom slots.
/// Whether a terminal tab needs the full sub-pane tree restore path
/// (dormant-spawn per leaf/tab) rather than the single-view fast path:
/// it has more than one split leaf, OR any leaf carries a multi-tab
/// per-pane strip. The one-leaf/one-tab case correctly returns `false`
/// and takes the fast path, which still recovers that leaf's persisted
/// surface/tab ids via `single_leaf_ids` (reads `sub_panes.first()`) and
/// can relay-reattach the surviving PTY.
fn needs_tree_restore(tab: &PersistedTab) -> bool {
    tab.sub_panes.len() > 1 || tab.sub_panes.iter().any(|sp| sp.tabs.len() > 1)
}

#[allow(clippy::too_many_arguments)]
fn build_multi_sub_pane_tree(
    tab: &PersistedTab,
    tab_ordinal: u32,
    pane_buffers: &mut PaneBuffersMap,
    project_cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<TerminalSplitTree> {
    type LeafSpec = (Vec<(Entity<TerminalView>, gpui::Subscription)>, usize);
    let mut leaves: Vec<LeafSpec> = Vec::with_capacity(tab.sub_panes.len());
    for (sub_pane_ordinal, sp) in tab.sub_panes.iter().enumerate() {
        // A leaf's tabs come from its explicit `tabs` list (per-pane tab
        // strip), or — for legacy / single-tab leaves — a single tab built
        // from the top-level cwd/surface_id/tab_id fields.
        let leaf_tabs: Vec<(Option<String>, String, String)> = if sp.tabs.is_empty() {
            vec![(sp.cwd.clone(), sp.surface_id.clone(), sp.tab_id.clone())]
        } else {
            sp.tabs
                .iter()
                .map(|t| (t.cwd.clone(), t.surface_id.clone(), t.tab_id.clone()))
                .collect()
        };
        let active_tab = sp.active_tab.min(leaf_tabs.len().saturating_sub(1));
        // Only the leaf's ACTIVE tab inherits the captured scrollback
        // (buffers are keyed per leaf, not per per-pane tab — finer-grained
        // scrollback capture is a follow-up); other tabs start empty.
        let active_prefill = pane_buffers
            .remove(&(tab_ordinal, sub_pane_ordinal as u32))
            .unwrap_or_default();
        let mut tab_views: Vec<(Entity<TerminalView>, gpui::Subscription)> =
            Vec::with_capacity(leaf_tabs.len());
        for (ti, (cwd_opt, surface_id, tab_id)) in leaf_tabs.into_iter().enumerate() {
            let leaf_cwd = resolve_cwd(cwd_opt.as_deref(), &project_cwd);
            // F3.4 slice 2: spawn each tab DORMANT (grid emulator, no PTY
            // child). First focus-in/keystroke wakes it via respawn.
            let Some((backend, session_id)) = spawn_local_pty_dormant(80, 24) else {
                tracing::warn!(label = %tab.label, "sub-pane spawn_dormant failed; dropping tab");
                return None;
            };
            let ids = SurfaceIds::restored(
                project_cwd.to_string_lossy().into_owned(),
                surface_id,
                tab_id,
            );
            let empty: Vec<u8> = Vec::new();
            let prefill: &[u8] = if ti == active_tab {
                &active_prefill
            } else {
                &empty
            };
            let view = cx.new(|cx| {
                TerminalView::mount_dormant(
                    backend,
                    session_id,
                    ids,
                    leaf_cwd,
                    prefill,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            });
            let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
            tab_views.push((view, observer));
        }
        leaves.push((tab_views, active_tab));
    }
    Some(TerminalSplitTree::from_persisted(
        &tab.tree,
        leaves,
        tab.active_sub_pane,
    ))
}

/// Validate + materialize a persisted cwd. Falls back to `project_cwd`
/// when the saved cwd is missing, blank, or no longer exists on disk.
/// Avoids spawning a shell into a stale directory.
fn resolve_cwd(cwd: Option<&str>, project_cwd: &std::path::Path) -> PathBuf {
    match cwd.map(PathBuf::from) {
        Some(p) if p.is_dir() => p,
        _ => project_cwd.to_path_buf(),
    }
}

fn static_adapter_id(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

/// Rebuild the identity triple for a single-leaf terminal tab from its
/// first persisted sub-pane (the only leaf). Empty/absent ids are minted
/// fresh inside `SurfaceIds::restored`.
fn single_leaf_ids(tab: &PersistedTab, cwd: &std::path::Path) -> SurfaceIds {
    let sp = tab.sub_panes.first();
    SurfaceIds::restored(
        cwd.to_string_lossy().into_owned(),
        sp.map(|s| s.surface_id.clone()).unwrap_or_default(),
        sp.map(|s| s.tab_id.clone()).unwrap_or_default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_terminal_view_for_tab(
    cwd: PathBuf,
    attach_hints: &AttachHints,
    ordinal: u32,
    ids: SurfaceIds,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<TerminalView>> {
    let (backend, session_id) = if let Some(relay_pty_id) = attach_hints.get(&ordinal)
        && let Some(result) = attach_pty_existing(relay_pty_id)
    {
        // Reattach to a surviving daemon PTY: the live shell already
        // carries its original OXIMUX_* env, so no env is injected here.
        result
    } else {
        spawn_local_pty(cwd, ids.env())?
    };
    Some(cx.new(|cx| {
        TerminalView::mount(
            backend, session_id, ids, theme, density, typography, window, cx,
        )
    }))
}

pub(crate) fn load_persisted_tabs(
    repo: &SettingsRepo,
    project_id: &str,
    window_id: &str,
) -> Option<PersistedTabs> {
    // Try the per-window key first.
    let key = settings_key(project_id, window_id);
    let raw_opt = match repo.get(&key) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "load_persisted_tabs: settings.get failed"
            );
            return None;
        }
    };
    // For the first window, fall back to the legacy (pre-V005) key so
    // existing single-window users' tab layouts survive the upgrade.
    let raw = match raw_opt {
        Some(r) => r,
        None if window_id == "main" => {
            let legacy_key = legacy_settings_key(project_id);
            match repo.get(&legacy_key) {
                Ok(Some(r)) => r,
                Ok(None) => return None,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        project_id,
                        "load_persisted_tabs: legacy settings.get failed"
                    );
                    return None;
                }
            }
        }
        None => return None,
    };
    match serde_json::from_str::<PersistedTabs>(&raw) {
        Ok(snap) => Some(snap),
        Err(err) => {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "load_persisted_tabs: parse failed; ignoring"
            );
            None
        }
    }
}

pub(crate) fn load_pane_buffers(
    repo: &PaneBufferRepo,
    project_id: &str,
    window_id: &str,
) -> PaneBuffersMap {
    match repo.get_all_for_project(project_id, window_id) {
        Ok(rows) => rows
            .into_iter()
            .map(|(ord, sub_ord, bytes)| ((ord, sub_ord), bytes))
            .collect(),
        Err(err) => {
            tracing::warn!(?err, project_id, window_id, "load_pane_buffers: get failed");
            HashMap::new()
        }
    }
}

pub(crate) fn save_persisted_tabs(
    repo: &SettingsRepo,
    project_id: &str,
    window_id: &str,
    snap: &PersistedTabs,
) {
    let key = settings_key(project_id, window_id);
    let json = match serde_json::to_string(snap) {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "save_persisted_tabs: serialize failed"
            );
            return;
        }
    };
    if let Err(err) = repo.set(&key, &json) {
        tracing::warn!(
            ?err,
            project_id,
            window_id,
            "save_persisted_tabs: settings.set failed"
        );
    }
}

/// Read the open-windows manifest. Returns an empty manifest (→ legacy
/// single-window boot) when the key is absent or fails to parse. `pub` so
/// the binary crate's boot path can decide how many windows to reopen.
pub fn load_windows_manifest(repo: &SettingsRepo) -> WindowsManifest {
    match repo.get(WINDOWS_MANIFEST_KEY) {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(?err, "load_windows_manifest: parse failed; ignoring");
            WindowsManifest::default()
        }),
        Ok(None) => WindowsManifest::default(),
        Err(err) => {
            tracing::warn!(?err, "load_windows_manifest: settings.get failed");
            WindowsManifest::default()
        }
    }
}

/// Persist the open-windows manifest (called from the quit / last-window
/// capture path). `pub` so the lib-level session-capture helper can save it.
pub fn save_windows_manifest(repo: &SettingsRepo, manifest: &WindowsManifest) {
    match serde_json::to_string(manifest) {
        Ok(json) => {
            if let Err(err) = repo.set(WINDOWS_MANIFEST_KEY, &json) {
                tracing::warn!(?err, "save_windows_manifest: settings.set failed");
            }
        }
        Err(err) => tracing::warn!(?err, "save_windows_manifest: serialize failed"),
    }
}

#[cfg(test)]
mod tests {
    //! Pure-logic coverage for the restore-factory decision helpers. These
    //! gate the boot-time restore path: which terminal tabs need the full
    //! sub-pane tree rebuild vs the single-view fast path, how a stale cwd
    //! is resolved, and how a single-leaf tab's identity triple is
    //! recovered (or minted when the blob predates context env). No GPUI
    //! context is needed — the functions are deterministic over plain data.
    use super::*;
    use crate::persisted_terminals::{PersistedLeafTab, PersistedSubPane};
    use std::path::Path;

    fn leaf_sub_pane(surface: &str, tab: &str) -> PersistedSubPane {
        PersistedSubPane {
            surface_id: surface.into(),
            tab_id: tab.into(),
            ..PersistedSubPane::default()
        }
    }

    #[test]
    fn needs_tree_restore_false_for_single_leaf_single_tab() {
        // One leaf, one implicit tab → fast single-view path, no rebuild.
        let tab = PersistedTab {
            sub_panes: vec![leaf_sub_pane("s0", "t0")],
            ..PersistedTab::default()
        };
        assert!(!needs_tree_restore(&tab));
    }

    #[test]
    fn needs_tree_restore_true_for_multiple_leaves() {
        let tab = PersistedTab {
            sub_panes: vec![leaf_sub_pane("s0", "t0"), leaf_sub_pane("s1", "t1")],
            ..PersistedTab::default()
        };
        assert!(needs_tree_restore(&tab));
    }

    #[test]
    fn needs_tree_restore_true_for_single_leaf_multi_tab_strip() {
        // One leaf, but its per-pane tab strip holds 2 terminals. The fast
        // single-view path would silently drop the second tab, so the
        // restorer MUST take the full tree path here.
        let mut sp = leaf_sub_pane("s0", "t0");
        sp.tabs = vec![
            PersistedLeafTab {
                cwd: None,
                surface_id: "s0".into(),
                tab_id: "t0".into(),
            },
            PersistedLeafTab {
                cwd: None,
                surface_id: "s1".into(),
                tab_id: "t1".into(),
            },
        ];
        let tab = PersistedTab {
            sub_panes: vec![sp],
            ..PersistedTab::default()
        };
        assert!(needs_tree_restore(&tab));
    }

    #[test]
    fn resolve_cwd_keeps_existing_dir() {
        // The crate manifest dir is guaranteed to exist on disk.
        let real = env!("CARGO_MANIFEST_DIR");
        assert_eq!(
            resolve_cwd(Some(real), Path::new("/tmp")),
            PathBuf::from(real)
        );
    }

    #[test]
    fn resolve_cwd_falls_back_when_missing_or_absent() {
        let project = Path::new("/tmp");
        // A path that no longer exists → fall back to the project cwd
        // rather than spawning a shell into a stale directory.
        assert_eq!(
            resolve_cwd(Some("/no/such/dir/oximux-xyz-123"), project),
            project.to_path_buf()
        );
        // No captured cwd → project cwd.
        assert_eq!(resolve_cwd(None, project), project.to_path_buf());
    }

    #[test]
    fn single_leaf_ids_preserves_persisted_ids() {
        let tab = PersistedTab {
            sub_panes: vec![leaf_sub_pane("surf-keep", "tab-keep")],
            ..PersistedTab::default()
        };
        let ids = single_leaf_ids(&tab, Path::new("/proj/root"));
        assert_eq!(ids.workspace_id, "/proj/root");
        assert_eq!(ids.surface_id, "surf-keep");
        assert_eq!(ids.tab_id, "tab-keep");
    }

    #[test]
    fn single_leaf_ids_mints_when_absent() {
        // Legacy blob with no sub-panes → ids minted fresh, never empty,
        // so every restored terminal still has a stable identity going on.
        let tab = PersistedTab::default();
        let ids = single_leaf_ids(&tab, Path::new("/proj/root"));
        assert_eq!(ids.workspace_id, "/proj/root");
        assert!(!ids.surface_id.is_empty(), "surface id must be minted");
        assert!(!ids.tab_id.is_empty(), "tab id must be minted");
    }
}
