//! Workspace-level actions, surfaced via key bindings.
//!
//! Defined as unit-struct actions via `gpui::actions!`. Bindings register
//! once at app boot (`main.rs` after `gpui_component::init`); handlers live
//! on the owning entity's root `div` via `.on_action(cx.listener(...))`.
//! GPUI dispatches actions up the element tree from the focused leaf, so
//! the focused `TerminalView` keystrokes still reach the shell while
//! `Cmd-*` combos bubble up to `WorkspaceRoot`.

use gpui::{Action, actions};

/// Payload action used by the per-pane "..." button. Carries the click's
/// absolute window coordinates so the shared `PaneActionsMenu` can anchor
/// itself to the chip instead of the workspace's top-right edge.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenPaneActionsAt {
    pub x: f32,
    pub y: f32,
}

/// Open the inline adapter-picker popover anchored to the `+` button.
///
/// Fired by:
/// - The `+` button's right-click handler — `x = Some(cursor_x)` so the
///   popover anchors under the clicked group's `+`. Fixes the multi-group
///   anchor bug (without this, the popover always landed under the first
///   group's `+`).
/// - The `NewAgent` keybind handler — `x = None`, meaning "use the
///   keyboard fallback anchor" (post-rail inset, computed by the handler).
///
/// `WorkspaceRoot`'s handler treats `x` as: `Some(px)` → anchor directly
/// at that pixel; `None` → use the post-rail fallback. A second dispatch
/// while the popover is open toggles it closed.
///
/// `Default` is hand-written because `Option::None` is the keyboard
/// fallback — both `derive(Default)` and dispatcher tests rely on it.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
#[derive(Default)]
pub struct RequestOpenAdapterPicker {
    pub x: Option<f32>,
}

/// Payload action fired by a tab chip's right-click. Carries the cursor
/// coords plus the target group's id and the right-clicked tab index so
/// the shared `TabContextMenu` knows which (group, tab) the user picked
/// even if focus moves before they select an item.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenTabContextMenuAt {
    pub x: f32,
    pub y: f32,
    pub group_id: u64,
    pub tab_idx: u32,
    /// When `true`, open the compact tab-header **view-options** menu (just
    /// "Switch to Terminal View") instead of the full right-click context menu.
    /// Fired by the agent-chat tab's eye button; the right-click path leaves it
    /// `false` for the full menu.
    pub view_only: bool,
}

/// Payload action fired by a right-click inside the terminal GRID (not the
/// tab chip). Carries the cursor coords, the right-clicked session id (so
/// `WorkspaceRoot` can resolve the exact `TerminalView` even across splits),
/// whether a selection is active at click time (drives the Copy / Send-to-
/// Agent enabled state), and the link token under the cursor if any (drives
/// the Open Link / Copy Link rows).
///
/// `link` is a `String` because `PathBuf`/enums aren't `Action`-compatible;
/// the receiver re-classifies it as URL vs `path:line:col` when opening.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
#[derive(Default)]
pub struct OpenTerminalContextMenuAt {
    pub x: f32,
    pub y: f32,
    pub session_id: u64,
    pub has_selection: bool,
    pub link: Option<String>,
}

/// Payload action fired by a workspace-strip chip's left-click. Carries
/// (group_id, tab_idx) so `WorkspaceRoot` can switch the active pane group
/// AND activate the right tab within it.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct ActivateGroupTab {
    pub group_id: u64,
    pub tab_idx: u32,
}

/// Payload action fired by the tab right-click "Change Title…" row.
/// Carries (group_id, tab_idx) so `WorkspaceRoot` can open a rename
/// modal targeted at that tab regardless of which group has focus when
/// the user clicks the row.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct RequestRenameTabAt {
    pub group_id: u64,
    pub tab_idx: u32,
}

/// Payload action fired by the tab right-click "Split X" rows. Splits a
/// SPECIFIC group (not necessarily the focused one) so right-clicking a
/// tab in any group can split that group without first stealing focus.
/// `direction` is encoded as a `(axis, insert_before)` pair:
///   - axis: 0 = Horizontal (left/right), 1 = Vertical (up/down)
///   - insert_before: true = Left/Up (insert before), false = Right/Down
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct SplitGroupAt {
    pub group_id: u64,
    pub axis: u8,
    pub insert_before: bool,
}

/// Payload action fired by the tab right-click "Pin Tab" / "Unpin Tab"
/// row. Carries (group_id, tab_idx) so the handler can flip the right
/// chip even after focus moves. The action is a toggle — render reads
/// `is_pinned(tab_idx)` to pick the row label.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct TogglePinTabAt {
    pub group_id: u64,
    pub tab_idx: u32,
}

/// Payload action fired by the tab right-click "Move Tab to New Window"
/// row (terminal tabs backed by a relay PTY only). The source window
/// handler detaches the relay session(s), removes the tab, pushes a
/// `PendingTearOff` into the process-global mailbox keyed by the minted
/// destination `window_id`, then dispatches this action to the app-level
/// handler in `main.rs` which opens the new window. The destination window
/// build routine consumes the pending entry and mounts the tab there.
///
/// Non-terminal tabs and terminal tabs whose backend has no external relay
/// id (in-process fallback) render this item disabled/hidden.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct MoveTabToNewWindow {
    pub group_id: u64,
    pub tab_idx: u32,
}

/// Payload action fired by a file-tree row's right-click. Carries the
/// cursor coords + the right-clicked path + a directory flag so the
/// shared `FileTreeContextMenu` can render the right item set (files
/// get Open / Open to the Side / Copy Relative; directories get just
/// Copy Path + Reveal).
///
/// `PathBuf` doesn't implement `Action`, so the path is shipped as a
/// `String`; receivers parse via `PathBuf::from`.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenFileTreeContextMenuAt {
    pub x: f32,
    pub y: f32,
    pub path: String,
    pub is_dir: bool,
}

/// Payload action fired by `FileTreeContextMenu`'s Open / Open to the
/// Side rows. Carries the path + a flag deciding whether to split right
/// before opening. Lives at the workspace level so the menu doesn't
/// need a weak self-handle into `ProjectPanes`.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenFileFromContextMenu {
    pub path: String,
    pub split_right: bool,
}

/// Fired when a workspace-jump (Cmd+J) palette row is activated. Carries the
/// minimal identity `WorkspaceRoot::activate_workspace` needs so the handler
/// builds the target `Workspace` without re-resolving (synthesized "primary"
/// rows are not DB rows). Dispatched at the workspace level.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct ActivateWorkspaceFromJump {
    pub workspace_id: String,
    pub project_id: String,
    pub worktree_path: String,
}

/// Right-click in the empty area below the file tree. Carries the
/// active project's root path so the menu can offer "New File" /
/// "New Folder" at the workspace root without the path being inferred
/// from a row.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenFileTreeBackgroundMenuAt {
    pub x: f32,
    pub y: f32,
    pub root: String,
}

/// Open Search panel seeded with a glob targeting the clicked folder.
/// `path` is the absolute folder path; the action handler converts it
/// to a `<rel>/**` include glob and switches the right sidebar's
/// active tab to Search.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FindInFolder {
    pub path: String,
}

/// Open the active project's root (or a specific subdirectory) in
/// VS Code. Falls back to a tracing warning when `code` isn't on PATH.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenInVSCode {
    pub path: String,
}

/// Open a directory in Finder (not Reveal — used by the toolbar
/// overflow "Open in Finder" item, which targets the workspace root).
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenInFinder {
    pub path: String,
}

/// Phase 02 stub: dispatched from the context-menu New File / New
/// Folder / Rename / Delete rows. Phase 03 wires the inline-input,
/// confirm-dialog, and fs ops behind it. Today the handler logs to
/// tracing so the end-to-end click → action → handler path is testable.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FileTreeNewFile {
    /// Parent directory the new file should land in.
    pub parent: String,
}

#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FileTreeNewFolder {
    pub parent: String,
}

#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FileTreeRename {
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FileTreeDelete {
    pub path: String,
}

/// Duplicate a file or folder next to itself with a collision-free
/// " copy" name. Dispatched from the file-tree context menu's Duplicate row.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct FileTreeDuplicate {
    pub path: String,
}

/// Right-click on a git_panel file row. Carries cursor coords + the
/// clicked row's path + section flags + the current selection set so
/// the shared `GitRowContextMenu` can pick the right scope variant
/// (Single / Multi / Folder) at open time.
///
/// `selection_paths` is populated only when the clicked row is part
/// of a >1 multi-select — that's what flips the menu into the Multi
/// variant. `folder_leaves` is populated only when `is_folder` is
/// true (tree-mode folder right-click).
///
/// `PathBuf` / `Vec<PathBuf>` aren't `Action`-compatible, so paths
/// ride as `String` / `Vec<String>` and receivers parse via
/// `PathBuf::from`.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenGitRowContextMenuAt {
    pub x: f32,
    pub y: f32,
    pub path: String,
    pub is_staged: bool,
    pub is_untracked: bool,
    pub is_folder: bool,
    pub selection_paths: Vec<String>,
    pub folder_leaves: Vec<String>,
}

/// Right-click on a commit-graph row in the source control panel.
/// Carries cursor coords + the full 40-char OID + the 8-char short OID
/// so the shared `CommitContextMenu` renders the Cherry-pick / Revert /
/// Copy SHA / Copy short SHA items without re-deriving the short prefix.
///
/// Mounted at `WorkspaceRoot` level (same pattern as
/// `FileTreeContextMenu` and `GitRowContextMenu`) so dismissal +
/// peer-overlay close-on-open behavior is shared.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenCommitContextMenuAt {
    pub x: f32,
    pub y: f32,
    pub sha: String,
    pub short_sha: String,
}

/// Payload action carrying text up to the workspace so it can resolve "the
/// active agent session" and stream the bytes through its CLI runtime.
/// `text` is sent verbatim — callers decide whether to append a trailing
/// newline: `TerminalView` send-selection leaves it newline-free so the
/// user reviews and presses Enter, while custom-command palette entries
/// append `\n` so the prompt auto-submits.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct SendTextToActiveAgent {
    pub text: String,
}

/// Payload action carrying an element picked in the embedded browser up to the
/// workspace, which resolves the target Agent Chat and stages the capture as a
/// `@browser` context chip plus an image attachment.
///
/// Separate from [`SendTextToActiveAgent`] because the two land on different
/// surfaces: that one bracketed-pastes into a PTY (where an image cannot go),
/// this one stages into a chat composer. The workspace falls back to
/// `SendTextToActiveAgent` with `markdown` alone when no chat tab is open, so a
/// terminal-only workspace keeps the behavior it had.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct SendPickToActiveChat {
    /// The element block from `browser_view::agent_context::format_pick`.
    pub markdown: String,
    /// The picked element's selector — becomes the chip's source label.
    pub selector: String,
    /// PNG of the element's bounding box. Empty when the crop failed or the
    /// platform has no native snapshot, in which case the chip is sent alone.
    pub png: Vec<u8>,
}

/// Payload action raised by the session-history picker to relaunch a past
/// session. The workspace resolves the adapter slug + cwd (falling back to
/// the active project's directory when the log omits one) and threads
/// resume/fork into the spawn.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct ResumeAgentSession {
    pub session_id: String,
    pub adapter: oximux_core::AgentAdapter,
    /// Import-provider preset id (`opencode`/`copilot`/`pi`) for a row whose
    /// store the index scanned directly. When set, the resume spawns a `Custom`
    /// PTY via `import_resume_command` instead of a native adapter launch.
    pub preset_id: Option<String>,
    /// The provider's native resume handle passed to `import_resume_command`:
    /// the session id (OpenCode/Copilot) or the rollout file path (Pi).
    pub resume_handle: String,
    pub cwd: String,
    pub fork: bool,
}

/// Payload action raised by the Session History side panel to reopen a past
/// Claude session as a chat tab. The workspace routes it to the active pane
/// group, which imports the transcript from `path` (empty → resume with no
/// pre-rendered history) and spawns a resumed chat rooted at `cwd`.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct OpenChatSession {
    pub session_id: String,
    pub path: String,
    pub cwd: String,
    /// Which agent produced the session — routes the reopen to the matching
    /// transport + transcript importer (Claude `--resume` + JSONL, Codex
    /// `thread/resume` + rollout). Defaults to Claude for older call sites.
    pub adapter: oximux_core::AgentAdapter,
    /// Import-provider preset id (`opencode`/`pi`) for a row whose store the
    /// index scanned directly. When set, the reopen builds a transcript-only
    /// **bridge** chat (seeded via `load_import_provider_transcript`, no live
    /// connection) whose composer is swapped for a Resume-in-terminal action —
    /// these providers have no in-app chat backend. `None` for native rows.
    pub preset_id: Option<String>,
}

/// Route-up action raised when a *New Agent* draft's "Run in a fresh worktree"
/// send lands: the leaf chat view carries no `WorkspaceRepo`, so it emits an
/// `AgentChatEvent` that the pane group turns into this action. `WorkspaceRoot`
/// (which owns `app_state`) resolves the active chat, creates the worktree +
/// `Workspace` DB row via `create_workspace_with_rollback`, then feeds the
/// outcome back through the chat's `on_worktree_create_outcome` callback so the
/// staged send resumes at the new worktree cwd. Mirrors the
/// [`SendTextToActiveAgent`] "resolve the active agent on the spot" shape.
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = oximux, no_json)]
pub struct CreateWorktreeWorkspaceForActiveChat {
    /// Pre-validated slug (`validate_slug` ran in the roster before this fired);
    /// the branch is `oximux/<slug>`.
    pub slug: String,
}

actions!(
    oximux,
    [
        /// Toggle the prompt composer bar over the active agent pane — an
        /// elevated multi-line draft with `@file` autocomplete. No-op when the
        /// active tab is not an agent.
        OpenComposerBar,
        /// Toggle voice dictation in the focused Agent Chat composer (mic on/off).
        /// Bound to Cmd+E. No-op when the active surface isn't a chat composer or
        /// dictation is disabled in Settings.
        ToggleDictation,
        /// Open the session-history picker — a centered modal listing past
        /// Claude Code / Codex sessions to resume or fork. Bound to Cmd+Shift+H.
        OpenSessionHistory,
        /// Reopen the highlighted session-history entry as a chat tab (Claude
        /// only). Dispatched by a modal-scoped `Cmd+Enter` binding — a real
        /// action rather than the modal's `on_key_down`, since macOS routes
        /// Cmd-modified keys through `performKeyEquivalent:` and they never
        /// reach element key listeners.
        OpenHistoryEntryAsChat,
        /// Cycle the session-history agent-type filter segment
        /// (All→Claude→Codex→OpenCode→All). Dispatched by a modal-scoped `Tab`
        /// binding — a real action rather than the modal's `on_key_down`,
        /// because GPUI consumes `Tab` for focus-navigation before it reaches
        /// element key listeners.
        CycleSessionTypeFilter,
        /// Split the focused pane horizontally — new pane on the right.
        /// Alias of `SplitRight`. Kept for legacy callers; the Cmd+D
        /// keybinding now drives `SplitSubPaneRight` instead so it
        /// matches the reference editor's sub-pane semantics.
        SplitHorizontal,
        /// Split the focused pane vertically — new pane on the bottom.
        /// Alias of `SplitDown`. See `SplitHorizontal` note above.
        SplitVertical,
        /// Sub-pane split: add a new PTY column to the RIGHT of the
        /// active terminal sub-pane in the focused tab. Bound to Cmd+D.
        SplitSubPaneRight,
        /// Sub-pane split: add a new PTY row BELOW the active terminal
        /// sub-pane in the focused tab. Bound to Cmd+Shift+D.
        SplitSubPaneDown,
        /// Cycle sub-pane focus to the NEXT live sub-pane in in-order
        /// traversal of the active tab's tree. Bound to Cmd+].
        FocusNextSubPane,
        /// Cycle sub-pane focus to the PREVIOUS live sub-pane. Cmd+[.
        FocusPrevSubPane,
        /// Zoom (maximize) the active sub-pane to fill the tab body;
        /// toggle to restore the tree layout. Bound to Cmd+Shift+Enter.
        ToggleZoomSubPane,
        /// Four-direction split: new pane on the right of focus.
        SplitRight,
        /// Four-direction split: new pane below focus.
        SplitDown,
        /// Four-direction split: new pane on the left of focus.
        SplitLeft,
        /// Four-direction split: new pane above focus.
        SplitUp,
        /// Close the focused pane group. No-op when it's the only group.
        CloseGroup,
        /// Dismiss any open transient overlay (pane actions menu, tab
        /// context menu, adapter picker). Bound to Escape.
        DismissOverlay,
        /// Open the Pane Actions dropdown (split direction picker).
        OpenPaneActions,
        /// Cycle focus to the next pane in in-order traversal.
        FocusNextPane,
        /// Cycle focus to the previous pane in in-order traversal.
        FocusPrevPane,
        /// Open a new terminal tab at the workspace (group) level.
        NewTab,
        /// Open a new embedded browser tab at the workspace (group) level.
        /// Bound to Cmd+Shift+B.
        NewBrowserTab,
        /// Open a new terminal tab INSIDE the active split pane (leaf),
        /// adding it to that pane's own tab strip. Bound to Cmd+Shift+T.
        NewTabInPane,
        /// Open a new top-level workspace window (Cmd+N). Handled by a
        /// global app-level listener in `main.rs` since it needs `&mut App`
        /// to call `open_window`; each window owns an independent
        /// `WorkspaceRoot` + project panes.
        NewWindow,
        /// Close the active tab in the focused pane. Cascades to `ClosePane`
        /// when it was the last tab so cmd-w does the right thing in both
        /// the multi-tab and single-tab case.
        CloseTab,
        /// Cycle to the next tab inside the focused pane.
        NextTab,
        /// Cycle to the previous tab inside the focused pane.
        PrevTab,
        /// Switch to the most-recently-used previous tab (MRU[1]) in the
        /// focused group. Repeated press toggles between the two most-
        /// recent tabs — the classic "Alt+Tab to last" muscle memory.
        /// Bound to Ctrl+Tab.
        MruNext,
        /// Switch to the LEAST-recently-used tab in the focused group
        /// (MRU.last()). Useful for cycling back through history without
        /// a HUD. Bound to Ctrl+Shift+Tab.
        MruPrev,
        /// Open the scrollback search overlay on the focused terminal pane.
        Search,
        /// Cycle the search overlay to the next match (wrapping), scrolling
        /// the viewport to keep the current match visible. No-op when the
        /// overlay is closed.
        FindNextMatch,
        /// Cycle the search overlay to the previous match. See FindNextMatch.
        FindPrevMatch,
        /// Toggle the git changed-files panel (binding moved to SelectSourceControlTab).
        OpenGitPanel,
        /// Refresh the source-control panel — re-runs the status poll
        /// against the active worktree AND reloads the commit graph's
        /// first page. Bound to Cmd+R; dispatched globally so the
        /// shortcut works regardless of which pane has focus inside
        /// the workspace. Cheap (a single poll-tick equivalent), so
        /// the binding doesn't need a focus context guard.
        RefreshSourceControl,
        /// Stage the file currently selected in the git panel.
        StageFile,
        /// Unstage the file currently selected in the git panel.
        UnstageFile,
        /// Revert (discard) worktree changes for the selected file. Routes
        /// through the discard confirm modal.
        RevertFile,
        /// Trigger the diff view to load the currently selected file in the
        /// git panel. Dispatched up the element tree from GitPanel row
        /// clicks; intercepted at the shell mount (step 14) or directly by
        /// DiffView when wired as a sibling.
        OpenDiff,
        /// Expand a collapsed (large) diff in the DiffView. Wired to the
        /// click on the "expand" affordance row.
        ExpandDiff,
        /// Retry a failed diff load. Wired to the Retry button on the
        /// failed-state row; reads stored path / staged / untracked flags
        /// from `DiffViewState::Failed`.
        RetryDiff,
        /// Open the commit dialog modal (step 14 binds Cmd+K).
        OpenCommitDialog,
        /// Submit the active commit dialog. Routed via dialog button click;
        /// declared here so step 14 can also bind Cmd+Enter to it.
        CommitStaged,
        /// Make the whole interface one step larger.
        ///
        /// Not the same control as `EditorZoomIn`, which is deliberately left
        /// on the bare ⌘+ / ⌘- / ⌘0 it has always had: that one changes the
        /// code font in the editor and diff body, the way it does in every
        /// other editor, and people reach for it constantly. This one moves
        /// the chrome as well and is a preference, not a per-file gesture, so
        /// it takes the shifted chord and is persisted.
        UiZoomIn,
        /// One step smaller — see [`UiZoomIn`].
        UiZoomOut,
        /// Back to 100% — see [`UiZoomIn`].
        UiZoomReset,
        /// Toggle the left rail (workspaces + nav) visibility (Cmd+B).
        ToggleLeftSidebar,
        /// Toggle the right sidebar visibility (Cmd+L).
        ToggleRightSidebar,
        /// Switch to the Files tab in the right sidebar (Cmd+Shift+T).
        /// Bound after Cmd+Shift+E/F/G — `T` for the file tree to avoid
        /// the Cmd+B (left sidebar) and Cmd+Shift+F (search) collisions.
        SelectFilesTab,
        /// Switch to the Explorer tab in the right sidebar (Cmd+Shift+E).
        SelectExplorerTab,
        /// Switch to the Search tab in the right sidebar (Cmd+Shift+F).
        SelectSearchTab,
        /// Switch to the Source Control tab in the right sidebar (Cmd+Shift+G).
        SelectSourceControlTab,
        /// Switch to the Session History tab in the right sidebar (Cmd+Shift+Y).
        SelectHistoryTab,
        /// Open the file/worktree Quick Open palette (Cmd+P). Phase 05 shell;
        /// backend file index lands in a later plan.
        OpenQuickOpen,
        /// Open the action Command Palette (Cmd+Shift+P). Phase 05.
        OpenCommandPalette,
        /// Open the workspace-jump palette (Cmd+J): fuzzy-jump to any
        /// workspace/worktree across every project.
        OpenWorkspaceJump,
        /// Open the inline adapter-picker popover anchored to the `+`
        /// button. Cmd+Shift+A reroutes here so the keyboard and mouse
        /// paths converge on the same surface. A second dispatch while the
        /// popover is open closes it (toggle).
        NewAgent,
        /// Open a new structured Agent Chat tab (Claude `stream-json`) in the
        /// active pane group. Distinct from `NewAgent` (a raw-PTY terminal
        /// agent): this renders the session as a chat thread. Bound to
        /// Cmd+Shift+C.
        NewAgentChat,
        /// Open the project picker modal (Cmd+O). Shows recent projects +
        /// "Open Folder…" affordance backed by a native NSOpenPanel.
        OpenProjectPicker,
        /// Open the workspace create dialog (Cmd+Shift+N). No-ops when no
        /// active project is set — welcome screen prompts the user to
        /// Cmd+O first.
        OpenWorkspaceCreate,
        /// Open the 3-card "Add a project" modal (Browse folder / Clone
        /// from URL / Remote project). Browse is wired; the other two
        /// are disabled "Coming soon" stubs for v1.
        OpenAddProjectDialog,
        /// Send the focused terminal's active selection to the active
        /// agent session's input buffer. No-op when no selection or no
        /// agent is open. Bound to Cmd+Shift+I ("Insert into agent").
        SendTerminalSelectionToAgent,
        /// Send the most-recent completed command's output (bracketed by
        /// P8 OSC 133/633 marks) to the active agent's input buffer.
        /// Requires shell-integration marks to be emitted; falls back
        /// to a no-op if fewer than two prompt marks are present. Bound
        /// to Cmd+Shift+O ("Output to agent").
        SendLastCommandOutputToAgent,
        /// Reshape the active project's pane groups into a single vertical
        /// stack (all panes top-to-bottom). Tab content is preserved.
        ApplyLayoutStacked,
        /// Reshape the active project's pane groups side-by-side in a
        /// single horizontal row. Tab content is preserved.
        ApplyLayoutHorizontal,
        /// Reshape the active project's pane groups into a top-content /
        /// bottom-terminal split, docking a terminal-bearing group at the
        /// bottom. An existing terminal group is reused; when none exists a
        /// new terminal group is spawned. Tab content is preserved.
        ApplyLayoutBottomTerminal,
        /// Re-read `commands.toml` from the global app data dir and the
        /// active project's `.oximux/commands.toml`, merging them into the
        /// palette's custom command list. No file watcher — reload is
        /// manual via this palette entry.
        ReloadCustomCommands,
        /// Open the settings modal (Cmd+,). Toggles closed on a second
        /// dispatch. Surfaces the terminal + AI settings that already
        /// round-trip to disk, a read-only keybindings list, and an
        /// appearance/about pane. Also reachable via the left-rail cog.
        OpenSettings,
        /// Open the settings modal directly at the About pane — version, app
        /// data dir, and the update controls. The menu's "About OxiMux" on
        /// both platforms; not bound to a chord, because About is a thing you
        /// go looking for once, not a thing you reach for.
        OpenAbout,
        /// Ask the updater for a manual check and show the answer. Opens the
        /// About pane first: a check whose result lands on a surface nobody is
        /// looking at is indistinguishable from one that never ran.
        CheckForUpdates,
        /// Quit and relaunch to apply a staged update. Only meaningful when
        /// the updater reports a ready version; the swap itself happens in
        /// the quit path, so this is "quit, then come back".
        RestartToUpdate,
        /// Toggle the "What's New" popover under the title-bar Update pill:
        /// the staged release's notes plus the restart button. Only dispatched
        /// while the updater reports a ready version (the pill is the only
        /// trigger and it renders only in that state).
        ToggleWhatsNew,
        /// Reopen the first-run welcome wizard (default agent + chat view).
        /// Palette-only — reopening never clears the completion flag, so it
        /// cannot re-trigger at boot.
        ShowWelcomeWizard,
        /// Toggle the in-window floating ("PiP") terminal (Cmd+Shift+T).
        /// First dispatch spawns it at the active worktree cwd; subsequent
        /// dispatches show/hide it (the PTY persists across hides). The card's
        /// close button tears the PTY down.
        ToggleFloatingTerminal,
        /// Toggle the active agent-chat tab between its chat and a companion
        /// terminal running the same session resumed interactively (Ctrl+Shift+V).
        /// No-op unless the active tab is an agent chat with a session to resume.
        ToggleChatTerminalView,
        /// Navigate BACK through the per-window workspace-activation history
        /// (browser-style). Bound to Cmd+Alt+Left. No-op at the oldest entry.
        NavWorkspaceBack,
        /// Navigate FORWARD through the workspace-activation history. Bound to
        /// Cmd+Alt+Right. No-op at the newest entry.
        NavWorkspaceForward,
    ]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_git_row_context_menu_default_is_empty_single() {
        let a = OpenGitRowContextMenuAt::default();
        assert_eq!(a.x, 0.0);
        assert_eq!(a.y, 0.0);
        assert!(a.path.is_empty());
        assert!(!a.is_staged);
        assert!(!a.is_untracked);
        assert!(!a.is_folder);
        assert!(a.selection_paths.is_empty());
        assert!(a.folder_leaves.is_empty());
    }

    #[test]
    fn open_git_row_context_menu_round_trip_payload() {
        // A multi-select right-click on a Staged row carrying 3 paths.
        let original = OpenGitRowContextMenuAt {
            x: 100.0,
            y: 200.0,
            path: "src/lib.rs".to_string(),
            is_staged: true,
            is_untracked: false,
            is_folder: false,
            selection_paths: vec![
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
                "Cargo.toml".to_string(),
            ],
            folder_leaves: Vec::new(),
        };
        // Clone + PartialEq prove the derive surface is intact (Action
        // dispatch relies on Clone; equality is the test surface for
        // payload preservation).
        let dup = original.clone();
        assert_eq!(original, dup);
        assert_eq!(dup.selection_paths.len(), 3);
        assert!(dup.is_staged);
    }

    #[test]
    fn open_commit_context_menu_default_is_empty() {
        let a = OpenCommitContextMenuAt::default();
        assert_eq!(a.x, 0.0);
        assert_eq!(a.y, 0.0);
        assert!(a.sha.is_empty());
        assert!(a.short_sha.is_empty());
    }

    #[test]
    fn open_commit_context_menu_round_trip_payload() {
        let original = OpenCommitContextMenuAt {
            x: 200.0,
            y: 320.5,
            sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            short_sha: "abcdef01".to_string(),
        };
        let dup = original.clone();
        assert_eq!(original, dup);
        assert_eq!(dup.sha.len(), 40);
        assert_eq!(dup.short_sha.len(), 8);
    }

    #[test]
    fn open_git_row_context_menu_folder_payload() {
        let folder = OpenGitRowContextMenuAt {
            x: 0.0,
            y: 0.0,
            path: "src".to_string(),
            is_staged: false,
            is_untracked: true,
            is_folder: true,
            selection_paths: Vec::new(),
            folder_leaves: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
        };
        assert!(folder.is_folder);
        assert!(folder.is_untracked);
        assert_eq!(folder.folder_leaves.len(), 2);
    }
}
