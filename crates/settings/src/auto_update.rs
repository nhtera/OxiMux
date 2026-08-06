//! Auto-update settings, loaded from `auto_update.toml` in the app data dir
//! and held as a GPUI [`Global`].
//!
//! Flat scalars, all with serde defaults, so a settings file written by an
//! older build (or no file at all) loads cleanly.
//!
//! `last_run_version` is bookkeeping rather than preference: the app compares
//! it against its own version at boot to decide whether to show a one-shot
//! "what's new" notice, then rewrites it. Empty means first run ever — which
//! shows nothing, because there is no upgrade to announce.

#[cfg(feature = "gpui")]
use gpui::Global;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoUpdateSettings {
    /// Whether the periodic check runs at all. Manual "check now" works
    /// regardless — turning this off means "stop asking on your own", not
    /// "never update".
    pub enabled: bool,
    /// Version of the last build that ran, for the what's-new notice.
    pub last_run_version: String,
}

impl Default for AutoUpdateSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            last_run_version: String::new(),
        }
    }
}

impl AutoUpdateSettings {
    pub const FILE_NAME: &'static str = "auto_update.toml";

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("auto-update settings serialize")
    }

    /// Whether finishing boot on `current` should announce an upgrade.
    ///
    /// False on a first run (nothing to compare against) and false when the
    /// version is unchanged, so the notice fires exactly once per update.
    pub fn should_announce(&self, current: &str) -> bool {
        !self.last_run_version.is_empty() && self.last_run_version != current
    }
}

#[cfg(feature = "gpui")]
impl Global for AutoUpdateSettings {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_enabled_with_no_recorded_version() {
        let s = AutoUpdateSettings::default();
        assert!(s.enabled);
        assert!(s.last_run_version.is_empty());
    }

    #[test]
    fn a_first_ever_run_announces_nothing() {
        assert!(!AutoUpdateSettings::default().should_announce("0.1.3"));
    }

    #[test]
    fn announces_once_per_version_change() {
        let s = AutoUpdateSettings {
            enabled: true,
            last_run_version: "0.1.3".into(),
        };
        assert!(s.should_announce("0.2.0"));
        assert!(!s.should_announce("0.1.3"));
    }

    #[test]
    fn round_trips_through_toml() {
        let s = AutoUpdateSettings {
            enabled: false,
            last_run_version: "0.2.0".into(),
        };
        let parsed = AutoUpdateSettings::from_toml_str(&s.to_toml_string()).expect("parses");
        assert_eq!(parsed, s);
    }

    #[test]
    fn a_file_from_an_older_build_loads_with_defaults() {
        // Only one key present: the rest must default rather than fail.
        let parsed = AutoUpdateSettings::from_toml_str("enabled = false").expect("parses");
        assert!(!parsed.enabled);
        assert!(parsed.last_run_version.is_empty());
    }
}
