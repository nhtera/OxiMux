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
    PersistedAgentTab, PersistedAxis, PersistedSubPane, PersistedTab, PersistedTabKind,
    PersistedTabs, PersistedTree, WINDOWS_MANIFEST_KEY, WindowsManifest, legacy_settings_key,
    settings_key,
};
use crate::shell::context_env::SurfaceIds;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;
use crate::shell::terminal_view::{
    TerminalView, attach_pty_existing, relay_state_snapshot, spawn_pending_placeholder_grid,
};
use crate::workspace_root::WorkspaceRoot;

const DEFAULT_AGENT_COLS: u16 = 120;
const DEFAULT_AGENT_ROWS: u16 = 32;

/// Per-pane cap on captured scrollback bytes. 512 KiB matches the
/// reference cockpit; covers ~5k rows × 80 cols of typical output even
/// with SGR runs.
pub const PANE_BUFFER_MAX_BYTES: usize = 512 * 1024;

/// Per-ordinal hint: "this tab should attach to this surviving daemon
/// PTY id instead of spawning fresh." Used by the single-pane fast path,
/// which only ever has one leaf/tab — its key is `(ordinal, 0, 0)`.
pub type AttachHints = HashMap<u32, String>;

/// Per-leaf-tab hint keyed `(ordinal, sub_pane, tab)`: "this split leaf /
/// per-pane tab should re-attach to this surviving daemon PTY id instead
/// of dormant-respawning." Drives the tree restore path.
pub type LeafAttachHints = HashMap<(u32, u32, u32), String>;

/// A persisted relay-id row: `(ordinal, sub_pane, tab, relay_pty_id,
/// relay_session)` as returned by `PaneRelayIdRepo::get_all_for_project`.
type RelayRow = (u32, u32, u32, String, String);

/// F3.4 — per-sub-pane scrollback bytes keyed by `(tab_ordinal,
/// sub_pane_ordinal)`. Replaces the pre-v4 flat `Vec<Vec<u8>>` so
/// multi-sub-pane tabs prefill every leaf's grid independently. Bytes
/// for pre-v4 rows resolve to `sub_pane_ordinal = 0` (via the V004
/// migration), keeping single-sub-pane tabs identical to the old path.
pub type PaneBuffersMap = HashMap<(u32, u32), Vec<u8>>;

/// Which restore slot a pending terminal view occupies, so the reconcile
/// pass can look up its survival hint in the maps `compute_attach_hints` /
/// `compute_leaf_attach_hints` produce.
pub(crate) enum PendingSlot {
    /// Single-pane fast path, keyed by tab ordinal (leaf 0 / tab 0 row).
    SingleTab(u32),
    /// Tree path, keyed `(ordinal, sub_pane, tab)`.
    LeafTab(u32, u32, u32),
}

/// One restored terminal surface awaiting its post-paint session: the
/// pending view to deliver into, the slot for hint lookup, and the
/// cwd/env a fresh spawn needs when no daemon PTY survived.
pub(crate) struct PendingAttach {
    pub view: WeakEntity<TerminalView>,
    pub slot: PendingSlot,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Raw (unvalidated) persisted relay id for a slot — what the pending view
/// carries for the quit-save capture fallback. Liveness/session validation
/// happens in the reconcile pass against a fresh daemon snapshot.
fn raw_relay_hint(rows: &[RelayRow], ord: u32, sub_pane: u32, tab: u32) -> Option<String> {
    rows.iter()
        .find(|(o, s, t, _, _)| *o == ord && *s == sub_pane && *t == tab)
        .map(|(_, _, _, pty_id, _)| pty_id.clone())
}

/// True when a persisted relay row still points at a live, same-session
/// daemon PTY — i.e. it survived the restart and can be re-attached.
fn relay_row_survives(
    session: &str,
    pty_id: &str,
    live_external_ids: &HashSet<String>,
    current_external_session: Option<&str>,
) -> bool {
    let session_ok = current_external_session.map(|s| s == session).unwrap_or(false);
    session_ok && live_external_ids.contains(pty_id)
}

/// Pre-filter persisted relay rows down to the single-pane tabs (leaf 0,
/// tab 0) that should re-attach, keyed by tab `ordinal`. Tree tabs use
/// [`compute_leaf_attach_hints`] instead.
pub fn compute_attach_hints(
    pane_relay_ids: &[RelayRow],
    live_external_ids: &HashSet<String>,
    current_external_session: Option<&str>,
) -> AttachHints {
    pane_relay_ids
        .iter()
        .filter_map(|(ord, sub_pane, tab, pty_id, session)| {
            (*sub_pane == 0
                && *tab == 0
                && relay_row_survives(session, pty_id, live_external_ids, current_external_session))
            .then(|| (*ord, pty_id.clone()))
        })
        .collect()
}

/// Pre-filter persisted relay rows down to the surviving leaf-tabs that
/// should re-attach, keyed `(ordinal, sub_pane, tab)`. Drives the tree
/// restore path so split leaves / per-pane tabs adopt their live daemon
/// PTYs instead of dormant-respawning.
pub fn compute_leaf_attach_hints(
    pane_relay_ids: &[RelayRow],
    live_external_ids: &HashSet<String>,
    current_external_session: Option<&str>,
) -> LeafAttachHints {
    pane_relay_ids
        .iter()
        .filter_map(|(ord, sub_pane, tab, pty_id, session)| {
            relay_row_survives(session, pty_id, live_external_ids, current_external_session)
                .then(|| ((*ord, *sub_pane, *tab), pty_id.clone()))
        })
        .collect()
}

/// Build the per-project panes entity from the persisted snapshot WITHOUT
/// touching the relay daemon: every restored terminal mounts as a
/// pending placeholder (in-process dormant grid, zero round-trips) and is
/// returned in the `Vec<PendingAttach>` for the caller to hand to
/// [`spawn_attach_reconcile`] AFTER the window has painted. This is what
/// keeps N restored tabs from gating first paint behind N sequential
/// daemon RPCs — the window shows the restored layout immediately and
/// each pane's content streams in as its session attaches.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_project_panes(
    cwd: PathBuf,
    snapshot: Option<PersistedTabs>,
    pane_buffers: PaneBuffersMap,
    pane_relay_ids: Vec<RelayRow>,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> (Entity<ProjectPanes>, Vec<PendingAttach>) {
    let mut pending: Vec<PendingAttach> = Vec::new();
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
        return (panes_entity, pending);
    };

    // v3 path: multi-group restore when `group_tree` + `groups` are
    // present. Falls through to the legacy single-group path otherwise.
    // Clone the tree out so we can hand it (by reference) into the
    // helper while still moving `snap` for the per-group iteration.
    if let (Some(tree), false) = (snap.group_tree.clone(), snap.groups.is_empty()) {
        let entity = restore_multi_group(
            panes_entity,
            snap,
            &tree,
            pane_buffers,
            &pane_relay_ids,
            &mut pending,
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            window,
            cx,
        );
        return (entity, pending);
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
                    // Multi-sub-pane restore — every leaf mounts pending;
                    // the reconcile pass attaches/spawns post-paint.
                    if let Some(tree) = build_multi_sub_pane_tree(
                        tab,
                        ordinal,
                        &mut pane_buffers,
                        &pane_relay_ids,
                        &mut pending,
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
                } else if let Some(view) = build_pending_terminal_view(
                    cwd.clone(),
                    &pane_relay_ids,
                    &mut pending,
                    ordinal,
                    single_leaf_ids(tab, &cwd),
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                ) {
                    // Content restore comes ONLY from a live daemon reattach
                    // (delivered by the post-paint reconcile). When no live
                    // PTY exists, the tab opens a clean fresh shell — we
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
    (panes_entity, pending)
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
    pane_relay_ids: &[RelayRow],
    pending: &mut Vec<PendingAttach>,
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
                            pane_relay_ids,
                            pending,
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
                    } else if let Some(view) = build_pending_terminal_view(
                        cwd.clone(),
                        pane_relay_ids,
                        pending,
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
    let adapter_id: &'static str = static_adapter_id(persisted.adapter);
    // On a respawn (PTY no longer alive in the daemon) re-apply the current
    // per-agent launch flags so a restored agent comes back with the same
    // defaults a fresh launch would use. Ignored on warm re-attach, which
    // adopts the already-running process and never reads cfg.
    let extra_args = cx
        .try_global::<oximux_settings::AgentLaunchSettings>()
        .map(|d| d.args_for(adapter_id))
        .unwrap_or_default();
    let cfg = AgentSessionConfig {
        adapter: persisted.adapter,
        worktree_path: PathBuf::from(&persisted.worktree_path),
        prompt: None,
        model: persisted.model.clone(),
        effort: persisted.effort.clone(),
        extra_args,
        env: Vec::new(),
        cols: DEFAULT_AGENT_COLS,
        rows: DEFAULT_AGENT_ROWS,
        custom_command: None,
    };
    let persisted_clone = persisted.clone();
    cx.spawn_in(window, async move |root, cx| {
        // Warm re-attach: if the agent's PTY is still alive in the relay
        // daemon (same session id), adopt the running CLI instead of
        // respawning — the conversation + scrollback resume exactly where
        // they were, identical to plain-terminal restore. `None` falls
        // through to a fresh respawn (which also routes through the daemon,
        // so a cold-restored agent survives the NEXT quit).
        // Both relay calls are blocking daemon round-trips; run them on the
        // background executor — this async closure itself executes on the
        // main thread (same discipline as `spawn_attach_reconcile`).
        let reattached = {
            let Ok(executor) = cx.update(|_, cx| cx.background_executor().clone()) else {
                return;
            };
            let relay_session = persisted_clone.relay_session.clone();
            let relay_external_id = persisted_clone.relay_external_id.clone();
            executor
                .spawn(async move {
                    let snap = relay_state_snapshot();
                    let session_ok = matches!(
                        (&relay_session, &snap.session_id),
                        (Some(s), Some(c)) if s == c
                    );
                    relay_external_id.as_deref().and_then(|ext| {
                        (session_ok && snap.live_external_ids.contains(ext))
                            .then(|| attach_pty_existing(ext))
                            .flatten()
                    })
                })
                .await
        };

        let attached = if let Some((backend, term_id)) = reattached {
            match cli_runtime.adopt_session(persisted_clone.adapter, backend.clone(), term_id) {
                Ok(session_id) => match cli_runtime.subscribe_status(session_id) {
                    Ok(status_rx) => Some((session_id, backend, term_id, status_rx)),
                    Err(err) => {
                        tracing::warn!(?err, "agent restore: subscribe_status (reattach) failed");
                        let _ = cli_runtime.cancel(session_id).await;
                        None
                    }
                },
                Err(err) => {
                    tracing::warn!(?err, "agent restore: adopt_session failed; respawning");
                    None
                }
            }
        } else {
            None
        };

        let (session_id, backend, term_id, status_rx) = match attached {
            Some(t) => t,
            None => {
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
                (session_id, backend, term_id, status_rx)
            }
        };
        // Restored sessions get a fresh agent_sessions row too (the boot
        // sweep already marked the pre-restart row Interrupted); without
        // this, a live re-adopted agent would read "Stopped" on the rail.
        let _ = root.update(cx, |this, cx| {
            crate::shell::agent_session_persistence::spawn_for_session(
                this,
                persisted_clone.worktree_path.clone(),
                adapter_id,
                persisted_clone.model.clone(),
                persisted_clone.effort.clone(),
                status_rx.clone(),
                cx,
            );
        });
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

/// One terminal per pane (single-terminal-per-pane model): expand any persisted leaf that held
/// more than one terminal (a legacy per-pane tab strip) into a horizontal
/// split of single-terminal leaves. Leaves that already hold one terminal
/// pass through unchanged, so a fully-migrated snapshot round-trips
/// untouched (idempotent — safe to run on every restore). Returns the
/// rewritten layout tree, the flattened one-terminal-per-leaf list (DFS
/// order), and the remapped active-leaf DFS position.
fn promote_per_pane_tabs(
    tree: &PersistedTree,
    sub_panes: &[PersistedSubPane],
    active_sub_pane: usize,
) -> (PersistedTree, Vec<PersistedSubPane>, usize) {
    fn single(cwd: Option<String>, surface_id: String, tab_id: String) -> PersistedSubPane {
        PersistedSubPane {
            cwd,
            surface_id,
            tab_id,
            tabs: Vec::new(),
            active_tab: 0,
        }
    }
    fn walk(
        node: &PersistedTree,
        sub_panes: &[PersistedSubPane],
        next_old_leaf: &mut usize,
        out: &mut Vec<PersistedSubPane>,
        active_old_leaf: usize,
        active_new_pos: &mut Option<usize>,
    ) -> PersistedTree {
        match node {
            PersistedTree::Leaf => {
                let old_idx = *next_old_leaf;
                *next_old_leaf += 1;
                let sp = sub_panes.get(old_idx).cloned().unwrap_or_default();
                // Empty `tabs` = one implicit terminal carried by the leaf's
                // top-level fields; otherwise one terminal per per-pane tab.
                let terminals: Vec<PersistedSubPane> = if sp.tabs.is_empty() {
                    vec![single(sp.cwd.clone(), sp.surface_id.clone(), sp.tab_id.clone())]
                } else {
                    sp.tabs
                        .iter()
                        .map(|t| single(t.cwd.clone(), t.surface_id.clone(), t.tab_id.clone()))
                        .collect()
                };
                let base = out.len();
                if old_idx == active_old_leaf {
                    let at = if sp.tabs.is_empty() {
                        0
                    } else {
                        sp.active_tab.min(terminals.len().saturating_sub(1))
                    };
                    *active_new_pos = Some(base + at);
                }
                let n = terminals.len();
                out.extend(terminals);
                if n <= 1 {
                    PersistedTree::Leaf
                } else {
                    PersistedTree::Split {
                        axis: PersistedAxis::Horizontal,
                        children: (0..n).map(|_| PersistedTree::Leaf).collect(),
                        weights: vec![1.0 / n as f32; n],
                    }
                }
            }
            PersistedTree::Split {
                axis,
                children,
                weights,
            } => PersistedTree::Split {
                axis: *axis,
                children: children
                    .iter()
                    .map(|c| {
                        walk(
                            c,
                            sub_panes,
                            next_old_leaf,
                            out,
                            active_old_leaf,
                            active_new_pos,
                        )
                    })
                    .collect(),
                weights: weights.clone(),
            },
        }
    }
    let mut out = Vec::with_capacity(sub_panes.len());
    let mut next_old_leaf = 0usize;
    let mut active_new_pos = None;
    let new_tree = walk(
        tree,
        sub_panes,
        &mut next_old_leaf,
        &mut out,
        active_sub_pane,
        &mut active_new_pos,
    );
    let active = active_new_pos.unwrap_or(0).min(out.len().saturating_sub(1));
    (new_tree, out, active)
}

#[allow(clippy::too_many_arguments)]
fn build_multi_sub_pane_tree(
    tab: &PersistedTab,
    tab_ordinal: u32,
    pane_buffers: &mut PaneBuffersMap,
    pane_relay_ids: &[RelayRow],
    pending: &mut Vec<PendingAttach>,
    project_cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<TerminalSplitTree> {
    type LeafSpec = (Vec<(Entity<TerminalView>, gpui::Subscription)>, usize);
    // single-terminal-per-pane model: one terminal per pane. Expand any legacy per-pane tab
    // strip into split leaves before building views, so each pane carries a
    // single terminal. Idempotent for already-single-terminal snapshots.
    let (tree, sub_panes, active_sub_pane) =
        promote_per_pane_tabs(&tab.tree, &tab.sub_panes, tab.active_sub_pane);
    let mut leaves: Vec<LeafSpec> = Vec::with_capacity(sub_panes.len());
    for (sub_pane_ordinal, sp) in sub_panes.iter().enumerate() {
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
        // Cold restore no longer replays a serialized grid — same rationale
        // as the single-pane path: a saved grid reflows to the new pane size
        // and scrambles full-screen TUIs, so content comes only from a live
        // daemon re-attach. Drain the captured buffer so it doesn't linger.
        let _ = pane_buffers.remove(&(tab_ordinal, sub_pane_ordinal as u32));
        let mut tab_views: Vec<(Entity<TerminalView>, gpui::Subscription)> =
            Vec::with_capacity(leaf_tabs.len());
        for (ti, (cwd_opt, surface_id, tab_id)) in leaf_tabs.into_iter().enumerate() {
            let leaf_cwd = resolve_cwd(cwd_opt.as_deref(), &project_cwd);
            let ids = SurfaceIds::restored(
                project_cwd.to_string_lossy().into_owned(),
                surface_id,
                tab_id,
            );
            // Every leaf mounts as a pending placeholder — zero daemon
            // round-trips here. The post-paint reconcile decides warm
            // re-attach (relay PTY survived: same session, still live) vs
            // cold spawn per leaf and delivers the session via
            // `adopt_live_session`. The fresh-spawn path carries the leaf's
            // OXIMUX_* env via the `PendingAttach` entry.
            let slot = (tab_ordinal, sub_pane_ordinal as u32, ti as u32);
            let relay_hint = raw_relay_hint(pane_relay_ids, slot.0, slot.1, slot.2);
            let Some((backend, session_id)) = spawn_pending_placeholder_grid() else {
                tracing::warn!(label = %tab.label, "sub-pane placeholder grid failed; dropping tab");
                return None;
            };
            let env = ids.env();
            let view = cx.new(|cx| {
                TerminalView::mount_pending(
                    backend,
                    session_id,
                    ids,
                    relay_hint,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            });
            pending.push(PendingAttach {
                view: view.downgrade(),
                slot: PendingSlot::LeafTab(slot.0, slot.1, slot.2),
                cwd: leaf_cwd,
                env,
            });
            let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
            tab_views.push((view, observer));
        }
        leaves.push((tab_views, active_tab));
    }
    Some(TerminalSplitTree::from_persisted(
        &tree,
        leaves,
        active_sub_pane,
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

/// Single-pane fast-path restore: mount a pending placeholder (zero
/// daemon round-trips) and register it for the post-paint reconcile,
/// which decides warm re-attach vs cold spawn off the paint path.
#[allow(clippy::too_many_arguments)]
fn build_pending_terminal_view(
    cwd: PathBuf,
    pane_relay_ids: &[RelayRow],
    pending: &mut Vec<PendingAttach>,
    ordinal: u32,
    ids: SurfaceIds,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<TerminalView>> {
    let relay_hint = raw_relay_hint(pane_relay_ids, ordinal, 0, 0);
    let (backend, session_id) = spawn_pending_placeholder_grid()?;
    let env = ids.env();
    let view = cx.new(|cx| {
        TerminalView::mount_pending(
            backend,
            session_id,
            ids,
            relay_hint,
            theme,
            density,
            typography,
            window,
            cx,
        )
    });
    pending.push(PendingAttach {
        view: view.downgrade(),
        slot: PendingSlot::SingleTab(ordinal),
        cwd,
        env,
    });
    Some(view)
}

/// Post-paint attach reconcile: ONE `ListPtys` round-trip validates the
/// persisted hints against the live daemon, then each pending view gets
/// its session — warm re-attach when its PTY survived, cold spawn
/// otherwise — delivered via `adopt_live_session`. Every daemon RPC runs
/// on the background executor (each is a `Handle::block_on` against the
/// relay runtime and must stay off the main thread); only the per-view
/// delivery hops back to the main thread. Tabs go live one by one, the
/// UI stays interactive throughout.
pub(crate) fn spawn_attach_reconcile(
    pane_relay_ids: Vec<RelayRow>,
    pending: Vec<PendingAttach>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) {
    if pending.is_empty() {
        return;
    }
    cx.spawn_in(window, async move |root, cx| {
        let started = std::time::Instant::now();
        let total = pending.len();
        let Ok(executor) = cx.update(|_, cx| cx.background_executor().clone()) else {
            return;
        };
        let snap = executor
            .spawn(async move { relay_state_snapshot() })
            .await;
        let attach_hints =
            compute_attach_hints(&pane_relay_ids, &snap.live_external_ids, snap.session_id.as_deref());
        let leaf_attach_hints = compute_leaf_attach_hints(
            &pane_relay_ids,
            &snap.live_external_ids,
            snap.session_id.as_deref(),
        );
        let checkpoints_dir = crate::relay_cold_restore::default_checkpoints_dir();
        let mut attached = 0usize;
        let mut spawned = 0usize;
        let mut cold_restored = 0usize;
        let mut cwd_only_restored = 0usize;
        for entry in pending {
            let hint = match &entry.slot {
                PendingSlot::SingleTab(ord) => attach_hints.get(ord).cloned(),
                PendingSlot::LeafTab(ord, sub, tab) => {
                    leaf_attach_hints.get(&(*ord, *sub, *tab)).cloned()
                }
            };
            // The RAW persisted id (no liveness validation) keys the
            // daemon's disk checkpoint for this slot — when the warm
            // path fails, it's how the cold path finds the dead PTY's
            // recovered scrollback.
            let raw_hint = match &entry.slot {
                PendingSlot::SingleTab(ord) => raw_relay_hint(&pane_relay_ids, *ord, 0, 0),
                PendingSlot::LeafTab(ord, sub, tab) => {
                    raw_relay_hint(&pane_relay_ids, *ord, *sub, *tab)
                }
            };
            let was_attach = hint.is_some();
            let cwd = entry.cwd;
            let env = entry.env;
            let ckpt_dir = checkpoints_dir.clone();
            let result = executor
                .spawn(async move {
                    // Warm re-attach first; a failed attach (PTY died between
                    // snapshot and now) falls back to a cold spawn, which
                    // itself falls back to an in-process PTY when the relay
                    // is unreachable.
                    if let Some(live) = hint.as_deref().and_then(attach_pty_existing) {
                        return Some((live, None));
                    }
                    // Cold spawn means the daemon lost this PTY. If it died
                    // uncleanly it left a disk checkpoint — recover the
                    // scrollback here (disk I/O stays off the main thread).
                    let cold = match (&ckpt_dir, raw_hint.as_deref()) {
                        (Some(dir), Some(id)) => {
                            crate::relay_cold_restore::read_cold_restore(dir, id)
                                .map(|restore| (restore, id.to_owned()))
                        }
                        _ => None,
                    };
                    // The checkpoint's cwd is the dead shell's LIVE working
                    // directory (kernel-resolved by the daemon each tick) —
                    // fresher than the persisted layout cwd, so the revived
                    // shell lands where the user actually was. Its (cols,
                    // rows) seed the replacement's initial dims so the first
                    // prompt wraps like the restored content above it.
                    let spawn_cwd = cold
                        .as_ref()
                        .and_then(|(restore, _)| restore.cwd.clone())
                        .unwrap_or(cwd);
                    let spawn_dims = cold.as_ref().and_then(|(restore, _)| restore.dims);
                    crate::shell::terminal_view::spawn_local_pty_sized(spawn_cwd, env, spawn_dims)
                        .map(|session| (session, cold))
                })
                .await;
            let Some(((backend, session_id), cold)) = result else {
                tracing::warn!("attach reconcile: spawn failed; pane stays empty");
                continue;
            };
            let delivered = entry.view.update(cx, |view, cx| {
                let adopted = view.adopt_live_session(backend.clone(), session_id, cx);
                if adopted {
                    if let Some((restore, _)) = &cold
                        && !restore.bytes.is_empty()
                    {
                        // Prefill BEFORE the first poll tick drains the fresh
                        // shell's prompt: recovered history paints first, the
                        // live prompt then appends below the restored marker.
                        // A cwd-only restore (no replayable scrollback) skips
                        // this — blank grid, recovered spawn dir.
                        view.prefill_grid(&restore.bytes);
                    }
                }
                adopted
            });
            match delivered {
                Ok(true) => {
                    if was_attach {
                        attached += 1;
                    } else {
                        spawned += 1;
                    }
                    // The recovered scrollback is on screen — consume the
                    // checkpoint so the same crash never restores twice.
                    // Only on delivery: an undelivered slot's layout entry
                    // survives (quit mid-reconcile) and may legitimately
                    // cold-restore on the NEXT launch. This consumes the
                    // DEAD PTY's checkpoint (the raw persisted hint); the
                    // cold-spawned replacement has a fresh daemon-minted
                    // id, so the live pane's `os_pid()` checkpoint lookup
                    // queries that new id — never this consumed one.
                    if let (Some(dir), Some((restore, pty_id))) = (&checkpoints_dir, &cold) {
                        // Count only content restores; a cwd-only restore
                        // (empty bytes) replayed nothing into the grid and
                        // would mislead crash triage if lumped in.
                        if !restore.bytes.is_empty() {
                            cold_restored += 1;
                        } else {
                            cwd_only_restored += 1;
                        }
                        let dir = dir.clone();
                        let pty_id = pty_id.clone();
                        executor
                            .spawn(async move {
                                crate::relay_cold_restore::consume_checkpoint(&dir, &pty_id);
                            })
                            .detach();
                    }
                }
                Ok(false) | Err(_) => {
                    // View gone (tab closed / quit mid-reconcile) or already
                    // live. A re-ATTACHED session must be detached, not
                    // closed: its daemon PTY predates this boot and the user
                    // expects it to survive (a quit-race capture re-persists
                    // its hint via `relay_id_for_capture`). A fresh SPAWN was
                    // never visible — close it so the daemon doesn't
                    // accumulate orphans. Skipped entirely during app quit
                    // (mirrors `TerminalView::drop`): the daemon outlives the
                    // GUI and the next launch re-reconciles. Detached thread:
                    // either call can block briefly and this loop runs on the
                    // main thread.
                    if crate::shell::terminal_view::APP_QUITTING
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        continue;
                    }
                    std::thread::spawn(move || {
                        if let Ok(mut be) = backend.lock() {
                            let _ = be.drain_events();
                            if was_attach {
                                let _ = be.detach(session_id);
                            } else {
                                let _ = be.close(session_id);
                            }
                        }
                    });
                }
            }
        }
        tracing::info!(
            total,
            attached,
            spawned,
            cold_restored,
            cwd_only_restored,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "post-paint pty attach reconcile done"
        );
        // Persist the post-reconcile reality right away: cold spawns
        // minted NEW pty ids that exist only in memory until the next
        // quit/switch capture — an app crash in that window would leave
        // the table pointing at dead ids and strand this boot's live
        // PTYs. Reuses the session id from the snapshot above (no extra
        // main-thread daemon round-trip). Skipped during quit so it
        // can't race the quit-save's own capture over a half-torn-down
        // pane tree.
        if let Some(session_id) = snap.session_id.as_deref()
            && !crate::shell::terminal_view::APP_QUITTING.load(std::sync::atomic::Ordering::SeqCst)
        {
            let _ = root.update(cx, |root, cx| {
                root.capture_all_pane_relay_ids_with_session(session_id, cx);
            });
        }
    })
    .detach();
}

/// Outcome of reading a persisted layout blob.
pub(crate) enum LoadedTabs {
    /// Nothing persisted for this key — first open of the project.
    Absent,
    /// A healthy, shape-validated snapshot.
    Snapshot(PersistedTabs),
    /// A payload was present but unusable — parse failure (truncated
    /// autosave) or a shape-invariant violation that would crash or
    /// scramble the live tree. The raw bytes are preserved aside as a
    /// `*.corrupt.json`; the caller falls back to the default layout
    /// and surfaces a toast.
    Corrupt,
}

/// Preserve an unusable payload + log, returning [`LoadedTabs::Corrupt`].
fn reject_corrupt_payload(key: &str, raw: &str, reason: &str) -> LoadedTabs {
    let preserved = crate::restore_fallback::corrupt_layouts_dir()
        .and_then(|dir| crate::restore_fallback::preserve_corrupt_payload(&dir, key, raw));
    tracing::error!(
        key,
        reason,
        ?preserved,
        "persisted layout rejected; falling back to default layout"
    );
    LoadedTabs::Corrupt
}

pub(crate) fn load_persisted_tabs(
    repo: &SettingsRepo,
    project_id: &str,
    window_id: &str,
) -> LoadedTabs {
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
            return LoadedTabs::Absent;
        }
    };
    // For the first window, fall back to the legacy (pre-V005) key so
    // existing single-window users' tab layouts survive the upgrade.
    let (used_key, raw) = match raw_opt {
        Some(r) => (key, r),
        None if window_id == "main" => {
            let legacy_key = legacy_settings_key(project_id);
            match repo.get(&legacy_key) {
                Ok(Some(r)) => (legacy_key, r),
                Ok(None) => return LoadedTabs::Absent,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        project_id,
                        "load_persisted_tabs: legacy settings.get failed"
                    );
                    return LoadedTabs::Absent;
                }
            }
        }
        None => return LoadedTabs::Absent,
    };
    let snap = match serde_json::from_str::<PersistedTabs>(&raw) {
        Ok(snap) => snap,
        Err(err) => return reject_corrupt_payload(&used_key, &raw, &format!("parse: {err}")),
    };
    // Shape gate: a parseable blob can still violate tree invariants the
    // live `PaneTree` relies on (weights/children desync, empty splits,
    // NaN weights, group/tab counts out of sync) — those panic or drop
    // panes much later, so they're rejected here, before any live entity
    // is built.
    match crate::restore_fallback::validate_persisted_tabs(&snap) {
        Ok(()) => LoadedTabs::Snapshot(snap),
        Err(err) => reject_corrupt_payload(&used_key, &raw, &format!("shape: {err}")),
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
    // Skip byte-identical writes: the periodic layout autosave calls
    // this every tick whether or not anything changed, and an idle
    // session shouldn't churn SQLite. Keyed per settings key so multiple
    // projects/windows dedupe independently. Quit/switch saves flow
    // through the same gate — skipping an identical write is always
    // correct.
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        json.hash(&mut hasher);
        hasher.finish()
    };
    // A poisoned lock only means some thread panicked mid-access; the
    // map is plain data and stays usable — recover rather than abort a
    // layout save (worst case: one redundant write).
    {
        let last = LAST_SAVED_TABS_HASH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.get(&key) == Some(&digest) {
            return;
        }
    }
    match repo.set(&key, &json) {
        // Record only AFTER the write lands — a failed write must stay
        // "dirty" so the next identical save retries instead of being
        // deduped into a permanent loss.
        Ok(()) => {
            LAST_SAVED_TABS_HASH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, digest);
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "save_persisted_tabs: settings.set failed"
            );
        }
    }
}

// Last-written layout JSON hash per settings key — the dedup gate for
// `save_persisted_tabs`. Process-local: a fresh launch always writes
// its first save, which is the conservative direction.
static LAST_SAVED_TABS_HASH: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

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

    // ── promote_per_pane_tabs: legacy per-pane tabs → split panes ───────────

    fn leaf_tab(surface: &str, tab: &str) -> PersistedLeafTab {
        PersistedLeafTab {
            cwd: None,
            surface_id: surface.into(),
            tab_id: tab.into(),
        }
    }

    #[test]
    fn promote_single_tab_leaf_is_noop() {
        // A single-terminal leaf round-trips unchanged — promotion is safe to
        // run on every restore.
        let sub_panes = vec![leaf_sub_pane("s0", "t0")];
        let (tree, out, active) = promote_per_pane_tabs(&PersistedTree::Leaf, &sub_panes, 0);
        assert!(matches!(tree, PersistedTree::Leaf));
        assert_eq!(out.len(), 1);
        assert_eq!(active, 0);
        assert_eq!(out[0].surface_id, "s0");
        assert!(out[0].tabs.is_empty());
    }

    #[test]
    fn promote_multi_tab_leaf_becomes_split() {
        // One leaf holding 2 per-pane tabs → a horizontal split of 2
        // single-terminal leaves; the active tab maps to the active leaf.
        let mut sp = leaf_sub_pane("s0", "t0");
        sp.tabs = vec![leaf_tab("s0", "t0"), leaf_tab("s1", "t1")];
        sp.active_tab = 1;
        let (tree, out, active) = promote_per_pane_tabs(&PersistedTree::Leaf, &[sp], 0);
        match tree {
            PersistedTree::Split { children, .. } => assert_eq!(children.len(), 2),
            PersistedTree::Leaf => panic!("multi-tab leaf must become a split"),
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].surface_id, "s0");
        assert_eq!(out[1].surface_id, "s1");
        assert!(out.iter().all(|sp| sp.tabs.is_empty()));
        assert_eq!(active, 1, "active per-pane tab maps to the new active leaf");
    }

    #[test]
    fn promote_mixed_split_expands_only_multi_tab_leaf() {
        // Split[ leaf(1 tab), leaf(2 tabs) ] → Split[ Leaf, Split[Leaf,Leaf] ]
        // with 3 single-terminal leaves total. Active leaf index shifts past
        // the expanded sibling.
        let mut multi = leaf_sub_pane("s1", "t1");
        multi.tabs = vec![leaf_tab("s1", "t1"), leaf_tab("s2", "t2")];
        let sub_panes = vec![leaf_sub_pane("s0", "t0"), multi];
        let tree = PersistedTree::Split {
            axis: PersistedAxis::Vertical,
            children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
            weights: vec![0.5, 0.5],
        };
        // Active was the second (multi-tab) leaf, its tab 0.
        let (new_tree, out, active) = promote_per_pane_tabs(&tree, &sub_panes, 1);
        assert_eq!(out.len(), 3);
        let PersistedTree::Split { children, .. } = &new_tree else {
            panic!("root stays a split");
        };
        assert!(matches!(children[0], PersistedTree::Leaf));
        assert!(
            matches!(&children[1], PersistedTree::Split { children, .. } if children.len() == 2)
        );
        assert_eq!(active, 1, "first terminal of the expanded leaf is at index 1");
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

    // ── compute_attach_hints: the reattach-reconciliation gate ─────────────
    //
    // On reload this decides, per persisted leaf, whether to REATTACH to a
    // surviving daemon PTY or RESPAWN a fresh shell. A hint is emitted only
    // when BOTH hold: the persisted relay session matches the current daemon
    // session AND the daemon still lists that pty id. Either miss → respawn.

    fn live_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Build a persisted relay row `(ordinal, sub_pane, tab, pty, session)`.
    fn row(ord: u32, sub_pane: u32, tab: u32, pty: &str, session: &str) -> RelayRow {
        (ord, sub_pane, tab, pty.to_string(), session.to_string())
    }

    #[test]
    fn attach_hints_keeps_live_survivor_with_matching_session() {
        let persisted = vec![row(0, 0, 0, "pty-A", "sess-1")];
        let hints = compute_attach_hints(&persisted, &live_set(&["pty-A"]), Some("sess-1"));
        assert_eq!(hints.get(&0).map(String::as_str), Some("pty-A"));
    }

    #[test]
    fn attach_hints_drops_dead_pty() {
        // Session matches, but the daemon no longer lists this pty → respawn.
        let persisted = vec![row(0, 0, 0, "pty-A", "sess-1")];
        let hints = compute_attach_hints(&persisted, &live_set(&[]), Some("sess-1"));
        assert!(
            hints.is_empty(),
            "a dead pty must not yield a reattach hint"
        );
    }

    #[test]
    fn attach_hints_drops_stale_session() {
        // The id is live but belongs to a DIFFERENT daemon session (the relay
        // restarted): the match is coincidental, so respawn rather than bind
        // to an unrelated process.
        let persisted = vec![row(0, 0, 0, "pty-A", "old-sess")];
        let hints = compute_attach_hints(&persisted, &live_set(&["pty-A"]), Some("new-sess"));
        assert!(hints.is_empty(), "a session mismatch must block reattach");
    }

    #[test]
    fn attach_hints_empty_when_no_current_session() {
        let persisted = vec![row(0, 0, 0, "pty-A", "sess-1")];
        let hints = compute_attach_hints(&persisted, &live_set(&["pty-A"]), None);
        assert!(hints.is_empty(), "no current session → nothing to reattach");
    }

    #[test]
    fn attach_hints_filters_mixed_set_to_live_survivors() {
        let persisted = vec![
            row(0, 0, 0, "pty-A", "sess-1"), // live + match → keep
            row(1, 0, 0, "pty-B", "sess-1"), // dead → drop
            row(2, 0, 0, "pty-C", "other"),  // wrong session → drop
        ];
        let hints = compute_attach_hints(&persisted, &live_set(&["pty-A", "pty-C"]), Some("sess-1"));
        assert_eq!(
            hints.len(),
            1,
            "only the live + session-matched leaf survives"
        );
        assert_eq!(hints.get(&0).map(String::as_str), Some("pty-A"));
        assert!(!hints.contains_key(&1), "dead pty-B dropped");
        assert!(!hints.contains_key(&2), "wrong-session pty-C dropped");
    }

    #[test]
    fn attach_hints_ignores_non_leaf_zero_rows() {
        // The single-pane fast path only wants leaf 0 / tab 0 rows. A split
        // leaf's row (sub_pane 1) must NOT leak into the ordinal-keyed map —
        // that tab restores via the tree path instead.
        let persisted = vec![
            row(0, 0, 0, "pty-main", "sess-1"),
            row(0, 1, 0, "pty-split", "sess-1"),
        ];
        let hints =
            compute_attach_hints(&persisted, &live_set(&["pty-main", "pty-split"]), Some("sess-1"));
        assert_eq!(hints.len(), 1, "only the (0,0,0) row maps to the ordinal");
        assert_eq!(hints.get(&0).map(String::as_str), Some("pty-main"));
    }

    // ── compute_leaf_attach_hints: per-leaf-tab reattach for the tree path ──

    #[test]
    fn leaf_hints_key_each_surviving_leaf_tab() {
        // A split tab (ordinal 0) with two leaves; leaf 1 has two per-pane
        // tabs. Each live + session-matched leaf-tab keys its own hint.
        let persisted = vec![
            row(0, 0, 0, "pty-l0", "sess-1"),
            row(0, 1, 0, "pty-l1t0", "sess-1"),
            row(0, 1, 1, "pty-l1t1", "sess-1"),
        ];
        let live = live_set(&["pty-l0", "pty-l1t0", "pty-l1t1"]);
        let hints = compute_leaf_attach_hints(&persisted, &live, Some("sess-1"));
        assert_eq!(hints.len(), 3);
        assert_eq!(hints.get(&(0, 0, 0)).map(String::as_str), Some("pty-l0"));
        assert_eq!(hints.get(&(0, 1, 0)).map(String::as_str), Some("pty-l1t0"));
        assert_eq!(hints.get(&(0, 1, 1)).map(String::as_str), Some("pty-l1t1"));
    }

    #[test]
    fn leaf_hints_drop_dead_and_stale_session_per_tab() {
        // Same session/live gate as the single-pane path, applied per leaf-tab:
        // a dead pty and a wrong-session pty both drop; the live one survives.
        let persisted = vec![
            row(0, 0, 0, "pty-live", "sess-1"), // keep
            row(0, 1, 0, "pty-dead", "sess-1"), // dead → drop
            row(0, 1, 1, "pty-old", "old-sess"), // wrong session → drop
        ];
        let hints = compute_leaf_attach_hints(&persisted, &live_set(&["pty-live"]), Some("sess-1"));
        assert_eq!(hints.len(), 1);
        assert_eq!(hints.get(&(0, 0, 0)).map(String::as_str), Some("pty-live"));
    }

    // ── load_persisted_tabs: corrupt-payload gate ───────────────────────────
    //
    // A damaged blob (truncated autosave, weights/children desync) must
    // never reach the live-tree build: the loader rejects it, preserves
    // the raw payload aside, and reports `Corrupt` so the caller falls
    // back to the default layout with a toast. The preserve dir is
    // redirected via OXIMUX_CORRUPT_LAYOUTS_DIR so test artifacts never
    // land in the real data dir.

    /// Serializes loader tests: they share the process-global preserve-dir
    /// env var, so parallel runs would cross-pollinate preserve dirs.
    static LOADER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn loader_env_guard() -> std::sync::MutexGuard<'static, ()> {
        LOADER_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn loader_fixture(raw: &str) -> (LoadedTabs, std::path::PathBuf) {
        let preserve_dir = std::env::temp_dir().join(format!(
            "oximux-loader-test-{}-{}",
            std::process::id(),
            raw.len()
        ));
        let _ = std::fs::remove_dir_all(&preserve_dir);
        // SAFETY: test-only; LOADER_ENV_LOCK (held by every caller for
        // the full test body) serializes all access to this variable.
        unsafe { std::env::set_var("OXIMUX_CORRUPT_LAYOUTS_DIR", &preserve_dir) };
        let db = oximux_storage::open_memory().expect("memory db");
        let repo = SettingsRepo::new(db);
        repo.set(&settings_key("proj", "main"), raw).expect("seed");
        let out = load_persisted_tabs(&repo, "proj", "main");
        // Clear immediately — the preserve happened (or didn't) inside the
        // load above, and a stale var must not leak into any later test
        // that doesn't hold the lock.
        unsafe { std::env::remove_var("OXIMUX_CORRUPT_LAYOUTS_DIR") };
        (out, preserve_dir)
    }

    #[test]
    fn loader_rejects_truncated_json_and_preserves_payload() {
        let _env = loader_env_guard();
        let truncated = r#"{"tabs":[{"label":"Terminal 1","tree":"Leaf"#;
        let (out, dir) = loader_fixture(truncated);
        assert!(matches!(out, LoadedTabs::Corrupt));
        let preserved: Vec<_> = std::fs::read_dir(&dir)
            .expect("preserve dir created")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".corrupt.json"))
            .collect();
        assert_eq!(preserved.len(), 1, "raw payload preserved exactly once");
        let body = std::fs::read_to_string(preserved[0].path()).unwrap();
        assert_eq!(body, truncated, "payload preserved byte-identical");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loader_rejects_shape_violation_and_preserves_payload() {
        let _env = loader_env_guard();
        // Parses fine, but the split's weights/children desync would
        // panic later in live-tree mutation — must be caught at load.
        let bad_shape = r#"{
            "tabs":[{"label":"T1","tree":{"Split":{"axis":"Horizontal",
                "children":["Leaf","Leaf"],"weights":[1.0]}}}],
            "active":0,"next_label_n":2}"#;
        let (out, dir) = loader_fixture(bad_shape);
        assert!(matches!(out, LoadedTabs::Corrupt));
        assert!(
            std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) >= 1,
            "shape-rejected payload preserved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loader_passes_healthy_snapshot() {
        let _env = loader_env_guard();
        let healthy = serde_json::to_string(&PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Terminal 1".into(),
                ..PersistedTab::default()
            }],
            active: 0,
            next_label_n: 2,
            ..PersistedTabs::default()
        })
        .unwrap();
        let (out, dir) = loader_fixture(&healthy);
        match out {
            LoadedTabs::Snapshot(snap) => assert_eq!(snap.tabs.len(), 1),
            _ => panic!("healthy snapshot must load"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loader_reports_absent_when_nothing_persisted() {
        let db = oximux_storage::open_memory().expect("memory db");
        let repo = SettingsRepo::new(db);
        assert!(matches!(
            load_persisted_tabs(&repo, "proj", "main"),
            LoadedTabs::Absent
        ));
    }
}
