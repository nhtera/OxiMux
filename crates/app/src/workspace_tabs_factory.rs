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

use gpui::{AppContext, Context, Entity, Window};
use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::notifier::Notifier;
use crate::persisted_terminals::{
    PersistedTabs, PersistedTree, count_leaves, restore_tree, settings_key,
};
use crate::shell::main_pane::MainPane;
use crate::shell::pane_tree::PaneId;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};
use crate::shell::workspace_tabs::WorkspaceTabs;
use crate::workspace_root::WorkspaceRoot;

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_workspace_tabs(
    cwd: PathBuf,
    snapshot: Option<PersistedTabs>,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<WorkspaceTabs>> {
    // Empty-tabs snapshot is treated as no-snapshot so the user never lands
    // on a blank welcome.
    let restore: Option<PersistedTabs> = snapshot.filter(|s| !s.tabs.is_empty());

    let (first_pane, rest) = match restore.as_ref() {
        Some(s) => {
            let initial = build_pane_for_tree(
                &s.tabs[0].tree,
                cwd.clone(),
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )?;
            (initial, &s.tabs[1..])
        }
        None => (
            build_default_pane(cwd.clone(), theme, density, typography.clone(), window, cx)?,
            &[][..],
        ),
    };

    let tabs_entity = cx.new(|cx| {
        WorkspaceTabs::new(
            first_pane,
            cwd.clone(),
            theme,
            density,
            typography.clone(),
            cli_runtime,
            notifier,
            window,
            cx,
        )
    });

    if !rest.is_empty() {
        let extra_tabs: Vec<_> = rest
            .iter()
            .filter_map(|t| {
                build_pane_for_tree(
                    &t.tree,
                    cwd.clone(),
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
                .map(|pane| (t.label.clone(), pane))
            })
            .collect();
        tabs_entity.update(cx, |tabs, cx| {
            for (label, pane) in extra_tabs {
                tabs.push_restored_terminal_tab(label, pane, cx);
            }
        });
    }
    if let Some(snap) = restore.as_ref() {
        tabs_entity.update(cx, |tabs, cx| {
            tabs.apply_restored_state(snap.active, snap.next_label_n, window, cx);
        });
    }
    Some(tabs_entity)
}

fn build_default_pane(
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<MainPane>> {
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
    Some(cx.new(|cx| MainPane::new(view, cwd, theme, density, typography, cx)))
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
