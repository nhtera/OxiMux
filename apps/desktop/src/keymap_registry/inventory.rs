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
    ReloadCustomCommands, Search, SelectExplorerTab, SelectHistoryTab, SelectSearchTab,
    SelectSourceControlTab,
    SendLastCommandOutputToAgent, SendTerminalSelectionToAgent, SplitHorizontal,
    SplitSubPaneDown, SplitSubPaneRight, SplitVertical, ToggleChatTerminalView,
    ToggleDictation, ToggleFloatingTerminal, ToggleLeftSidebar, ToggleRightSidebar,
    ToggleZoomSubPane, UiZoomIn, UiZoomOut, UiZoomReset,
};
// The AppKit application menu's actions, bound only where that menu exists.
#[cfg(target_os = "macos")]
use crate::menu::{Copy, HideApp, HideOthers, Minimize, Quit};
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
/// - Chords are written with gpui's `secondary-` modifier, NOT `cmd-`. The two
///   are the same key on macOS, but `cmd-` maps to `platform`, which off macOS
///   is the *Windows* key — so `cmd-p` would have become Win+P (Project a
///   display) rather than Quick Open. `secondary-` is Command on macOS and
///   Control elsewhere, which is what every one of these chords means.
/// - Shift is stripped and the key remapped to the shifted character
///   (`]`→`}`, `[`→`{`), so binding strings use the post-shift char —
///   `secondary-shift-]` would never match. Long recorded here as a macOS
///   quirk; it is not. gpui's Windows backend does the same thing for the
///   punctuation and digit keys (`get_keystroke_key` → `get_shifted_key`,
///   which clears `modifiers.shift`), which is why a `secondary-shift-=`
///   binding was measured matching nothing on Windows either.
/// - ctrl-tab is the cross-platform "last tab" standard (cmd-tab is
///   reserved by macOS for app switching).
/// - ctrl-shift-1/2/3 are free of macOS system bindings.
pub const ACTIONS: &[ActionSpec] = &[
    // ---- Global -----------------------------------------------------
    entry!("open_settings", "Open settings", Global, "secondary-,", OpenSettings),
    entry!("new_window", "New window", Global, "secondary-n", NewWindow),
    entry!("open_project_picker", "Open project", Global, "secondary-o", OpenProjectPicker),
    entry!("open_workspace_create", "New workspace", Global, "secondary-shift-n", OpenWorkspaceCreate),
    entry!("new_agent", "New agent", Global, "secondary-shift-a", NewAgent),
    entry!("new_agent_chat", "New agent chat", Global, "secondary-shift-c", NewAgentChat),
    entry!("toggle_dictation", "Toggle voice dictation", Global, "secondary-e", ToggleDictation),
    entry!(
        "toggle_floating_terminal",
        "Toggle floating terminal",
        Global,
        "secondary-shift-t",
        ToggleFloatingTerminal
    ),
    entry!(
        "toggle_chat_terminal_view",
        "Toggle chat/terminal view",
        Global,
        "ctrl-shift-v",
        ToggleChatTerminalView
    ),
    entry!("save_file", "Save file", Global, "secondary-s", SaveFile),
    entry!("editor_zoom_in", "Zoom in editor", Global, "secondary-=", EditorZoomIn),
    entry!("editor_zoom_out", "Zoom out editor", Global, "secondary--", EditorZoomOut),
    entry!("editor_zoom_reset", "Reset editor zoom", Global, "secondary-0", EditorZoomReset),
    // Interface zoom sits on the shifted versions of the editor's chords: same
    // gesture, wider blast radius. Written with the post-shift characters
    // because that is what both platforms deliver — see the chord notes above;
    // `secondary-shift-=` matches nothing anywhere and was measured doing
    // exactly that.
    entry!("ui_zoom_in", "Zoom in interface", Global, "secondary-+", UiZoomIn),
    entry!("ui_zoom_out", "Zoom out interface", Global, "secondary-_", UiZoomOut),
    // Reset has no default chord. Its natural one is ⌘⇧0, and Windows does not
    // deliver Ctrl+Shift+0 to applications at all — a documented OS-level quirk
    // that gpui's own keyboard handling carries a note about. Rather than ship
    // a shortcut that works on one platform and silently does nothing on the
    // other, reset lives in Settings → Appearance and the command palette,
    // where it is discoverable on both. A user who wants a key can bind one.
    entry!("ui_zoom_reset", "Reset interface zoom", Global, "", UiZoomReset),
    entry!(
        "reload_custom_commands",
        "Reload custom commands",
        Global,
        "",
        ReloadCustomCommands
    ),
    // The AppKit application menu's own items. Deliberately NOT migrated to
    // `secondary-`: three of the four name concepts Windows does not have
    // (hide app, hide others), and the Ctrl chords they would translate into
    // are spoken for elsewhere — Ctrl+H is Replace, Ctrl+M is Enter in a
    // terminal. An action that cannot work should not hold a chord hostage.
    #[cfg(target_os = "macos")]
    entry!("quit", "Quit OxiMux", Global, "cmd-q", Quit),
    #[cfg(target_os = "macos")]
    entry!("hide_app", "Hide OxiMux", Global, "cmd-h", HideApp),
    #[cfg(target_os = "macos")]
    entry!("hide_others", "Hide others", Global, "cmd-alt-h", HideOthers),
    #[cfg(target_os = "macos")]
    entry!("minimize_window", "Minimize window", Global, "cmd-m", Minimize),
    // Copy is bound here so the keystroke reaches GPUI at all. The Edit menu
    // declares it as an `OsAction`, and with no keymap entry AppKit owned ⌘C
    // outright: measured in a live window, neither a key-down listener nor an
    // action handler anywhere in the app saw it. `Global`, because Copy means
    // copy in every pane; the transcript's handler consumes it only when there
    // is a transcript selection and otherwise lets it through.
    #[cfg(target_os = "macos")]
    entry!("copy", "Copy", Global, "cmd-c", Copy),
    // ---- Tabs -------------------------------------------------------
    entry!("new_tab", "New tab", Tabs, "secondary-t", NewTab),
    entry!("new_browser_tab", "New browser tab", Tabs, "secondary-shift-b", NewBrowserTab),
    // cmd-w closes the active sub-pane when the focused tab has multiple
    // sub-panes, else the per-pane tab, else the whole tab (cascade lives
    // in `PaneGroup::on_close_tab`).
    entry!("close_tab", "Close tab / sub-pane", Tabs, "secondary-w", CloseTab),
    entry!("next_tab", "Next tab", Tabs, "secondary-}", NextTab),
    entry!("prev_tab", "Previous tab", Tabs, "secondary-{", PrevTab),
    entry!("mru_next_tab", "Most-recent tab", Tabs, "ctrl-tab", MruNext),
    entry!("mru_prev_tab", "Least-recent tab", Tabs, "ctrl-shift-tab", MruPrev),
    // ---- Panes & Layout ----------------------------------------------
    // cmd-d / cmd-shift-d split the focused terminal tab into sub-panes;
    // tab-GROUP splits default unbound (palette + context menu).
    entry!("split_sub_pane_right", "Split sub-pane right", Panes, "secondary-d", SplitSubPaneRight),
    entry!("split_sub_pane_down", "Split sub-pane down", Panes, "secondary-shift-d", SplitSubPaneDown),
    entry!(
        "split_group_horizontal",
        "Split pane group horizontally",
        Panes,
        "",
        SplitHorizontal
    ),
    entry!("split_group_vertical", "Split pane group vertically", Panes, "", SplitVertical),
    entry!("focus_next_sub_pane", "Focus next sub-pane", Panes, "secondary-]", FocusNextSubPane),
    entry!("focus_prev_sub_pane", "Focus previous sub-pane", Panes, "secondary-[", FocusPrevSubPane),
    entry!(
        "focus_next_pane_group",
        "Focus next pane group",
        Panes,
        "secondary-shift-}",
        FocusNextPane
    ),
    entry!(
        "focus_prev_pane_group",
        "Focus previous pane group",
        Panes,
        "secondary-shift-{",
        FocusPrevPane
    ),
    // Zoom = expand the focused sub-pane; second press restores.
    entry!("toggle_zoom_sub_pane", "Zoom sub-pane", Panes, "secondary-shift-enter", ToggleZoomSubPane),
    entry!("close_pane_group", "Close pane group", Panes, "secondary-shift-w", CloseGroup),
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
    entry!("search_scrollback", "Search scrollback", Terminal, "secondary-f", Search),
    entry!("find_next_match", "Find next match", Terminal, "secondary-g", FindNextMatch),
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
        "secondary-shift-i",
        SendTerminalSelectionToAgent
    ),
    entry!(
        "send_last_output_to_agent",
        "Send last output to agent",
        Terminal,
        "secondary-shift-o",
        SendLastCommandOutputToAgent
    ),
    // ---- Source Control ----------------------------------------------
    entry!("open_commit_dialog", "Commit dialog", Scm, "secondary-k", OpenCommitDialog),
    entry!(
        "refresh_source_control",
        "Refresh source control",
        Scm,
        "secondary-r",
        RefreshSourceControl
    ),
    entry!(
        "select_source_control_tab",
        "Source control tab",
        Scm,
        "secondary-shift-g",
        SelectSourceControlTab
    ),
    // ---- Navigation ---------------------------------------------------
    entry!("open_quick_open", "Quick Open (files)", Navigation, "secondary-p", OpenQuickOpen),
    entry!(
        "open_composer_bar",
        "Compose prompt to agent",
        Navigation,
        "secondary-i",
        OpenComposerBar
    ),
    entry!(
        "open_session_history",
        "Session history (resume / fork)",
        Navigation,
        "secondary-shift-h",
        OpenSessionHistory
    ),
    // Command palette uses cmd-shift-p (cmd-k is the commit dialog).
    entry!(
        "open_command_palette",
        "Command palette",
        Navigation,
        "secondary-shift-p",
        OpenCommandPalette
    ),
    // Jump to any workspace/worktree across all projects.
    entry!("open_workspace_jump", "Jump to workspace", Navigation, "secondary-j", OpenWorkspaceJump),
    entry!("toggle_left_sidebar", "Toggle left sidebar", Navigation, "secondary-b", ToggleLeftSidebar),
    entry!(
        "toggle_right_sidebar",
        "Toggle right sidebar",
        Navigation,
        "secondary-l",
        ToggleRightSidebar
    ),
    entry!("select_explorer_tab", "Explorer tab", Navigation, "secondary-shift-e", SelectExplorerTab),
    entry!("select_search_tab", "Search tab", Navigation, "secondary-shift-f", SelectSearchTab),
    entry!("select_history_tab", "Session History tab", Navigation, "secondary-shift-y", SelectHistoryTab),
    // Browser-style steps through this window's workspace-activation history.
    entry!(
        "nav_workspace_back",
        "Workspace history back",
        Navigation,
        "secondary-alt-left",
        NavWorkspaceBack
    ),
    entry!(
        "nav_workspace_forward",
        "Workspace history forward",
        Navigation,
        "secondary-alt-right",
        NavWorkspaceForward
    ),
    // Esc dismisses any open transient overlay (handled at WorkspaceRoot).
    entry!("dismiss_overlay", "Dismiss overlay", Navigation, "escape", DismissOverlay),
];
