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
    SelectSourceControlTab, ShowWelcomeWizard, SplitHorizontal, SplitVertical, ToggleLeftSidebar,
    ToggleRightSidebar,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    QuickOpen,
    Commands,
    /// Cmd+J: fuzzy-jump to any workspace/worktree across all projects.
    WorkspaceJump,
}

/// One workspace row in the Cmd+J jump palette. Carries the minimal identity
/// `WorkspaceRoot::activate_workspace` needs (`project_id` + `id` +
/// `worktree_path`) so activation never has to re-resolve the row — important
/// because the synthesized "primary" rows (id `primary:<project>`) are not DB
/// rows. `attention` is the row's `attention_rank` (lower = more urgent),
/// used to float action-needing workspaces to the top of the browse list.
#[derive(Debug, Clone)]
pub struct WorkspaceJumpItem {
    pub workspace_id: String,
    pub project_id: String,
    pub worktree_path: String,
    pub label: String,
    pub attention: u8,
}

/// One actionable command shown in the Command Palette mode.
#[derive(Debug, Clone, Copy)]
pub struct CommandEntry {
    pub name: &'static str,
    /// Keymap-registry id; the displayed chord is resolved live from the
    /// registry at build time so user overrides show up in the palette.
    pub action_id: Option<&'static str>,
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
    pub keybinding: Option<String>,
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
        action_id: Some("split_group_horizontal"),
        make_action: || Box::new(SplitHorizontal),
    },
    CommandEntry {
        name: "Split Pane Vertically",
        action_id: Some("split_group_vertical"),
        make_action: || Box::new(SplitVertical),
    },
    CommandEntry {
        name: "New Tab",
        action_id: Some("new_tab"),
        make_action: || Box::new(NewTab),
    },
    CommandEntry {
        name: "Close Tab",
        action_id: Some("close_tab"),
        make_action: || Box::new(CloseTab),
    },
    CommandEntry {
        name: "Toggle Right Sidebar",
        action_id: Some("toggle_right_sidebar"),
        make_action: || Box::new(ToggleRightSidebar),
    },
    CommandEntry {
        name: "Toggle Left Sidebar",
        action_id: Some("toggle_left_sidebar"),
        make_action: || Box::new(ToggleLeftSidebar),
    },
    CommandEntry {
        name: "Source Control",
        action_id: Some("select_source_control_tab"),
        make_action: || Box::new(SelectSourceControlTab),
    },
    CommandEntry {
        name: "Search Pane",
        action_id: Some("search_scrollback"),
        make_action: || Box::new(Search),
    },
    CommandEntry {
        name: "Open Commit Dialog",
        action_id: Some("open_commit_dialog"),
        make_action: || Box::new(OpenCommitDialog),
    },
    CommandEntry {
        name: "Quick Open",
        action_id: Some("open_quick_open"),
        make_action: || Box::new(OpenQuickOpen),
    },
    CommandEntry {
        name: "Command Palette",
        action_id: Some("open_command_palette"),
        make_action: || Box::new(OpenCommandPalette),
    },
    CommandEntry {
        name: "Layout: Stacked",
        action_id: Some("layout_stacked"),
        make_action: || Box::new(ApplyLayoutStacked),
    },
    CommandEntry {
        name: "Layout: Horizontal",
        action_id: Some("layout_horizontal"),
        make_action: || Box::new(ApplyLayoutHorizontal),
    },
    CommandEntry {
        name: "Layout: Bottom Terminal",
        action_id: Some("layout_bottom_terminal"),
        make_action: || Box::new(ApplyLayoutBottomTerminal),
    },
    CommandEntry {
        name: "Reload Custom Commands",
        action_id: Some("reload_custom_commands"),
        make_action: || Box::new(ReloadCustomCommands),
    },
    CommandEntry {
        name: "Show Welcome Wizard",
        action_id: None,
        make_action: || Box::new(ShowWelcomeWizard),
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
            keybinding: c
                .action_id
                .and_then(crate::keymap_registry::display_chord_for),
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
    fn command_entries_reference_registry_action_ids() {
        for entry in PALETTE_COMMANDS {
            if let Some(id) = entry.action_id {
                assert!(
                    crate::keymap_registry::spec(id).is_some(),
                    "palette entry `{}` names unknown action id `{id}`",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn built_items_resolve_chords_from_registry() {
        let items = build_palette_items(&[]);
        let new_tab = items.iter().find(|i| i.name == "New Tab").unwrap();
        assert_eq!(new_tab.keybinding.as_deref(), Some("⌘T"));
        // Group splits ship unbound — no chip until the user binds them.
        let split = items
            .iter()
            .find(|i| i.name == "Split Pane Horizontally")
            .unwrap();
        assert_eq!(split.keybinding, None);
    }

    #[test]
    fn palette_commands_has_sixteen_entries() {
        // 14 original + "Reload Custom Commands" + "Show Welcome Wizard"
        assert_eq!(PALETTE_COMMANDS.len(), 16);
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
