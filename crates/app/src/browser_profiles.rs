//! Browser profiles: named, cookie/cache-isolated webview stores.
//!
//! Each profile is a stable UUID whose bytes select a macOS
//! `WKWebsiteDataStore` (see [`crate::shell::browser_view`]); distinct
//! profiles never see each other's cookies, logins, or cache. The list lives
//! in a small JSON file in the app data dir and is mirrored into a GPUI global
//! so the browser toolbar can enumerate + switch profiles. The implicit
//! "Default" profile is `None` (the shared default store) and is not stored.

use std::path::PathBuf;

use gpui::App;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Mirror of the other settings loaders' per-user data subdir.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const FILE_NAME: &str = "browser_profiles.json";

/// A named, isolated browser profile. `id`'s bytes are the data-store
/// identifier handed to the webview.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserProfile {
    pub id: Uuid,
    pub name: String,
}

/// The persisted profile list, installed as a GPUI global.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BrowserProfiles {
    pub profiles: Vec<BrowserProfile>,
}

impl gpui::Global for BrowserProfiles {}

impl BrowserProfiles {
    /// Display name for a profile id; the `None`/unknown id is "Default".
    pub fn name_of(&self, id: Option<Uuid>) -> String {
        match id {
            None => "Default".to_string(),
            Some(id) => self
                .profiles
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Default".to_string()),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR).join(FILE_NAME))
}

fn load() -> BrowserProfiles {
    let Some(path) = settings_path() else {
        return BrowserProfiles::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
            tracing::warn!(?path, %err, "browser_profiles.json parse failed; using empty list");
            BrowserProfiles::default()
        }),
        // Absent file is the common case (no profiles created yet).
        Err(_) => BrowserProfiles::default(),
    }
}

/// Persist the profile list. Best-effort: failures are logged, not fatal.
pub fn save(profiles: &BrowserProfiles) {
    let Some(path) = settings_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(profiles) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                tracing::warn!(?path, %err, "could not save browser_profiles.json");
            }
        }
        Err(err) => tracing::warn!(%err, "could not serialize browser profiles"),
    }
}

/// Load the profile list and install it as a global. Call once from the app's
/// `run` closure alongside the other settings installers.
pub fn install(cx: &mut App) {
    cx.set_global(load());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_of_resolves_default_and_known() {
        let id = Uuid::new_v4();
        let profiles = BrowserProfiles {
            profiles: vec![BrowserProfile { id, name: "Work".into() }],
        };
        assert_eq!(profiles.name_of(None), "Default");
        assert_eq!(profiles.name_of(Some(id)), "Work");
        // An id not in the list falls back to "Default" (e.g. a deleted
        // profile still referenced by a restored tab).
        assert_eq!(profiles.name_of(Some(Uuid::new_v4())), "Default");
    }

    #[test]
    fn round_trips_through_json() {
        let profiles = BrowserProfiles {
            profiles: vec![BrowserProfile { id: Uuid::new_v4(), name: "P1".into() }],
        };
        let json = serde_json::to_string(&profiles).expect("serialize");
        let back: BrowserProfiles = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.profiles, profiles.profiles);
    }
}
