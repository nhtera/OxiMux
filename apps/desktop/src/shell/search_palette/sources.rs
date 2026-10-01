//! Build the palette [`Snapshot`] from live `WorkspaceRoot` state.
//!
//! In-memory reads only — no SQLite, no disk. Worktree rows, statuses and
//! session times come from the rail's background-gathered caches
//! (`rail_workspaces_by_project`, `rail_latest_status`, `rail_last_active`);
//! tabs come from each mounted project's pane groups. Runs from a
//! `WorkspaceRoot` action handler, never from inside a pane group's own
//! `update` (reading a view mid-update aborts).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use gpui::App;
use oximux_core::{Project, Workspace};

use crate::shell::pane_group::render::tab_kind_icon;
use crate::shell::pane_group::{PaneGroupTab, PaneGroupTabKind, TabColor};
use crate::shell::search_palette::model::{ProjectItem, Snapshot, TabItem, WorktreeItem};
use crate::shell::search_palette::recency::{fallback_ms, now_ms};
use crate::shell::search_palette::settings_items::{action_items, settings_items};
use crate::workspace_root::WorkspaceRoot;

pub fn snapshot(root: &WorkspaceRoot, cx: &App) -> Snapshot {
    let current_project = root.active_project.as_ref().map(|p| p.id.as_str());
    let current_worktree = root.active_worktree.as_ref().map(|p| p.to_string_lossy().to_string());
    let live: HashSet<String> = root
        .project_panes_by_project
        .values()
        .flat_map(|panes| panes.read(cx).live_worktree_paths(cx))
        .collect();

    let projects = &root.app_state.recent_projects;
    let mut worktrees = Vec::new();
    for project in projects {
        for w in root.rail_workspaces_by_project.get(&project.id).into_iter().flatten() {
            if w.archived_at.is_some() {
                continue;
            }
            worktrees.push(WorktreeItem {
                workspace_id: w.id.clone(),
                project_id: project.id.clone(),
                worktree_path: w.worktree_path.clone(),
                name: worktree_display_name(w, project),
                branch: w.branch.clone(),
                is_primary: w.worktree_path == project.root_path,
                project_name: project.name.clone(),
                tint: w.tint.as_deref().and_then(TabColor::from_slug).map(TabColor::rgb),
                status: root.rail_latest_status.get(&w.id).cloned().flatten(),
                is_live: live.contains(&w.worktree_path),
                is_current: current_worktree.as_deref() == Some(w.worktree_path.as_str()),
                last_visited_ms: root.recency.worktree_ms(&w.id),
                last_activity_ms: fallback_ms(
                    root.rail_last_active.get(&w.id).map(String::as_str),
                    &w.created_at,
                ),
            });
        }
    }

    let tabs = collect_tabs(root, &worktrees, current_project, cx);

    let project_items = projects
        .iter()
        .map(|p| ProjectItem {
            project_id: p.id.clone(),
            name: p.name.clone(),
            root_path: tilde_path(&p.root_path),
            is_current: current_project == Some(p.id.as_str()),
        })
        .collect();

    Snapshot {
        tabs,
        worktrees,
        projects: project_items,
        settings: settings_items(),
        actions: action_items(),
        now_ms: now_ms(),
    }
}

/// Every tab of every mounted project, in visual order per group.
fn collect_tabs(
    root: &WorkspaceRoot,
    worktrees: &[WorktreeItem],
    current_project: Option<&str>,
    cx: &App,
) -> Vec<TabItem> {
    let by_path: HashMap<(&str, &str), &WorktreeItem> = worktrees
        .iter()
        .map(|w| ((w.project_id.as_str(), w.worktree_path.as_str()), w))
        .collect();
    let mut tabs = Vec::new();
    for project in &root.app_state.recent_projects {
        let Some(panes) = root.project_panes_by_project.get(&project.id) else {
            continue;
        };
        let panes = panes.read(cx);
        let active_group = panes.active_group().map(|g| g.entity_id());
        for group in panes.group_entities() {
            let is_active_group =
                current_project == Some(project.id.as_str()) && active_group == Some(group.entity_id());
            let g = group.read(cx);
            for (idx, tab) in g.visible_tabs() {
                let worktree_path = match &tab.kind {
                    PaneGroupTabKind::Agent { worktree_path, .. } => worktree_path.clone(),
                    _ => g.cwd().clone(),
                };
                let path_key = worktree_path.to_string_lossy();
                let wt = by_path.get(&(project.id.as_str(), path_key.as_ref())).copied();
                tabs.push(tab_item(tab, project, wt, &worktree_path, is_active_group && idx == g.active()));
            }
        }
    }
    tabs
}

fn tab_item(
    tab: &PaneGroupTab,
    project: &Project,
    wt: Option<&WorktreeItem>,
    worktree_path: &Path,
    is_current: bool,
) -> TabItem {
    let (aliases, path, needs_attention) = describe_kind(&tab.kind);
    let seed = wt.map(|w| w.last_activity_ms).unwrap_or(0);
    TabItem {
        project_id: project.id.clone(),
        uid: tab.uid,
        title: tab.custom_title.clone().unwrap_or_else(|| tab.label.clone()).to_string(),
        icon: tab_kind_icon(&tab.kind),
        aliases,
        path,
        project_name: project.name.clone(),
        worktree_name: wt.map(|w| w.name.clone()).unwrap_or_else(|| {
            worktree_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        }),
        branch: wt.map(|w| w.branch.clone()).unwrap_or_default(),
        tint: wt.and_then(|w| w.tint),
        is_current,
        in_current_worktree: wt.is_some_and(|w| w.is_current),
        needs_attention,
        last_ms: if tab.last_focused_ms > 0 { tab.last_focused_ms } else { seed },
    }
}

/// `(aliases, backing path, needs attention)` for a tab kind.
fn describe_kind(kind: &PaneGroupTabKind) -> (Vec<String>, Option<String>, bool) {
    let words = |ws: &[&str]| ws.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let file = |p: &Path| Some(p.to_string_lossy().to_string());
    match kind {
        PaneGroupTabKind::Terminal => (words(&["terminal", "shell"]), None, false),
        PaneGroupTabKind::Agent { adapter_id, status_rx, .. } => {
            let mut aliases = words(&["agent", adapter_id]);
            // "claude-code" also answers to "claude".
            if let Some((head, _)) = adapter_id.split_once('-') {
                aliases.push(head.to_string());
            }
            (aliases, None, status_rx.borrow().status.is_blocking())
        }
        PaneGroupTabKind::AgentChat { .. } => (words(&["chat", "agent"]), None, false),
        PaneGroupTabKind::Editor { path } => (words(&["file", "editor"]), file(path), false),
        PaneGroupTabKind::Diff { path, .. }
        | PaneGroupTabKind::BranchFile { path }
        | PaneGroupTabKind::StashFile { path, .. } => (words(&["diff"]), file(path), false),
        PaneGroupTabKind::Commit { .. }
        | PaneGroupTabKind::StashAll { .. }
        | PaneGroupTabKind::CombinedDiff { .. } => (words(&["diff"]), None, false),
        PaneGroupTabKind::Browser { url } => (words(&["browser", "web"]), Some(url.clone()), false),
        PaneGroupTabKind::Tasks => (words(&["tasks", "issues"]), None, false),
        PaneGroupTabKind::Automations => (words(&["automations", "schedules"]), None, false),
    }
}

/// The rail's row name: a worktree's own name, the default branch for a
/// synthesized primary (its `name`), else the project name.
fn worktree_display_name(w: &Workspace, project: &Project) -> String {
    if !w.name.is_empty() {
        w.name.clone()
    } else if !w.branch.is_empty() {
        w.branch.clone()
    } else {
        project.name.clone()
    }
}

/// `$HOME/x` → `~/x`, for display only.
fn tilde_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}
