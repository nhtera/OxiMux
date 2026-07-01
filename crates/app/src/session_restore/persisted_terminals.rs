//! Per-project terminal-tab persistence.
//!
//! Stored as a single JSON blob in the `settings` table under the key
//! `terminal_tabs:<project_id>` (size cap 64 KiB — generous for dozens of
//! tabs with deep split trees). The blob captures tab labels, active
//! index, the next monotonic label counter, and each tab's pane tree
//! (axes + weights + leaf count). Per-pane cwd is **not** persisted — all
//! panes restore at the owning project's `root_path`.
//!
//! What is **not** persisted:
//! - PTY scrollback / running command output (the shell process is dead).
//!   Restored shells start with a fresh prompt.
//! - Per-pane focus inside a tab (active leaf restores to the first leaf).
//! - Live agent process — the `CliRuntime` session dies with the app. On
//!   restore the adapter respawns with the same `{adapter, worktree, model,
//!   effort}` it had before and reloads its own conversation from disk.

use serde::{Deserialize, Serialize};

use oximux_core::AgentAdapter;

#[cfg(test)]
use crate::shell::pane_tree::PaneId;
use crate::shell::pane_tree::{Axis, PaneTree};

const KEY_PREFIX: &str = "terminal_tabs:";

/// Build the settings key for per-window tab persistence. Format:
/// `terminal_tabs:<project_id>:<window_id>`. The window_id scopes saves
/// so two app windows on the same project don't clobber each other.
pub fn settings_key(project_id: &str, window_id: &str) -> String {
    format!("{KEY_PREFIX}{project_id}:{window_id}")
}

/// Legacy (pre-V005) settings key. Format: `terminal_tabs:<project_id>`.
/// Used as a fallback read when `window_id == "main"` and the new-format
/// key returns `None` — preserves single-window users' tab layout across
/// the upgrade without data loss.
pub fn legacy_settings_key(project_id: &str) -> String {
    format!("{KEY_PREFIX}{project_id}")
}

/// Settings key for the open-windows manifest (the set of windows that were
/// open at the last quit, so boot can reopen them all).
pub const WINDOWS_MANIFEST_KEY: &str = "open_windows";

/// The set of windows open at the last quit. Persisted under
/// [`WINDOWS_MANIFEST_KEY`] so the next launch reopens each window under its
/// stable `window_id`, restoring that window's project + per-window layout.
/// An absent / empty manifest means "single-window legacy boot" — open one
/// window keyed `"main"` and bootstrap the most-recent project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowsManifest {
    pub windows: Vec<PersistedWindow>,
}

/// One window's restore record: its stable persist id + the project that was
/// active in it at quit (`None` = no project, e.g. the welcome screen).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWindow {
    pub window_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedTabs {
    pub tabs: Vec<PersistedTab>,
    pub active: usize,
    pub next_label_n: u64,
    /// Visual order of tabs as a list of flat indices into `tabs`. Empty
    /// = "use insertion order" (legacy snapshots). New snapshots always
    /// emit a populated vector matching `tabs.len()` so drag-reordered
    /// strips survive restart.
    ///
    /// `#[serde(default)]` keeps pre-v2 blobs readable — they parse with
    /// `tab_order: Vec::new()` and the restorer falls back to insertion
    /// order via the empty-vec check.
    #[serde(default)]
    pub tab_order: Vec<usize>,
    /// Workspace-level pane-group split tree. `None` = legacy single-group
    /// snapshot (every tab lands in one group on restore). `Some(...)` =
    /// multi-group layout; the tree's DFS leaf order pairs 1:1 with
    /// `groups` below.
    ///
    /// `#[serde(default)]` resolves to `None` for pre-v3 blobs so single-
    /// group restores keep working untouched.
    #[serde(default)]
    pub group_tree: Option<PersistedTree>,
    /// Per-group state, one entry per leaf in `group_tree` in DFS order.
    /// `tabs` (top-level) holds the FLAT concatenation of every group's
    /// tabs; each `PersistedGroup.tab_count` tells the restorer how many
    /// to consume off the front for that group. Empty when `group_tree`
    /// is `None`.
    #[serde(default)]
    pub groups: Vec<PersistedGroup>,
    /// DFS index of the active group in `groups`. `0` is the default for
    /// pre-v3 blobs (which only ever had one group anyway). Saturating-
    /// clamped to `groups.len() - 1` on restore.
    #[serde(default)]
    pub active_group: usize,
    /// Agent-chat transcripts for the `AgentChat` tabs in this snapshot, one
    /// per chat session that had a completed turn. **Not** part of the layout
    /// blob JSON (`#[serde(skip)]`): each transcript is stored under its own
    /// `agent_chat:<session_id>` settings key to avoid the layout blob's size
    /// cap. `snapshot()` fills this from the live views; `save_persisted_tabs`
    /// drains it to the per-session keys, and `load_persisted_tabs` re-populates
    /// it from disk so the factory can rehydrate without the `SettingsRepo`.
    #[serde(skip)]
    pub chat_transcripts: Vec<crate::persisted_chat::PersistedChatTranscript>,
}

/// Per-group restore state emitted alongside the workspace-level
/// `group_tree`. Tabs themselves live in the flat `PersistedTabs.tabs`
/// Vec; `tab_count` says how many consecutive entries belong to THIS
/// group (visited in DFS leaf order). Per-group `active` + `tab_order`
/// are LOCAL indices into the group's own slice of that flat vec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PersistedGroup {
    /// Number of tabs in this group. Restorer pulls `tab_count` entries
    /// off the flat `PersistedTabs.tabs` iterator in order.
    pub tab_count: usize,
    /// Local active index into the group's tab slice. Clamped to
    /// `[0, tab_count)` on restore.
    pub active: usize,
    /// Visual order — local indices in `[0, tab_count)` matching the
    /// per-group `tab_order_iter()`. Empty = "insertion order" fallback.
    #[serde(default)]
    pub tab_order: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedTab {
    pub label: String,
    /// Per-tab sub-pane layout tree (one terminal tab → multiple PTY
    /// splits via `Cmd+D`). Pre-v4 snapshots always emitted
    /// `PersistedTree::Leaf` here regardless of the live split state, so
    /// restored multi-sub-pane tabs collapsed back to a single pane.
    /// v4 reflects the actual `TerminalSplitTree::tree()` topology so
    /// Cmd+D layouts survive restart.
    pub tree: PersistedTree,
    /// Present iff the tab was an agent tab. `None` keeps the existing
    /// plain-terminal restore path. `#[serde(default)]` lets older snapshots
    /// (no `agent` field) parse cleanly.
    #[serde(default)]
    pub agent: Option<PersistedAgentTab>,
    /// Explicit kind tag. `#[serde(default)]` resolves to
    /// `PersistedTabKind::Terminal` for pre-v2 blobs — combined with the
    /// existing `agent` field (`Some` → Agent restoration path), this
    /// keeps every legacy snapshot readable. New snapshots set this for
    /// editor tabs (which the legacy schema dropped entirely).
    #[serde(default)]
    pub kind: PersistedTabKind,
    /// Per-leaf sub-pane state in DFS leaf-walk order of `tree`. Length
    /// must equal `tree`'s leaf count (>= 1). Empty for legacy
    /// (pre-v4) blobs — restorer falls back to a single sub-pane at the
    /// project cwd. Active leaf is `active_sub_pane` below (also a DFS
    /// position).
    #[serde(default)]
    pub sub_panes: Vec<PersistedSubPane>,
    /// Active sub-pane's position in DFS leaf order of `tree`. `0` for
    /// single-leaf trees (the default for pre-v4 blobs).
    #[serde(default)]
    pub active_sub_pane: usize,
    /// `true` when this was a reusable single-click "preview" tab (italic
    /// label). Restored as a preview so a browse tab stays a browse tab
    /// across a relaunch instead of silently becoming permanent.
    /// `#[serde(default)]` → `false` for blobs written before preview
    /// state was persisted (their tabs restore permanent, as they did).
    #[serde(default)]
    pub is_preview: bool,
    /// `true` when the user pinned this tab. Pinned tabs cluster at the
    /// front of the strip, so persisting this keeps both the pin and the
    /// saved order intact. `#[serde(default)]` → `false` for older blobs.
    #[serde(default)]
    pub pinned: bool,
    /// User-assigned color-tag slug (`TabColor::slug()`), or `None` for no
    /// tint. Round-trips via `TabColor::from_slug` on restore; an unknown
    /// slug degrades to no tint. `#[serde(default)]` → `None` for older blobs.
    #[serde(default)]
    pub color: Option<String>,
    /// User-assigned custom title from "Change Title". `None` → fall back
    /// to the default label. `#[serde(default)]` → `None` for older blobs.
    #[serde(default)]
    pub custom_title: Option<String>,
}

/// Per-leaf state captured by `PersistedTab.sub_panes`. Carries enough
/// metadata to respawn a PTY in the same shell location on restore.
/// Scrollback is NOT stored here — it stays in the existing
/// `pane_buffers` table (one row per tab; only the active sub-pane's
/// buffer is captured today, matching pre-v4 behavior).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PersistedSubPane {
    /// Working directory at capture time. `None` → restore falls back
    /// to the project cwd. Resolved at snapshot via `cwd_of_pid` on the
    /// PTY's local process id. Mirrors the leaf's ACTIVE tab so a legacy
    /// (pre-per-pane-tabs) reader still restores the visible terminal.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Stable `OXIMUX_SURFACE_ID` of the leaf's active tab. Empty for
    /// pre-existing blobs → restore mints a fresh one. Round-tripping it
    /// keeps the surface id identical across an app quit -> reattach.
    #[serde(default)]
    pub surface_id: String,
    /// Stable `OXIMUX_TAB_ID` of the leaf's active tab. Empty for older
    /// blobs → restore mints a fresh one.
    #[serde(default)]
    pub tab_id: String,
    /// Per-pane (per-leaf) tab strip: one entry per terminal in this
    /// leaf. Empty for legacy / single-tab leaves — restore then falls
    /// back to a one-tab leaf built from the `cwd`/`surface_id`/`tab_id`
    /// fields above. When non-empty it is the source of truth.
    #[serde(default)]
    pub tabs: Vec<PersistedLeafTab>,
    /// Active tab index within `tabs`. `0` for single-tab leaves.
    #[serde(default)]
    pub active_tab: usize,
}

/// One terminal tab inside a persisted leaf. Carries the per-tab cwd +
/// stable surface/tab ids so each terminal in a split pane's strip is
/// restored with its own identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PersistedLeafTab {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub surface_id: String,
    #[serde(default)]
    pub tab_id: String,
}

/// Tab kind tag persisted alongside `PersistedTab`. The agent variant
/// is unused (Agent metadata lives in the legacy `agent: Option<...>`
/// field to keep diffs small + back-compat trivial). Terminal is the
/// default — see `PersistedTab::kind` doc.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum PersistedTabKind {
    #[default]
    Terminal,
    /// Editor tab — `path` is the absolute filesystem path. Restore
    /// reopens the file (or surfaces a missing-file warning if gone).
    Editor { path: String },
    /// Embedded browser tab — `url` is the last-known address. Restore
    /// reopens a browser tab navigated to it. `profile_id` selects the
    /// isolated cookie/cache store (a browser profile); absent / `None`
    /// restores into the shared default store (back-compat for tabs saved
    /// before profiles existed).
    Browser {
        url: String,
        #[serde(default)]
        profile_id: Option<uuid::Uuid>,
    },
    /// Agent Chat tab — a structured Claude chat surface (not a terminal).
    /// `cwd` is the working directory to respawn in; `model` the launch model;
    /// `session_id` (once Claude minted one) both keys the transcript blob and
    /// drives `--resume`. A `None` session_id restores a fresh chat (the tab
    /// never completed a turn, so there's nothing to resume).
    AgentChat {
        cwd: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
}

/// Per-tab agent metadata sufficient to respawn the same CLI session on
/// restore. The agent CLI itself reloads its conversation from disk; this
/// blob only carries what `start_session` needs to reach the same shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAgentTab {
    pub adapter: AgentAdapter,
    pub adapter_id: String,
    pub worktree_path: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Daemon-side PTY id of the live agent session at snapshot time.
    /// Present only when the agent ran through the relay. On restore, if
    /// the relay is still on the same session and this id is still alive,
    /// the tab re-attaches to the running CLI instead of respawning it.
    /// `#[serde(default)]` keeps pre-existing blobs (no field) readable.
    #[serde(default)]
    pub relay_external_id: Option<String>,
    /// Relay daemon session id at snapshot time. Compared against the
    /// current session on restore — a mismatch (daemon restarted) means
    /// the PTY is gone, so the tab respawns fresh instead of re-attaching.
    #[serde(default)]
    pub relay_session: Option<String>,
}

/// Mirrors `PaneTree` but without GPUI-side `PaneId`s (regenerated on
/// restore). `Split` keeps weights so drag-resize is preserved.
/// `Default` resolves to `Leaf` so `PersistedTab::default()` produces a
/// well-formed single-pane tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum PersistedTree {
    #[default]
    Leaf,
    Split {
        axis: PersistedAxis,
        children: Vec<PersistedTree>,
        weights: Vec<f32>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PersistedAxis {
    Horizontal,
    Vertical,
}

impl From<Axis> for PersistedAxis {
    fn from(a: Axis) -> Self {
        match a {
            Axis::Horizontal => Self::Horizontal,
            Axis::Vertical => Self::Vertical,
        }
    }
}

impl From<PersistedAxis> for Axis {
    fn from(a: PersistedAxis) -> Self {
        match a {
            PersistedAxis::Horizontal => Self::Horizontal,
            PersistedAxis::Vertical => Self::Vertical,
        }
    }
}

/// Convert any live `PaneTree<L>` into a persistable shape. Leaf ids are
/// discarded — `PersistedTree::Leaf` carries no payload; the restorer
/// re-allocates ids by walking the persisted tree in DFS order. Generic
/// over leaf type so the same helper covers both per-tab content trees
/// (`PaneTree<PaneId>`) and workspace group layout (`PaneTree<PaneGroupId>`).
pub fn snapshot_tree<L: Copy + PartialEq>(t: &PaneTree<L>) -> PersistedTree {
    match t {
        PaneTree::Leaf(_) => PersistedTree::Leaf,
        PaneTree::Split {
            axis,
            children,
            weights,
        } => PersistedTree::Split {
            axis: (*axis).into(),
            children: children.iter().map(snapshot_tree).collect(),
            weights: weights.clone(),
        },
    }
}

/// Walk a `PersistedTree` and produce a parallel `PaneTree<L>` whose
/// leaves are assigned fresh ids via `alloc`. Generic over leaf type
/// so the same helper rebuilds per-tab pane trees AND the workspace
/// group tree (`PaneGroupId`). Caller supplies the allocator so id
/// space stays monotonic across the rebuild.
pub fn restore_tree<L, F>(p: &PersistedTree, alloc: &mut F) -> PaneTree<L>
where
    L: Copy + PartialEq,
    F: FnMut() -> L,
{
    match p {
        PersistedTree::Leaf => PaneTree::Leaf(alloc()),
        PersistedTree::Split {
            axis,
            children,
            weights,
        } => PaneTree::Split {
            axis: (*axis).into(),
            children: children.iter().map(|c| restore_tree(c, alloc)).collect(),
            weights: weights.clone(),
        },
    }
}

/// Collect leaves in-order so the renderer + the per-leaf TerminalView
/// entities can be created in the same order they appear in the tree.
pub fn count_leaves(t: &PersistedTree) -> usize {
    match t {
        PersistedTree::Leaf => 1,
        PersistedTree::Split { children, .. } => children.iter().map(count_leaves).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree() -> PaneTree {
        PaneTree::Split {
            axis: Axis::Horizontal,
            children: vec![
                PaneTree::Leaf(PaneId(1)),
                PaneTree::Split {
                    axis: Axis::Vertical,
                    children: vec![PaneTree::Leaf(PaneId(2)), PaneTree::Leaf(PaneId(3))],
                    weights: vec![1.0, 2.0],
                },
            ],
            weights: vec![1.0, 1.5],
        }
    }

    #[test]
    fn snapshot_then_restore_preserves_topology_and_weights() {
        let original = make_tree();
        let snap = snapshot_tree(&original);
        let mut next = 100u64;
        let restored = restore_tree(&snap, &mut || {
            let id = PaneId(next);
            next += 1;
            id
        });
        // PaneIds differ but the structural shape + weights match.
        match (&original, &restored) {
            (PaneTree::Split { weights: w1, .. }, PaneTree::Split { weights: w2, .. }) => {
                assert_eq!(w1, w2)
            }
            _ => panic!("structure mismatch"),
        }
        assert_eq!(count_leaves(&snap), 3);
    }

    #[test]
    fn settings_key_format() {
        assert_eq!(
            settings_key("proj_abc", "main"),
            "terminal_tabs:proj_abc:main"
        );
        assert_eq!(settings_key("proj_abc", "w1"), "terminal_tabs:proj_abc:w1");
    }

    #[test]
    fn legacy_settings_key_format() {
        assert_eq!(legacy_settings_key("proj_abc"), "terminal_tabs:proj_abc");
    }

    #[test]
    fn round_trip_json_serde() {
        let snap = snapshot_tree(&make_tree());
        let blob = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Terminal 1".into(),
                tree: snap,
                agent: None,
                kind: PersistedTabKind::Terminal,
                ..PersistedTab::default()
            }],
            active: 0,
            next_label_n: 2,
            tab_order: vec![0],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.next_label_n, 2);
        assert!(back.tabs[0].agent.is_none());
        assert_eq!(back.tab_order, vec![0]);
        assert_eq!(back.tabs[0].kind, PersistedTabKind::Terminal);
    }

    #[test]
    fn legacy_blob_without_agent_field_still_parses() {
        // Snapshots from pre-step-15 builds had no `agent` field. Verify
        // serde-default keeps them readable so the user's first relaunch
        // after upgrade doesn't drop their tab layout.
        let legacy =
            r#"{"tabs":[{"label":"Terminal 1","tree":"Leaf"}],"active":0,"next_label_n":2}"#;
        let parsed: PersistedTabs = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.tabs.len(), 1);
        assert!(parsed.tabs[0].agent.is_none());
        // Pre-v2 fields fall back to defaults — Terminal kind and empty
        // tab_order (caller infers insertion order).
        assert_eq!(parsed.tabs[0].kind, PersistedTabKind::Terminal);
        assert!(parsed.tab_order.is_empty());
    }

    #[test]
    fn round_trip_preserves_tab_cosmetics() {
        // Preview/pin/color/title must survive a quit→relaunch so a restored
        // strip looks exactly as the user left it.
        let blob = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "README.md".into(),
                tree: PersistedTree::Leaf,
                agent: None,
                kind: PersistedTabKind::Terminal,
                is_preview: true,
                pinned: true,
                color: Some("teal".into()),
                custom_title: Some("Notes".into()),
                ..PersistedTab::default()
            }],
            active: 0,
            next_label_n: 1,
            tab_order: vec![0],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        let tab = &back.tabs[0];
        assert!(tab.is_preview);
        assert!(tab.pinned);
        assert_eq!(tab.color.as_deref(), Some("teal"));
        assert_eq!(tab.custom_title.as_deref(), Some("Notes"));
    }

    #[test]
    fn legacy_blob_without_cosmetics_defaults_to_permanent() {
        // Blobs written before cosmetics were persisted must restore as
        // plain permanent tabs (no preview, no pin, no color/title) instead
        // of failing to parse.
        let legacy =
            r#"{"tabs":[{"label":"Terminal 1","tree":"Leaf"}],"active":0,"next_label_n":1}"#;
        let parsed: PersistedTabs = serde_json::from_str(legacy).unwrap();
        let tab = &parsed.tabs[0];
        assert!(!tab.is_preview);
        assert!(!tab.pinned);
        assert!(tab.color.is_none());
        assert!(tab.custom_title.is_none());
    }

    #[test]
    fn round_trip_agent_tab_preserves_metadata() {
        let blob = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Claude Code 1".into(),
                tree: PersistedTree::Leaf,
                agent: Some(PersistedAgentTab {
                    adapter: AgentAdapter::ClaudeCode,
                    adapter_id: "claude-code".into(),
                    worktree_path: "/tmp/proj".into(),
                    model: Some("claude-opus-4-7".into()),
                    effort: Some("high".into()),
                    relay_external_id: None,
                    relay_session: None,
                }),
                kind: PersistedTabKind::Terminal,
                ..PersistedTab::default()
            }],
            active: 0,
            next_label_n: 2,
            tab_order: vec![0],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        let agent = back.tabs[0].agent.as_ref().unwrap();
        assert_eq!(agent.adapter, AgentAdapter::ClaudeCode);
        assert_eq!(agent.adapter_id, "claude-code");
        assert_eq!(agent.worktree_path, "/tmp/proj");
        assert_eq!(agent.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(agent.effort.as_deref(), Some("high"));
    }

    #[test]
    fn round_trip_editor_tab_carries_path() {
        let blob = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "README.md".into(),
                tree: PersistedTree::Leaf,
                agent: None,
                kind: PersistedTabKind::Editor {
                    path: "/tmp/proj/README.md".into(),
                },
                ..PersistedTab::default()
            }],
            active: 0,
            next_label_n: 2,
            tab_order: vec![0],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back.tabs[0].kind,
            PersistedTabKind::Editor { ref path } if path == "/tmp/proj/README.md"
        ));
    }

    #[test]
    fn tab_order_round_trips() {
        // Three tabs, dragged to visual sequence [2, 0, 1].
        let blob = PersistedTabs {
            tabs: vec![
                PersistedTab {
                    label: "T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "T1".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "T2".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
            ],
            active: 1,
            next_label_n: 3,
            tab_order: vec![2, 0, 1],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tab_order, vec![2, 0, 1]);
    }

    #[test]
    fn legacy_blob_defaults_group_tree_to_none() {
        // Pre-v3 blobs had no `group_tree` / `groups` / `active_group`
        // fields. Serde-default keeps them readable; restore falls back
        // to the single-group path (driven by `group_tree.is_none()`).
        let legacy =
            r#"{"tabs":[{"label":"Terminal 1","tree":"Leaf"}],"active":0,"next_label_n":2}"#;
        let parsed: PersistedTabs = serde_json::from_str(legacy).unwrap();
        assert!(parsed.group_tree.is_none());
        assert!(parsed.groups.is_empty());
        assert_eq!(parsed.active_group, 0);
    }

    #[test]
    fn multi_group_tree_round_trips() {
        // Two side-by-side groups with one terminal each. The flat tabs
        // vec carries both in DFS order; `groups` records per-group
        // tab_count + active + tab_order. Restoring this blob in the
        // app would rebuild a 2-group horizontal split.
        let blob = PersistedTabs {
            tabs: vec![
                PersistedTab {
                    label: "G0-T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "G1-T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
            ],
            active: 1,
            next_label_n: 3,
            tab_order: vec![0, 1],
            group_tree: Some(PersistedTree::Split {
                axis: PersistedAxis::Horizontal,
                children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
                weights: vec![1.0, 1.0],
            }),
            groups: vec![
                PersistedGroup {
                    tab_count: 1,
                    active: 0,
                    tab_order: vec![0],
                },
                PersistedGroup {
                    tab_count: 1,
                    active: 0,
                    tab_order: vec![0],
                },
            ],
            active_group: 1,
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.active_group, 1);
        assert_eq!(back.groups.len(), 2);
        assert_eq!(back.groups[0].tab_count, 1);
        assert_eq!(back.groups[1].tab_count, 1);
        match back.group_tree.as_ref().expect("group_tree present") {
            PersistedTree::Split {
                axis,
                children,
                weights,
            } => {
                assert!(matches!(axis, PersistedAxis::Horizontal));
                assert_eq!(children.len(), 2);
                assert_eq!(weights, &vec![1.0, 1.0]);
            }
            _ => panic!("expected Split at root"),
        }
    }

    #[test]
    fn multi_group_preserves_per_group_active_and_order() {
        // Three groups, the middle one has 3 tabs reordered to [2,0,1]
        // with the second slot active. Verify the per-group state
        // round-trips through JSON intact.
        let blob = PersistedTabs {
            tabs: vec![
                PersistedTab {
                    label: "G0-T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "G1-T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "G1-T1".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "G1-T2".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
                PersistedTab {
                    label: "G2-T0".into(),
                    tree: PersistedTree::Leaf,
                    agent: None,
                    kind: PersistedTabKind::Terminal,
                    ..PersistedTab::default()
                },
            ],
            active: 2, // G1-T1 globally
            next_label_n: 5,
            tab_order: vec![0, 3, 1, 2, 4], // G0=[0], G1=[T2,T0,T1], G2=[0]
            group_tree: Some(PersistedTree::Split {
                axis: PersistedAxis::Horizontal,
                children: vec![
                    PersistedTree::Leaf,
                    PersistedTree::Split {
                        axis: PersistedAxis::Vertical,
                        children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
                        weights: vec![1.0, 1.0],
                    },
                ],
                weights: vec![1.0, 2.0],
            }),
            groups: vec![
                PersistedGroup {
                    tab_count: 1,
                    active: 0,
                    tab_order: vec![0],
                },
                PersistedGroup {
                    tab_count: 3,
                    active: 1, // G1-T1 locally
                    tab_order: vec![2, 0, 1],
                },
                PersistedGroup {
                    tab_count: 1,
                    active: 0,
                    tab_order: vec![0],
                },
            ],
            active_group: 1,
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.groups.len(), 3);
        assert_eq!(back.groups[1].tab_count, 3);
        assert_eq!(back.groups[1].active, 1);
        assert_eq!(back.groups[1].tab_order, vec![2, 0, 1]);
        assert_eq!(back.active_group, 1);
        // Flat tab_order projection survives too.
        assert_eq!(back.tab_order, vec![0, 3, 1, 2, 4]);
        // Tree's nested vertical split preserved on G1's leaf side.
        let group_tree = back.group_tree.as_ref().expect("group_tree present");
        let PersistedTree::Split { children, .. } = group_tree else {
            panic!("expected Split at root");
        };
        let PersistedTree::Split { axis, .. } = &children[1] else {
            panic!("expected nested split on right");
        };
        assert!(matches!(axis, PersistedAxis::Vertical));
    }

    #[test]
    fn sub_pane_tree_with_cwds_round_trips() {
        // 3-sub-pane terminal tab: horizontal split with the right side
        // further split vertically. Each leaf carries its captured cwd.
        // The blob must round-trip per-leaf cwd + active position
        // through JSON intact.
        let mut tab = PersistedTab {
            label: "Terminal 1".into(),
            tree: PersistedTree::Split {
                axis: PersistedAxis::Horizontal,
                children: vec![
                    PersistedTree::Leaf,
                    PersistedTree::Split {
                        axis: PersistedAxis::Vertical,
                        children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
                        weights: vec![1.0, 2.0],
                    },
                ],
                weights: vec![1.0, 1.5],
            },
            ..PersistedTab::default()
        };
        tab.sub_panes = vec![
            PersistedSubPane {
                cwd: Some("/tmp/a".into()),
                ..Default::default()
            },
            PersistedSubPane {
                cwd: Some("/tmp/b".into()),
                ..Default::default()
            },
            PersistedSubPane {
                cwd: None,
                ..Default::default()
            },
        ];
        tab.active_sub_pane = 2; // bottom-right leaf in DFS order
        let blob = PersistedTabs {
            tabs: vec![tab],
            active: 0,
            next_label_n: 2,
            tab_order: vec![0],
            ..PersistedTabs::default()
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tabs[0].sub_panes.len(), 3);
        assert_eq!(back.tabs[0].active_sub_pane, 2);
        assert_eq!(back.tabs[0].sub_panes[0].cwd.as_deref(), Some("/tmp/a"));
        assert_eq!(back.tabs[0].sub_panes[1].cwd.as_deref(), Some("/tmp/b"));
        assert_eq!(back.tabs[0].sub_panes[2].cwd, None);
        // Tree shape preserved.
        let PersistedTree::Split {
            axis,
            children,
            weights,
        } = &back.tabs[0].tree
        else {
            panic!("expected split root");
        };
        assert!(matches!(axis, PersistedAxis::Horizontal));
        assert_eq!(weights, &vec![1.0, 1.5]);
        assert_eq!(children.len(), 2);
        let PersistedTree::Split { axis: inner, .. } = &children[1] else {
            panic!("expected nested split");
        };
        assert!(matches!(inner, PersistedAxis::Vertical));
    }

    #[test]
    fn legacy_blob_defaults_sub_panes_empty() {
        // Pre-v4 blobs had no `sub_panes` / `active_sub_pane` fields.
        // Restore falls back to single-pane (handled by the factory's
        // `tab.sub_panes.len() > 1` check, which fails for empty).
        let legacy =
            r#"{"tabs":[{"label":"Terminal 1","tree":"Leaf"}],"active":0,"next_label_n":2}"#;
        let parsed: PersistedTabs = serde_json::from_str(legacy).unwrap();
        assert!(parsed.tabs[0].sub_panes.is_empty());
        assert_eq!(parsed.tabs[0].active_sub_pane, 0);
    }

    #[test]
    fn sub_pane_surface_and_tab_ids_round_trip() {
        let sp = PersistedSubPane {
            cwd: Some("/tmp/x".into()),
            surface_id: "surf-42".into(),
            tab_id: "tab-7".into(),
            ..Default::default()
        };
        let back: PersistedSubPane = serde_json::from_str(&serde_json::to_string(&sp).unwrap())
            .expect("sub-pane round-trips");
        assert_eq!(back.surface_id, "surf-42");
        assert_eq!(back.tab_id, "tab-7");
    }

    #[test]
    fn pre_context_env_sub_pane_defaults_ids_empty() {
        // A sub-pane blob from before context env (only `cwd`) must parse
        // with empty ids so the restorer can mint fresh ones.
        let legacy = r#"{"cwd":"/tmp/x"}"#;
        let parsed: PersistedSubPane = serde_json::from_str(legacy).unwrap();
        assert!(parsed.surface_id.is_empty());
        assert!(parsed.tab_id.is_empty());
        assert!(parsed.tabs.is_empty());
        assert_eq!(parsed.active_tab, 0);
    }

    #[test]
    fn multi_tab_leaf_round_trips() {
        let sp = PersistedSubPane {
            cwd: Some("/tmp/b".into()),
            surface_id: "surf-b".into(),
            tab_id: "tab-b".into(),
            tabs: vec![
                PersistedLeafTab {
                    cwd: Some("/tmp/a".into()),
                    surface_id: "surf-a".into(),
                    tab_id: "tab-a".into(),
                },
                PersistedLeafTab {
                    cwd: Some("/tmp/b".into()),
                    surface_id: "surf-b".into(),
                    tab_id: "tab-b".into(),
                },
            ],
            active_tab: 1,
        };
        let back: PersistedSubPane =
            serde_json::from_str(&serde_json::to_string(&sp).unwrap()).unwrap();
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.active_tab, 1);
        assert_eq!(back.tabs[0].tab_id, "tab-a");
        assert_eq!(back.tabs[1].surface_id, "surf-b");
        // Top-level fields mirror the active tab for legacy readers.
        assert_eq!(back.tab_id, "tab-b");
    }

    #[test]
    fn group_tree_snapshot_then_restore_via_pane_group_id() {
        // The same `snapshot_tree` / `restore_tree` helpers must work
        // for the workspace's `PaneTree<PaneGroupId>` (not just the
        // per-tab content tree). Pin the genericity here.
        use crate::shell::pane_tree::PaneGroupId;
        let original: PaneTree<PaneGroupId> = PaneTree::Split {
            axis: Axis::Horizontal,
            children: vec![
                PaneTree::Leaf(PaneGroupId(0)),
                PaneTree::Leaf(PaneGroupId(1)),
            ],
            weights: vec![1.0, 2.5],
        };
        let snap = snapshot_tree(&original);
        let mut next = 10u64;
        let restored: PaneTree<PaneGroupId> = restore_tree(&snap, &mut || {
            let id = PaneGroupId(next);
            next += 1;
            id
        });
        // Ids re-allocated, weights + topology preserved.
        let PaneTree::Split {
            children, weights, ..
        } = &restored
        else {
            panic!("expected split");
        };
        assert_eq!(weights, &vec![1.0, 2.5]);
        assert_eq!(children.len(), 2);
        assert_eq!(
            restored.in_order_leaves(),
            vec![PaneGroupId(10), PaneGroupId(11)]
        );
    }
}
