//! Search palette data model — a plain-data snapshot of everything the
//! palette can find (open tabs, worktrees, projects, settings panes, actions).
//!
//! Built once per palette open by [`super::sources::snapshot`] and never
//! mutated while the palette is up, so the indices an [`ItemRef`] carries stay
//! valid for the whole session and live status ticks never reshuffle rows.
//! Holds no GPUI entity handles: activation goes back through
//! [`Target`], which `WorkspaceRoot` resolves against live state.

use gpui::Action;
use oximux_core::AgentStatus;

use crate::shell::settings_modal::SettingsPane;

/// One open tab in a mounted project.
#[derive(Clone, Debug, Default)]
pub struct TabItem {
    pub project_id: String,
    /// [`crate::shell::pane_group::PaneGroupTab::uid`] — survives reorder and
    /// moves between groups, so activation searches by it rather than index.
    pub uid: u64,
    /// Chip text: custom title, else the generated label.
    pub title: String,
    /// Leading glyph asset path (terminal / adapter brand / file …).
    pub icon: &'static str,
    /// Extra words the tab answers to ("terminal", "agent", the adapter slug).
    pub aliases: Vec<String>,
    /// Backing file for editor / diff tabs.
    pub path: Option<String>,
    pub project_name: String,
    /// Display name of the worktree the tab belongs to (empty if unresolved).
    pub worktree_name: String,
    pub branch: String,
    /// Owning worktree row's tint (0xRRGGBB) for the location chip.
    pub tint: Option<u32>,
    pub is_current: bool,
    pub in_current_worktree: bool,
    /// The tab's agent is blocked on the user (input or approval).
    pub needs_attention: bool,
    /// Recency key in unix ms: focus stamp, else session-history seed; 0 unknown.
    pub last_ms: i64,
}

/// One worktree row (including a project's synthesized primary).
#[derive(Clone, Debug, Default)]
pub struct WorktreeItem {
    /// `workspaces.id`, or `primary:<project_id>` for the repo root.
    pub workspace_id: String,
    pub project_id: String,
    pub worktree_path: String,
    pub name: String,
    pub branch: String,
    pub is_primary: bool,
    pub project_name: String,
    pub tint: Option<u32>,
    /// Latest agent-session status (rail cache).
    pub status: Option<AgentStatus>,
    /// Has an open PTY / agent tab.
    pub is_live: bool,
    pub is_current: bool,
    /// Unix ms of the last activation this run, if any.
    pub last_visited_ms: Option<i64>,
    /// Newest agent-session time, else row creation; 0 unknown.
    pub last_activity_ms: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectItem {
    pub project_id: String,
    pub name: String,
    pub root_path: String,
    pub is_current: bool,
}

#[derive(Clone, Debug)]
pub struct SettingItem {
    pub pane: SettingsPane,
    pub label: &'static str,
    pub icon: &'static str,
    /// Space-separated synonyms the scorer also matches.
    pub keywords: &'static str,
}

/// Factory for a fresh action instance (fn pointer so tables can be `const`).
pub type MakeAction = fn() -> Box<dyn Action>;

#[derive(Clone)]
pub struct ActionItem {
    pub label: String,
    /// Label plus synonyms — what the scorer matches.
    pub search_text: String,
    /// Live display chord ("⌘⇧T"), when bound.
    pub chord: Option<String>,
    pub make: MakeAction,
}

/// Everything searchable at the moment the palette opened.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub tabs: Vec<TabItem>,
    pub worktrees: Vec<WorktreeItem>,
    pub projects: Vec<ProjectItem>,
    pub settings: Vec<SettingItem>,
    pub actions: Vec<ActionItem>,
    /// Capture time (unix ms) — the reference point for row ages.
    pub now_ms: i64,
}

/// Index of one item inside a [`Snapshot`] list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemRef {
    Tab(usize),
    Worktree(usize),
    Project(usize),
    Setting(usize),
    Action(usize),
}

/// What activating a row does, resolved by `WorkspaceRoot` against live state
/// (the snapshot may be stale by then — a missing target is a toast, never a
/// panic).
#[derive(Clone)]
pub enum Target {
    Tab { project_id: String, uid: u64 },
    Worktree { workspace_id: String, project_id: String, worktree_path: String },
    Project { project_id: String },
    Setting(SettingsPane),
    Action(MakeAction),
    CreateWorktree(String),
}

impl Snapshot {
    /// Stable identity for a row — element ids and selection tracking key on
    /// it, never on a list index.
    pub fn item_id(&self, item: ItemRef) -> String {
        match item {
            ItemRef::Tab(i) => format!("tab:{}", self.tabs[i].uid),
            ItemRef::Worktree(i) => format!("wt:{}", self.worktrees[i].workspace_id),
            ItemRef::Project(i) => format!("proj:{}", self.projects[i].project_id),
            ItemRef::Setting(i) => format!("set:{}", self.settings[i].label),
            ItemRef::Action(i) => format!("act:{}", self.actions[i].label),
        }
    }

    pub fn target(&self, item: ItemRef) -> Target {
        match item {
            ItemRef::Tab(i) => {
                let t = &self.tabs[i];
                Target::Tab { project_id: t.project_id.clone(), uid: t.uid }
            }
            ItemRef::Worktree(i) => {
                let w = &self.worktrees[i];
                Target::Worktree {
                    workspace_id: w.workspace_id.clone(),
                    project_id: w.project_id.clone(),
                    worktree_path: w.worktree_path.clone(),
                }
            }
            ItemRef::Project(i) => Target::Project { project_id: self.projects[i].project_id.clone() },
            ItemRef::Setting(i) => Target::Setting(self.settings[i].pane),
            ItemRef::Action(i) => Target::Action(self.actions[i].make),
        }
    }

    /// Number of distinct projects the snapshot spans (gates the Filter button).
    pub fn project_count(&self) -> usize {
        self.projects.len()
    }
}
