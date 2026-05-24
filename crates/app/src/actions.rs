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

actions!(
    oximux,
    [
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
        /// Open a new terminal tab inside the focused pane.
        NewTab,
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
        /// Toggle the git changed-files panel (binding moved to SelectSourceControlTab).
        OpenGitPanel,
        /// Stage the file currently selected in the git panel.
        StageFile,
        /// Unstage the file currently selected in the git panel.
        UnstageFile,
        /// Revert (discard) worktree changes for the selected file. Step 8 is
        /// a no-op stub; step 11 wires the type-to-confirm modal.
        RevertFile,
        /// Trigger the diff view to load the currently selected file in the
        /// git panel. Dispatched up the element tree from GitPanel row
        /// clicks; intercepted at the shell mount (step 14) or directly by
        /// DiffView when wired as a sibling.
        OpenDiff,
        /// Expand a collapsed (large) diff in the DiffView. Wired to the
        /// click on the "expand" affordance row.
        ExpandDiff,
        /// Open the commit dialog modal (step 14 binds Cmd+K).
        OpenCommitDialog,
        /// Submit the active commit dialog. Routed via dialog button click;
        /// declared here so step 14 can also bind Cmd+Enter to it.
        CommitStaged,
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
        /// Open the file/worktree Quick Open palette (Cmd+P). Phase 05 shell;
        /// backend file index lands in a later plan.
        OpenQuickOpen,
        /// Open the action Command Palette (Cmd+Shift+P). Phase 05.
        OpenCommandPalette,
        /// Open the inline adapter-picker popover anchored to the `+`
        /// button. Cmd+Shift+A reroutes here so the keyboard and mouse
        /// paths converge on the same surface. A second dispatch while the
        /// popover is open closes it (toggle).
        NewAgent,
        /// Workspace-internal request to open the adapter picker. Fired by
        /// the `+` button's `on_mouse_down` and by the `NewAgent` action
        /// handler; handled in `WorkspaceRoot`. Kept distinct from
        /// `NewAgent` so the button-fired path doesn't fight a focus-chain
        /// keystroke during state recovery.
        RequestOpenAdapterPicker,
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
    ]
);
