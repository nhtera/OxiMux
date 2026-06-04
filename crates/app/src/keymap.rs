//! The app's global key bindings, in one place so the binary entry point
//! and the headless keymap-driven E2E tests install the *same* map — a
//! `simulate_keystrokes` test then exercises the real keystroke → keymap →
//! action dispatch path, not just a direct handler call.

use gpui::KeyBinding;
use oximux_editor::SaveFile;

use crate::actions::{
    CloseGroup, CloseTab, DismissOverlay, FocusNextPane, FocusNextSubPane, FocusPrevPane,
    FocusPrevSubPane, MruNext, MruPrev, NewAgent, NewTab, NewWindow, NextTab,
    OpenCommandPalette, OpenCommitDialog, OpenProjectPicker, OpenQuickOpen, OpenWorkspaceCreate,
    PrevTab, RefreshSourceControl, Search, SelectExplorerTab, SelectSearchTab,
    SelectSourceControlTab, SendLastCommandOutputToAgent, SendTerminalSelectionToAgent,
    SplitSubPaneDown, SplitSubPaneRight, ToggleLeftSidebar, ToggleRightSidebar, ToggleZoomSubPane,
};

/// Build the default global key bindings. `main` installs these at boot via
/// `cx.bind_keys(default_key_bindings())`; tests install the identical list
/// so simulated keystrokes route through the production keymap.
pub fn default_key_bindings() -> Vec<KeyBinding> {
    vec![
        // cmd-d / cmd-shift-d split the focused terminal tab into sub-panes.
        // Tab-GROUP splits stay on the tab right-click + Pane Actions menu.
        KeyBinding::new("cmd-d", SplitSubPaneRight, None),
        KeyBinding::new("cmd-shift-d", SplitSubPaneDown, None),
        // cmd-w closes the active sub-pane when the focused tab has multiple
        // sub-panes, else the per-pane tab, else the whole tab. The cascade
        // disambiguation lives in `PaneGroup::on_close_tab`.
        KeyBinding::new("cmd-w", CloseTab, None),
        // cmd-shift-w closes the focused pane GROUP (no-op with one group).
        KeyBinding::new("cmd-shift-w", CloseGroup, None),
        // cmd-[ / cmd-] cycle sub-pane focus within the active tab.
        KeyBinding::new("cmd-]", FocusNextSubPane, None),
        KeyBinding::new("cmd-[", FocusPrevSubPane, None),
        // cmd-shift-] / cmd-shift-[ cycle GROUP focus. macOS shifts the key
        // to `}` / `{` post-modifier, so the binding strings must use the
        // post-shift character (see the cmd-} / cmd-{ note below).
        KeyBinding::new("cmd-shift-}", FocusNextPane, None),
        KeyBinding::new("cmd-shift-{", FocusPrevPane, None),
        // cmd-shift-enter zooms the focused sub-pane; second press restores.
        KeyBinding::new("cmd-shift-enter", ToggleZoomSubPane, None),
        KeyBinding::new("cmd-t", NewTab, None),
        // cmd-n opens a new top-level window (each with its own WorkspaceRoot).
        KeyBinding::new("cmd-n", NewWindow, None),
        // macOS strips `shift` and remaps the key to the shifted character
        // (`]`→`}`, `[`→`{`); binding strings must use the post-shift char —
        // `cmd-shift-]` would never match.
        KeyBinding::new("cmd-}", NextTab, None),
        KeyBinding::new("cmd-{", PrevTab, None),
        // MRU cycle — ctrl-tab is the cross-platform "last tab" standard
        // (cmd-tab is reserved by macOS for app switching).
        KeyBinding::new("ctrl-tab", MruNext, None),
        KeyBinding::new("ctrl-shift-tab", MruPrev, None),
        // cmd-f opens the focused pane's scrollback search overlay.
        KeyBinding::new("cmd-f", Search, None),
        // Sidebar + tab selection.
        KeyBinding::new("cmd-b", ToggleLeftSidebar, None),
        KeyBinding::new("cmd-l", ToggleRightSidebar, None),
        KeyBinding::new("cmd-shift-e", SelectExplorerTab, None),
        KeyBinding::new("cmd-shift-f", SelectSearchTab, None),
        KeyBinding::new("cmd-shift-g", SelectSourceControlTab, None),
        KeyBinding::new("cmd-k", OpenCommitDialog, None),
        // Command palette uses cmd-shift-p (cmd-k is the commit dialog).
        KeyBinding::new("cmd-p", OpenQuickOpen, None),
        KeyBinding::new("cmd-shift-p", OpenCommandPalette, None),
        KeyBinding::new("cmd-o", OpenProjectPicker, None),
        KeyBinding::new("cmd-shift-n", OpenWorkspaceCreate, None),
        // cmd-shift-a spawns a new agent tab using the first built-in adapter.
        KeyBinding::new("cmd-shift-a", NewAgent, None),
        // cmd-shift-i sends the focused terminal's selection to the
        // active agent's input buffer (no trailing newline, so the user
        // reviews + hits Enter). cmd-shift-o sends the last completed
        // command's output (bracketed by P8 shell-integration marks).
        KeyBinding::new("cmd-shift-i", SendTerminalSelectionToAgent, None),
        KeyBinding::new("cmd-shift-o", SendLastCommandOutputToAgent, None),
        // cmd-r refreshes the source-control panel (status poll + graph
        // page reload). Dispatched globally so the user can fire from any
        // pane focus; cheap (single poll-tick equivalent), no rate limit.
        KeyBinding::new("cmd-r", RefreshSourceControl, None),
        // cmd-s saves the active editor buffer; no-op when no editor focused.
        KeyBinding::new("cmd-s", SaveFile, None),
        // Escape dismisses any open transient overlay (handled at WorkspaceRoot).
        KeyBinding::new("escape", DismissOverlay, None),
    ]
}
