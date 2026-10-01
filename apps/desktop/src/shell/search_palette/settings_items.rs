//! Static palette rows: one per offered settings pane, plus the action
//! catalog (the Command Palette's built-ins and a few palette-only entries).

use crate::actions::{OpenSessionHistory, OpenSettings};
use crate::shell::command_palette::entry::{PALETTE_COMMANDS, search_text_for};
use crate::shell::search_palette::model::{ActionItem, MakeAction, SettingItem};
use crate::shell::settings_modal::SettingsPane;

/// Words a settings pane answers to beyond its label ("keys" → Keybindings).
fn keywords(pane: SettingsPane) -> &'static str {
    match pane {
        SettingsPane::Git => "git branch worktree source control scm",
        SettingsPane::Terminal => "terminal shell font cursor scrollback pty",
        SettingsPane::Agents => "agents ai models launch claude codex default agent",
        SettingsPane::Voice => "voice dictation speech microphone transcribe",
        SettingsPane::ScreenControl => "computer use screen control driver",
        SettingsPane::Simulator => "mobile emulator simulator ios android device",
        SettingsPane::Notifications => "notifications alerts sounds bell",
        SettingsPane::Schedules => "schedules cron automations timers",
        SettingsPane::Remote => "remote mobile pairing phone",
        SettingsPane::Integrations => "integrations cli tools install sign in",
        SettingsPane::Keybindings => "keys keybindings shortcuts hotkeys chords",
        SettingsPane::Appearance => "appearance theme colors font density zoom dark light",
        SettingsPane::About => "about version update license",
    }
}

pub fn settings_items() -> Vec<SettingItem> {
    SettingsPane::offered()
        .into_iter()
        .map(|pane| SettingItem {
            pane,
            label: pane.label(),
            icon: pane.icon_path(),
            keywords: keywords(pane),
        })
        .collect()
}

/// Palette-only actions — useful from a search box but absent from the
/// Command Palette catalog. `(label, keymap id, synonyms, factory)`.
const EXTRA_ACTIONS: &[(&str, &str, &str, MakeAction)] = &[
    ("Open Settings", "open_settings", "preferences config", || Box::new(OpenSettings)),
    ("Session History", "open_session_history", "resume past sessions conversations", || {
        Box::new(OpenSessionHistory)
    }),
];

pub fn action_items() -> Vec<ActionItem> {
    let catalog = PALETTE_COMMANDS.iter().map(|c| ActionItem {
        label: c.name.to_string(),
        search_text: search_text_for(c.name),
        chord: c.action_id.and_then(crate::keymap_registry::display_chord_for),
        make: c.make_action,
    });
    let extras = EXTRA_ACTIONS.iter().map(|(label, id, synonyms, make)| ActionItem {
        label: label.to_string(),
        search_text: format!("{label} {synonyms}"),
        chord: crate::keymap_registry::display_chord_for(id),
        make: *make,
    });
    catalog.chain(extras).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_pane_has_a_row_with_keywords() {
        let items = settings_items();
        assert_eq!(items.len(), SettingsPane::offered().len());
        assert!(items.iter().all(|i| !i.keywords.is_empty() && !i.label.is_empty()));
    }

    #[test]
    fn extra_actions_name_real_registry_ids() {
        for (_, id, _, _) in EXTRA_ACTIONS {
            assert!(crate::keymap_registry::spec(id).is_some(), "unknown keymap id {id}");
        }
    }

    #[test]
    fn action_labels_are_unique() {
        let items = action_items();
        let mut labels: Vec<_> = items.iter().map(|a| a.label.as_str()).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len());
    }
}
