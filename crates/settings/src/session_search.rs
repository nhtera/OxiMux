//! Session search settings, loaded from `session_search.toml` in the app data
//! dir and held as a GPUI [`Global`].
//!
//! One switch: whether OxiMux keeps a local full-text index of past agent
//! conversations. Off by default — the index reads every transcript on disk
//! and occupies hundreds of MB on a busy machine, so it is the user's call.

#[cfg(feature = "gpui")]
use gpui::Global;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSearchSettings {
    /// Keep the index current and search it from Session History.
    pub enabled: bool,
}

impl SessionSearchSettings {
    pub const FILE_NAME: &'static str = "session_search.toml";

    /// What a fresh install runs with.
    pub fn shipped() -> Self {
        Self::default()
    }

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("session search settings serialize")
    }

    /// The settings in `dir`, or the shipped defaults when the file is absent
    /// or unreadable.
    pub fn load_from_dir(dir: &std::path::Path) -> Self {
        std::fs::read_to_string(dir.join(Self::FILE_NAME))
            .ok()
            .and_then(|t| Self::from_toml_str(&t).ok())
            .unwrap_or_else(Self::shipped)
    }
}

#[cfg(feature = "gpui")]
impl Global for SessionSearchSettings {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        assert!(!SessionSearchSettings::shipped().enabled);
        assert!(!SessionSearchSettings::from_toml_str("").unwrap().enabled);
    }

    #[test]
    fn round_trips_through_toml() {
        let s = SessionSearchSettings { enabled: true };
        assert_eq!(SessionSearchSettings::from_toml_str(&s.to_toml_string()).unwrap(), s);
    }

    #[test]
    fn missing_or_broken_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(SessionSearchSettings::load_from_dir(dir.path()), SessionSearchSettings::shipped());
        std::fs::write(dir.path().join(SessionSearchSettings::FILE_NAME), "enabled = \"maybe\"").unwrap();
        assert_eq!(SessionSearchSettings::load_from_dir(dir.path()), SessionSearchSettings::shipped());
        std::fs::write(dir.path().join(SessionSearchSettings::FILE_NAME), "enabled = true").unwrap();
        assert!(SessionSearchSettings::load_from_dir(dir.path()).enabled);
    }
}
