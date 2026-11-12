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
use crate::shell::pane_content::PaneContent;
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
    // One host read for the whole snapshot rather than one per tab.
    let host = cx
        .try_global::<crate::remote_control::RemoteControl>()
        .and_then(|rc| rc.host_core());
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
                tabs.push(tab_item(
                    tab,
                    project,
                    wt,
                    &worktree_path,
                    is_active_group && idx == g.active(),
                    host.as_deref(),
                    cx,
                ));
            }
        }
    }
    tabs
}

#[allow(clippy::too_many_arguments)]
fn tab_item(
    tab: &PaneGroupTab,
    project: &Project,
    wt: Option<&WorktreeItem>,
    worktree_path: &Path,
    is_current: bool,
    host: Option<&oximux_remote_host::HostCore>,
    cx: &App,
) -> TabItem {
    let (aliases, path) = describe_kind(&tab.kind);
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
        needs_attention: tab_needs_you(tab, host, cx),
        last_ms: if tab.last_focused_ms > 0 { tab.last_focused_ms } else { seed },
    }
}

/// Whether this tab is one somebody has to come back to, by the one
/// definition in `oximux_agents::attention`.
///
/// This used to be `status.is_blocking()`, read off the tab kind — a third
/// answer to the question, narrower than either of the other two: it saw a
/// permission or a prompt and missed a failure entirely. And a chat tab was
/// hard-coded `false`, so a chat waiting on an approval never surfaced here at
/// all. `is_blocking` stays where it belongs, on the status machine.
fn tab_needs_you(
    tab: &PaneGroupTab,
    host: Option<&oximux_remote_host::HostCore>,
    cx: &App,
) -> bool {
    match (&tab.kind, &tab.content) {
        // A cockpit agent: its parsed status, through the one mapping.
        (PaneGroupTabKind::Agent { status_rx, .. }, _) => {
            crate::shell::agents_dashboard::model::status_needs_you(&status_rx.borrow().status)
        }
        // A chat session: the host holds its health, so it is the authority.
        // The session id is on the view, not on the tab kind.
        (PaneGroupTabKind::AgentChat { .. }, PaneContent::AgentChat(view)) => view
            .read(cx)
            .session_id()
            .zip(host)
            .and_then(|(id, host)| {
                host.attention_of(&oximux_agents::session_prefs::chat_key(id))
            })
            .is_some_and(|attention| attention.kind.is_needs_you()),
        _ => false,
    }
}

/// `(aliases, backing path)` for a tab kind.
fn describe_kind(kind: &PaneGroupTabKind) -> (Vec<String>, Option<String>) {
    let words = |ws: &[&str]| ws.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let file = |p: &Path| Some(p.to_string_lossy().to_string());
    match kind {
        PaneGroupTabKind::Terminal => (words(&["terminal", "shell"]), None),
        PaneGroupTabKind::Agent { adapter_id, .. } => {
            let mut aliases = words(&["agent", adapter_id]);
            // "claude-code" also answers to "claude".
            if let Some((head, _)) = adapter_id.split_once('-') {
                aliases.push(head.to_string());
            }
            (aliases, None)
        }
        PaneGroupTabKind::AgentChat { .. } => (words(&["chat", "agent"]), None),
        PaneGroupTabKind::Editor { path } => (words(&["file", "editor"]), file(path)),
        PaneGroupTabKind::Diff { path, .. }
        | PaneGroupTabKind::BranchFile { path }
        | PaneGroupTabKind::StashFile { path, .. } => (words(&["diff"]), file(path)),
        PaneGroupTabKind::Commit { .. }
        | PaneGroupTabKind::StashAll { .. }
        | PaneGroupTabKind::CombinedDiff { .. } => (words(&["diff"]), None),
        PaneGroupTabKind::Browser { url } => (words(&["browser", "web"]), Some(url.clone())),
        PaneGroupTabKind::Tasks => (words(&["tasks", "issues"]), None),
        PaneGroupTabKind::Automations => (words(&["automations", "schedules"]), None),
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
