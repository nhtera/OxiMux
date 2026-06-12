//! App-side loader/saver for `keybindings.toml` plus the boot install of
//! the effective keymap (defaults ⊕ overrides).
//!
//! Unlike `terminal.toml` there is no file watcher: the settings pane is
//! the editing surface and applies its own changes live; a hand-edited
//! file is picked up on the next launch. Load problems are stashed here
//! because they surface before any window (and therefore any toast layer)
//! exists — the first `WorkspaceRoot` drains them into toasts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use gpui::App;
use oximux_settings::KeybindingOverrides;

use crate::keymap_registry;

fn settings_path() -> Option<PathBuf> {
    crate::terminal_settings::app_data_dir().map(|d| d.join(KeybindingOverrides::FILE_NAME))
}

/// Read the override map from disk. Missing file = no overrides (the common
/// case). A parse error falls back to defaults with a warning — a broken
/// file must never cost the whole keymap.
pub fn load_overrides() -> (BTreeMap<String, String>, Vec<String>) {
    let Some(path) = settings_path() else {
        return (BTreeMap::new(), Vec::new());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return (BTreeMap::new(), Vec::new()),
    };
    match KeybindingOverrides::from_toml_str(&text) {
        Ok(parsed) => {
            let warnings = parsed
                .non_string_keys
                .iter()
                .map(|k| format!("keybindings.toml: `{k}` must be a string — line ignored"))
                .collect();
            (parsed.overrides, warnings)
        }
        Err(err) => {
            tracing::warn!(?path, %err, "keybindings.toml parse failed; using defaults");
            (
                BTreeMap::new(),
                vec!["keybindings.toml is not valid TOML — using default shortcuts".to_string()],
            )
        }
    }
}

/// Persist the override map. Only overrides are written — defaults live in
/// the registry inventory, so an empty map produces a header-only file.
pub fn save_overrides(overrides: &BTreeMap<String, String>) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for keybindings.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, KeybindingOverrides::to_toml_string(overrides))
}

/// Warnings produced before any window existed, awaiting a toast layer.
static BOOT_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Drain the boot-time load warnings (first caller wins; called by the
/// first `WorkspaceRoot` so the user sees a toast, not just a log line).
pub fn take_boot_warnings() -> Vec<String> {
    match BOOT_WARNINGS.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
    }
}

/// Boot wiring: load overrides, install the effective keymap, stash any
/// problems for the first window to toast. Must run before `set_menus`
/// (GPUI reads the keymap once when building menu-item glyphs).
pub fn install(cx: &mut App) {
    let (overrides, mut warnings) = load_overrides();
    warnings.extend(keymap_registry::install(cx, &overrides));
    for warning in &warnings {
        tracing::warn!(%warning, "keybinding override problem");
    }
    if !warnings.is_empty() {
        match BOOT_WARNINGS.lock() {
            Ok(mut g) => g.extend(warnings),
            Err(poisoned) => poisoned.into_inner().extend(warnings),
        }
    }
}
