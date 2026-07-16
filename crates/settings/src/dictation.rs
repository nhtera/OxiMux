//! Voice-dictation settings, loaded from `dictation.toml` in the app data dir
//! and held as a GPUI [`Global`] so the composer mic button and the Voice
//! settings pane read one source of truth.
//!
//! Mirrors the `agent_launch` settings contract: `from_toml_str` /
//! `to_toml_string` / `sanitized`, a `FILE_NAME`, and a live-reload watcher in
//! the app crate. Defaults are deliberately usable out of the box: dictation
//! enabled, the Vietnamese-capable `whisper-small` model, language auto-detect.
//! The model still has to be downloaded before the mic works — the setting only
//! records *which* model, not that it is present on disk.

use gpui::Global;
use serde::{Deserialize, Serialize};

/// The default model id. Kept in sync with the dictation crate's catalog
/// `DEFAULT_MODEL_ID` (the settings crate must not depend on the engine crate,
/// so the string is duplicated — a drift here only means a bad default that the
/// use-site clamps back when the id isn't in the live catalog).
pub const DEFAULT_MODEL_ID: &str = "whisper-small";

/// Accepted whisper language selectors. `auto` = detect; `vi`/`en` pin it.
pub const LANGUAGES: &[&str] = &["auto", "vi", "en"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DictationSettings {
    /// Master switch. `false` hides the mic button and turns the Cmd+E
    /// keybinding into a "dictation disabled" toast.
    pub enabled: bool,
    /// Catalog id of the model used for transcription.
    pub model_id: String,
    /// Whisper language: `"auto"` | `"vi"` | `"en"` (ignored by transducer
    /// models, which are English/European only).
    pub language: String,
}

impl Default for DictationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model_id: DEFAULT_MODEL_ID.to_string(),
            language: "auto".to_string(),
        }
    }
}

impl Global for DictationSettings {}

impl DictationSettings {
    pub const FILE_NAME: &'static str = "dictation.toml";

    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// The whisper `language` param: `None` for auto-detect, else the code.
    pub fn language_param(&self) -> Option<String> {
        match self.language.as_str() {
            "" | "auto" => None,
            other => Some(other.to_string()),
        }
    }

    /// Trim + normalize hand-edited values: blank model → default, unknown
    /// language → `auto`. Model id is only trimmed here (the live catalog at the
    /// use-site is the authority on whether an id resolves).
    pub fn sanitized(mut self) -> Self {
        self.model_id = self.model_id.trim().to_string();
        if self.model_id.is_empty() {
            self.model_id = DEFAULT_MODEL_ID.to_string();
        }
        self.language = self.language.trim().to_lowercase();
        if !LANGUAGES.contains(&self.language.as_str()) {
            self.language = "auto".to_string();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_whisper_small_auto() {
        let d = DictationSettings::default();
        assert!(d.enabled);
        assert_eq!(d.model_id, "whisper-small");
        assert_eq!(d.language, "auto");
        assert_eq!(d.language_param(), None);
    }

    #[test]
    fn round_trips_through_toml() {
        let s = DictationSettings {
            enabled: false,
            model_id: "whisper-tiny".into(),
            language: "vi".into(),
        };
        let parsed = DictationSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed, s);
        assert_eq!(parsed.language_param(), Some("vi".into()));
    }

    #[test]
    fn missing_keys_take_defaults() {
        let s = DictationSettings::from_toml_str("").expect("empty parses");
        assert!(s.enabled);
        assert_eq!(s.model_id, "whisper-small");
    }

    #[test]
    fn sanitize_clamps_blank_model_and_unknown_language() {
        let s = DictationSettings {
            enabled: true,
            model_id: "   ".into(),
            language: "FR".into(),
        }
        .sanitized();
        assert_eq!(s.model_id, "whisper-small", "blank model → default");
        assert_eq!(s.language, "auto", "unknown language → auto");
    }

    #[test]
    fn sanitize_lowercases_known_language() {
        let s = DictationSettings {
            language: "VI".into(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.language, "vi");
    }
}
