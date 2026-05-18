//! Workspace-level actions, surfaced via key bindings.
//!
//! Defined as unit-struct actions via `gpui::actions!`. Bindings register
//! once at app boot (`main.rs` after `gpui_component::init`); handlers live
//! on the owning entity's root `div` via `.on_action(cx.listener(...))`.
//! GPUI dispatches actions up the element tree from the focused leaf, so
//! the focused `TerminalView` keystrokes still reach the shell while
//! `Cmd-*` combos bubble up to `WorkspaceRoot`.

use gpui::actions;

actions!(
    oximux,
    [
        /// Split the focused pane horizontally — new pane on the right.
        /// Alias of `SplitRight` (kept for the existing Cmd+D binding).
        SplitHorizontal,
        /// Split the focused pane vertically — new pane on the bottom.
        /// Alias of `SplitDown` (kept for the existing Cmd+Shift+D binding).
        SplitVertical,
        /// four-direction split: new pane on the right of focus.
        SplitRight,
        /// four-direction split: new pane below focus.
        SplitDown,
        /// four-direction split: new pane on the left of focus.
        SplitLeft,
        /// four-direction split: new pane above focus.
        SplitUp,
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
    ]
);
