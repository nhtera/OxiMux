//! The action inventory — every user-facing action with its id, label,
//! category, and default chord. Replaces the old hand-mirrored keymap +
//! settings-pane table pair; this table is the only place a default chord
//! is written. Deliberately a data table, so it may exceed the usual file
//! LOC cap.
//!
//! Clipboard shortcuts (⌘C/⌘V/⌘X) are intentionally absent: the terminal
//! owns them, and a global binding would shadow it.

use gpui::KeyBinding;

use super::{ActionSpec, Category};
use crate::actions::{
    ApplyLayoutBottomTerminal, ApplyLayoutHorizontal, ApplyLayoutStacked, CloseGroup, CloseTab,
    DismissOverlay, FindNextMatch, FindPrevMatch, FocusNextPane, FocusNextSubPane, FocusPrevPane,
    FocusPrevSubPane, MruNext, MruPrev, NavWorkspaceBack, NavWorkspaceForward, NewAgent,
    NewAgentChat, NewBrowserTab, NewTab, NewWindow, NextTab, OpenCommandPalette, OpenCommitDialog,
    OpenComposerBar, OpenProjectPicker, OpenQuickOpen, OpenSessionHistory,
    OpenSettings, OpenWorkspaceCreate, OpenWorkspaceJump, PrevTab, RefreshSourceControl,
    ReloadCustomCommands, Search, SelectExplorerTab, SelectSearchTab, SelectSourceControlTab,
    SendLastCommandOutputToAgent, SendTerminalSelectionToAgent, SplitHorizontal,
    SplitSubPaneDown, SplitSubPaneRight, SplitVertical, ToggleFloatingTerminal, ToggleLeftSidebar,
    ToggleRightSidebar, ToggleZoomSubPane,
};
use crate::menu::{HideApp, HideOthers, Minimize, Quit};
use oximux_editor::{EditorZoomIn, EditorZoomOut, EditorZoomReset, SaveFile};

/// Shorthand for the per-entry bind fn — each closure is non-capturing so
/// it coerces to a `fn` pointer and the table stays `const`.
macro_rules! entry {
    ($id:literal, $label:literal, $cat:ident, $chord:literal, $action:expr) => {
        ActionSpec {
            id: $id,
            label: $label,
            category: Category::$cat,
            default_chord: $chord,
            bind: |chord| KeyBinding::new(chord, $action, None),
        }
    };
}

/// Chord notes inherited from the original keymap:
/// - macOS strips `shift` and remaps the key to the shifted character
///   (`]`→`}`, `[`→`{`), so binding strings use the post-shift char —
///   `cmd-shift-]` would never match.
/// - ctrl-tab is the cross-platform "last tab" standard (cmd-tab is
///   reserved by macOS for app switching).
/// - ctrl-shift-1/2/3 are free of macOS system bindings.
pub const ACTIONS: &[ActionSpec] = &[
    // ---- Global -----------------------------------------------------
    entry!("open_settings", "Open settings", Global, "cmd-,", OpenSettings),
    entry!("new_window", "New window", Global, "cmd-n", NewWindow),
    entry!("open_project_picker", "Open project", Global, "cmd-o", OpenProjectPicker),
    entry!("open_workspace_create", "New workspace", Global, "cmd-shift-n", OpenWorkspaceCreate),
    entry!("new_agent", "New agent", Global, "cmd-shift-a", NewAgent),
    entry!("new_agent_chat", "New agent chat", Global, "cmd-shift-c", NewAgentChat),
    entry!(
        "toggle_floating_terminal",
        "Toggle floating terminal",
        Global,
        "cmd-shift-t",
        ToggleFloatingTerminal
    ),
    entry!("save_file", "Save file", Global, "cmd-s", SaveFile),
    entry!("editor_zoom_in", "Zoom in editor", Global, "cmd-=", EditorZoomIn),
    entry!("editor_zoom_out", "Zoom out editor", Global, "cmd--", EditorZoomOut),
    entry!("editor_zoom_reset", "Reset editor zoom", Global, "cmd-0", EditorZoomReset),
    entry!(
        "reload_custom_commands",
        "Reload custom commands",
        Global,
        "",
        ReloadCustomCommands
    ),
    entry!("quit", "Quit OxiMux", Global, "cmd-q", Quit),
    entry!("hide_app", "Hide OxiMux", Global, "cmd-h", HideApp),
    entry!("hide_others", "Hide others", Global, "cmd-alt-h", HideOthers),
    entry!("minimize_window", "Minimize window", Global, "cmd-m", Minimize),
    // ---- Tabs -------------------------------------------------------
    entry!("new_tab", "New tab", Tabs, "cmd-t", NewTab),
    entry!("new_browser_tab", "New browser tab", Tabs, "cmd-shift-b", NewBrowserTab),
    // cmd-w closes the active sub-pane when the focused tab has multiple
    // sub-panes, else the per-pane tab, else the whole tab (cascade lives
    // in `PaneGroup::on_close_tab`).
    entry!("close_tab", "Close tab / sub-pane", Tabs, "cmd-w", CloseTab),
    entry!("next_tab", "Next tab", Tabs, "cmd-}", NextTab),
    entry!("prev_tab", "Previous tab", Tabs, "cmd-{", PrevTab),
    entry!("mru_next_tab", "Most-recent tab", Tabs, "ctrl-tab", MruNext),
    entry!("mru_prev_tab", "Least-recent tab", Tabs, "ctrl-shift-tab", MruPrev),
    // ---- Panes & Layout ----------------------------------------------
    // cmd-d / cmd-shift-d split the focused terminal tab into sub-panes;
    // tab-GROUP splits default unbound (palette + context menu).
    entry!("split_sub_pane_right", "Split sub-pane right", Panes, "cmd-d", SplitSubPaneRight),
    entry!("split_sub_pane_down", "Split sub-pane down", Panes, "cmd-shift-d", SplitSubPaneDown),
    entry!(
        "split_group_horizontal",
        "Split pane group horizontally",
        Panes,
        "",
        SplitHorizontal
    ),
    entry!("split_group_vertical", "Split pane group vertically", Panes, "", SplitVertical),
    entry!("focus_next_sub_pane", "Focus next sub-pane", Panes, "cmd-]", FocusNextSubPane),
    entry!("focus_prev_sub_pane", "Focus previous sub-pane", Panes, "cmd-[", FocusPrevSubPane),
    entry!(
        "focus_next_pane_group",
        "Focus next pane group",
        Panes,
        "cmd-shift-}",
        FocusNextPane
    ),
    entry!(
        "focus_prev_pane_group",
        "Focus previous pane group",
        Panes,
        "cmd-shift-{",
        FocusPrevPane
    ),
    // Zoom = expand the focused sub-pane; second press restores.
    entry!("toggle_zoom_sub_pane", "Zoom sub-pane", Panes, "cmd-shift-enter", ToggleZoomSubPane),
    entry!("close_pane_group", "Close pane group", Panes, "cmd-shift-w", CloseGroup),
    entry!("layout_stacked", "Layout: stacked", Panes, "ctrl-shift-1", ApplyLayoutStacked),
    entry!(
        "layout_horizontal",
        "Layout: side-by-side",
        Panes,
        "ctrl-shift-2",
        ApplyLayoutHorizontal
    ),
    entry!(
        "layout_bottom_terminal",
        "Layout: bottom terminal",
        Panes,
        "ctrl-shift-3",
        ApplyLayoutBottomTerminal
    ),
    // ---- Terminal & Agents -------------------------------------------
    entry!("search_scrollback", "Search scrollback", Terminal, "cmd-f", Search),
    entry!("find_next_match", "Find next match", Terminal, "cmd-g", FindNextMatch),
    // cmd-shift-g (the platform "find previous" convention) is owned by
    // select_source_control_tab, a shipped default; prev gets the alt
    // variant instead. Both are user-rebindable.
    entry!("find_prev_match", "Find previous match", Terminal, "alt-cmd-g", FindPrevMatch),
    // Sends the focused terminal's selection to the active agent's input
    // buffer (no trailing newline — the user reviews + hits Enter).
    entry!(
        "send_selection_to_agent",
        "Send selection to agent",
        Terminal,
        "cmd-shift-i",
        SendTerminalSelectionToAgent
    ),
    entry!(
        "send_last_output_to_agent",
        "Send last output to agent",
        Terminal,
        "cmd-shift-o",
        SendLastCommandOutputToAgent
    ),
    // ---- Source Control ----------------------------------------------
    entry!("open_commit_dialog", "Commit dialog", Scm, "cmd-k", OpenCommitDialog),
    entry!(
        "refresh_source_control",
        "Refresh source control",
        Scm,
        "cmd-r",
        RefreshSourceControl
    ),
    entry!(
        "select_source_control_tab",
        "Source control tab",
        Scm,
        "cmd-shift-g",
        SelectSourceControlTab
    ),
    // ---- Navigation ---------------------------------------------------
    entry!("open_quick_open", "Quick Open (files)", Navigation, "cmd-p", OpenQuickOpen),
    entry!(
        "open_composer_bar",
        "Compose prompt to agent",
        Navigation,
        "cmd-i",
        OpenComposerBar
    ),
    entry!(
        "open_session_history",
        "Session history (resume / fork)",
        Navigation,
        "cmd-shift-h",
        OpenSessionHistory
    ),
    // Command palette uses cmd-shift-p (cmd-k is the commit dialog).
    entry!(
        "open_command_palette",
        "Command palette",
        Navigation,
        "cmd-shift-p",
        OpenCommandPalette
    ),
    // Jump to any workspace/worktree across all projects.
    entry!("open_workspace_jump", "Jump to workspace", Navigation, "cmd-j", OpenWorkspaceJump),
    entry!("toggle_left_sidebar", "Toggle left sidebar", Navigation, "cmd-b", ToggleLeftSidebar),
    entry!(
        "toggle_right_sidebar",
        "Toggle right sidebar",
        Navigation,
        "cmd-l",
        ToggleRightSidebar
    ),
    entry!("select_explorer_tab", "Explorer tab", Navigation, "cmd-shift-e", SelectExplorerTab),
    entry!("select_search_tab", "Search tab", Navigation, "cmd-shift-f", SelectSearchTab),
    // Browser-style steps through this window's workspace-activation history.
    entry!(
        "nav_workspace_back",
        "Workspace history back",
        Navigation,
        "cmd-alt-left",
        NavWorkspaceBack
    ),
    entry!(
        "nav_workspace_forward",
        "Workspace history forward",
        Navigation,
        "cmd-alt-right",
        NavWorkspaceForward
    ),
    // Esc dismisses any open transient overlay (handled at WorkspaceRoot).
    entry!("dismiss_overlay", "Dismiss overlay", Navigation, "escape", DismissOverlay),
];
