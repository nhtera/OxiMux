//! Palette entry types — mode enum, static command catalog, and the unified
//! runtime item type that merges built-in and custom commands.
//!
//! The static `PALETTE_COMMANDS` catalog uses non-capturing fn pointers so
//! it can live in a `const` array. Custom commands (loaded from TOML at
//! runtime) carry a prompt string and cannot fit that shape, so they are
//! represented separately via `PaletteItemAction::Custom`.

use gpui::Action;

use crate::actions::{
    ApplyLayoutBottomTerminal, ApplyLayoutHorizontal, ApplyLayoutStacked, CloseTab, NewTab,
    OpenCommandPalette, OpenCommitDialog, OpenQuickOpen, ReloadCustomCommands, Search,
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

/// The action bound to a resolved palette row at open/render time. Built-in
/// commands carry a fn-pointer factory; custom commands carry an owned prompt
/// string that is dispatched via `SendTextToActiveAgent`.
#[derive(Clone)]
pub enum PaletteItemAction {
    /// Built-in command: factory fn that produces a `Box<dyn Action>`.
    Builtin(fn() -> Box<dyn Action>),
    /// User-defined command: prompt text sent to the active agent session,
    /// with a trailing newline appended so the agent auto-submits it.
    Custom(String),
}

/// A resolved palette row built at open time by merging the static catalog
/// with the loaded custom commands. `display_group` drives the group
/// separator rendering ("Commands" vs "Custom").
#[derive(Clone)]
pub struct PaletteItem {
    pub name: String,
    pub keybinding: Option<&'static str>,
    pub action: PaletteItemAction,
    pub display_group: PaletteGroup,
}

/// Group label for palette rows — controls the visual separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteGroup {
    /// Built-in app commands from the static catalog.
    Commands,
    /// User-defined custom commands loaded from TOML.
    Custom,
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
    CommandEntry {
        name: "Layout: Stacked",
        keybinding: Some("⌃⇧1"),
        make_action: || Box::new(ApplyLayoutStacked),
    },
    CommandEntry {
        name: "Layout: Horizontal",
        keybinding: Some("⌃⇧2"),
        make_action: || Box::new(ApplyLayoutHorizontal),
    },
    CommandEntry {
        name: "Layout: Bottom Terminal",
        keybinding: Some("⌃⇧3"),
        make_action: || Box::new(ApplyLayoutBottomTerminal),
    },
    CommandEntry {
        name: "Reload Custom Commands",
        keybinding: None,
        make_action: || Box::new(ReloadCustomCommands),
    },
];

/// Build the unified candidate list from the static catalog plus loaded
/// custom commands. Built-in entries come first under the "Commands" group;
/// custom entries appear after under the "Custom" group.
pub fn build_palette_items(custom_commands: &[oximux_settings::CustomCommand]) -> Vec<PaletteItem> {
    let mut items: Vec<PaletteItem> = PALETTE_COMMANDS
        .iter()
        .map(|c| PaletteItem {
            name: c.name.to_string(),
            keybinding: c.keybinding,
            action: PaletteItemAction::Builtin(c.make_action),
            display_group: PaletteGroup::Commands,
        })
        .collect();

    for cc in custom_commands {
        if cc.name.is_empty() {
            continue;
        }
        items.push(PaletteItem {
            name: cc.name.clone(),
            keybinding: None,
            action: PaletteItemAction::Custom(cc.prompt.clone()),
            display_group: PaletteGroup::Custom,
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_entry_displays_keybinding_glyphs() {
        assert_eq!(PALETTE_COMMANDS[0].keybinding, Some("⌘D"));
    }

    #[test]
    fn palette_commands_has_fifteen_entries() {
        // 14 original + 1 "Reload Custom Commands"
        assert_eq!(PALETTE_COMMANDS.len(), 15);
    }

    #[test]
    fn reload_custom_commands_entry_present() {
        assert!(PALETTE_COMMANDS
            .iter()
            .any(|c| c.name == "Reload Custom Commands"));
    }

    #[test]
    fn build_palette_items_appends_custom_commands() {
        let custom = vec![
            oximux_settings::CustomCommand {
                name: "My Command".to_string(),
                prompt: "do something".to_string(),
                scope: None,
            },
        ];
        let items = build_palette_items(&custom);
        // All built-ins + 1 custom
        assert_eq!(items.len(), PALETTE_COMMANDS.len() + 1);
        let custom_item = items.last().unwrap();
        assert_eq!(custom_item.name, "My Command");
        assert_eq!(custom_item.display_group, PaletteGroup::Custom);
        assert!(matches!(custom_item.action, PaletteItemAction::Custom(_)));
    }

    #[test]
    fn build_palette_items_skips_empty_name_commands() {
        let custom = vec![
            oximux_settings::CustomCommand {
                name: String::new(),
                prompt: "orphan".to_string(),
                scope: None,
            },
        ];
        let items = build_palette_items(&custom);
        assert_eq!(items.len(), PALETTE_COMMANDS.len());
    }

    #[test]
    fn builtin_items_have_commands_group() {
        let items = build_palette_items(&[]);
        assert!(items.iter().all(|i| i.display_group == PaletteGroup::Commands));
    }
}
