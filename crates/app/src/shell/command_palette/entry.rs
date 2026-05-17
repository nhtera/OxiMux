//! Palette entry types — mode enum, command catalog, action factory map.
//!
//! `PALETTE_COMMANDS` is a static registry. Phase N+1 may replace it with
//! `cx.available_actions()` if GPUI exposes such an iterator.

use gpui::Action;

use crate::actions::{
    CloseTab, NewTab, OpenCommandPalette, OpenCommitDialog, OpenQuickOpen, Search,
    SelectSourceControlTab, SplitHorizontal, SplitVertical, ToggleLeftSidebar, ToggleRightSidebar,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    QuickOpen,
    Commands,
}

/// One actionable command shown in the Command Palette mode.
#[derive(Debug, Clone, Copy)]
pub struct CommandEntry {
    pub name: &'static str,
    pub keybinding: Option<&'static str>,
    /// Factory returning a fresh `Box<dyn Action>` each call. `fn` pointer
    /// (not closure) so it can live in a `const` array.
    pub make_action: fn() -> Box<dyn Action>,
}

/// Static command catalog. Order = render order; first item is the default
/// selected row in Command Palette mode.
pub const PALETTE_COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        name: "Split Pane Horizontally",
        keybinding: Some("⌘D"),
        make_action: || Box::new(SplitHorizontal),
    },
    CommandEntry {
        name: "Split Pane Vertically",
        keybinding: Some("⌘⇧D"),
        make_action: || Box::new(SplitVertical),
    },
    CommandEntry {
        name: "New Tab",
        keybinding: Some("⌘T"),
        make_action: || Box::new(NewTab),
    },
    CommandEntry {
        name: "Close Tab",
        keybinding: Some("⌘W"),
        make_action: || Box::new(CloseTab),
    },
    CommandEntry {
        name: "Toggle Right Sidebar",
        keybinding: Some("⌘L"),
        make_action: || Box::new(ToggleRightSidebar),
    },
    CommandEntry {
        name: "Toggle Left Sidebar",
        keybinding: Some("⌘B"),
        make_action: || Box::new(ToggleLeftSidebar),
    },
    CommandEntry {
        name: "Source Control",
        keybinding: Some("⌘⇧G"),
        make_action: || Box::new(SelectSourceControlTab),
    },
    CommandEntry {
        name: "Search Pane",
        keybinding: Some("⌘F"),
        make_action: || Box::new(Search),
    },
    CommandEntry {
        name: "Open Commit Dialog",
        keybinding: Some("⌘K"),
        make_action: || Box::new(OpenCommitDialog),
    },
    CommandEntry {
        name: "Quick Open",
        keybinding: Some("⌘P"),
        make_action: || Box::new(OpenQuickOpen),
    },
    CommandEntry {
        name: "Command Palette",
        keybinding: Some("⌘⇧P"),
        make_action: || Box::new(OpenCommandPalette),
    },
];

/// Quick Open stub data — replaced by a live file index in a later plan.
pub const QUICK_OPEN_STUBS: &[&str] = &["src/main.rs", "src/lib.rs", "Cargo.toml"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_entry_displays_keybinding_glyphs() {
        assert_eq!(PALETTE_COMMANDS[0].keybinding, Some("⌘D"));
    }

    #[test]
    fn palette_commands_has_eleven_entries() {
        assert_eq!(PALETTE_COMMANDS.len(), 11);
    }

    #[test]
    fn quick_open_stubs_includes_main_rs() {
        assert!(QUICK_OPEN_STUBS.contains(&"src/main.rs"));
    }

    #[test]
    fn every_command_has_keybinding() {
        assert!(PALETTE_COMMANDS.iter().all(|c| c.keybinding.is_some()));
    }
}
