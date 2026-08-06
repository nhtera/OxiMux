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

#[cfg(feature = "gpui")]
use gpui::Global;
use serde::{Deserialize, Serialize};

/// The default model id. Kept in sync with the dictation crate's catalog
/// `DEFAULT_MODEL_ID` (the settings crate must not depend on the engine crate,
/// so the string is duplicated — a drift here only means a bad default that the
/// use-site clamps back when the id isn't in the live catalog).
pub const DEFAULT_MODEL_ID: &str = "whisper-small";

use crate::dictation_languages;

/// How a dictation press behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DictationMode {
    /// Press ⌘E / click the mic once to start, again to stop.
    #[default]
    Toggle,
    /// Dictate only while ⌘E / the mic is held; release to stop-and-insert.
    Hold,
}

impl DictationMode {
    pub fn is_hold(self) -> bool {
        matches!(self, DictationMode::Hold)
    }
}

/// How long a warm recognizer lingers after a dictation before being dropped to
/// release its ONNX memory. Trades idle RAM against the reload cost of the next
/// press (seconds for the larger models).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModelUnloadTimeout {
    /// Keep the engine warm forever — fastest, holds the most memory.
    #[serde(rename = "never")]
    Never,
    /// Drop the engine as soon as a dictation finishes decoding.
    #[serde(rename = "immediately")]
    Immediately,
    #[serde(rename = "2m")]
    Min2,
    #[serde(rename = "5m")]
    Min5,
    /// The historical hardcoded behaviour, kept as the default.
    #[default]
    #[serde(rename = "10m")]
    Min10,
    #[serde(rename = "15m")]
    Min15,
    #[serde(rename = "1h")]
    Hour1,
}

impl ModelUnloadTimeout {
    /// The idle window before teardown. `None` means "never unload"; a zero
    /// duration means "unload immediately after each decode". This is the shape
    /// the dictation controller consumes (it cannot depend on this crate).
    pub fn as_duration(self) -> Option<std::time::Duration> {
        use std::time::Duration;
        match self {
            ModelUnloadTimeout::Never => None,
            ModelUnloadTimeout::Immediately => Some(Duration::ZERO),
            ModelUnloadTimeout::Min2 => Some(Duration::from_secs(2 * 60)),
            ModelUnloadTimeout::Min5 => Some(Duration::from_secs(5 * 60)),
            ModelUnloadTimeout::Min10 => Some(Duration::from_secs(10 * 60)),
            ModelUnloadTimeout::Min15 => Some(Duration::from_secs(15 * 60)),
            ModelUnloadTimeout::Hour1 => Some(Duration::from_secs(60 * 60)),
        }
    }

    /// Voice-pane label.
    pub fn label(self) -> &'static str {
        match self {
            ModelUnloadTimeout::Never => "Never",
            ModelUnloadTimeout::Immediately => "Immediately",
            ModelUnloadTimeout::Min2 => "2 minutes",
            ModelUnloadTimeout::Min5 => "5 minutes",
            ModelUnloadTimeout::Min10 => "10 minutes",
            ModelUnloadTimeout::Min15 => "15 minutes",
            ModelUnloadTimeout::Hour1 => "1 hour",
        }
    }

    /// All variants in display order.
    pub const ALL: &'static [ModelUnloadTimeout] = &[
        ModelUnloadTimeout::Never,
        ModelUnloadTimeout::Hour1,
        ModelUnloadTimeout::Min15,
        ModelUnloadTimeout::Min10,
        ModelUnloadTimeout::Min5,
        ModelUnloadTimeout::Min2,
        ModelUnloadTimeout::Immediately,
    ];
}

// NOTE: no `auto_submit_key` setting. The composer has exactly one send path
// (`ComposerView::submit`) — there is no distinct Cmd+Enter send to choose
// between, so a key picker here would be a knob that changes nothing.

// No `Eq`: `word_correction_threshold` is an f64. `PartialEq` suffices for the
// change-detection the settings watcher does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// cpal input-device name to capture from. `None`/empty = system default.
    /// A stale name (device unplugged) falls back to default at the use-site.
    pub input_device: Option<String>,
    /// Toggle vs. press-and-hold dictation.
    pub mode: DictationMode,
    /// Trim silence with Silero VAD before decoding. Keeps only speech spans so
    /// whisper never hallucinates captions over pauses. Defaults on; the ~629 KB
    /// VAD model downloads once on first use, and a missing model degrades to the
    /// plain silence gate.
    pub vad_enabled: bool,
    /// Dictionary of proper nouns / brand / command names the transcript is
    /// fuzzy-corrected toward (e.g. "OxiMux", "ChargeBee"). Empty = no correction.
    pub custom_words: Vec<String>,
    /// Acceptance threshold for custom-word correction (lower = stricter). `0`
    /// disables it even with a non-empty dictionary.
    pub word_correction_threshold: f64,
    /// Clean transcripts of filler words, stutters, and whole-output whisper
    /// hallucinations ("(sad music)"). Language-gated fillers; defaults on.
    pub filler_filter_enabled: bool,
    /// How long the warm recognizer survives idle before it is torn down.
    pub model_unload_timeout: ModelUnloadTimeout,
    /// Append a single space after an inserted transcript so back-to-back
    /// dictations don't run together. Off by default (the space is unwanted when
    /// dictating a single value into a field).
    pub append_trailing_space: bool,
    /// Send the composer automatically once a dictated transcript lands in it.
    /// Off by default — a mis-fire would send a half-written message.
    pub auto_submit: bool,
    /// Play a short system sound when recording starts and stops.
    pub audio_feedback_enabled: bool,
}

impl Default for DictationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model_id: DEFAULT_MODEL_ID.to_string(),
            language: "auto".to_string(),
            input_device: None,
            mode: DictationMode::Toggle,
            vad_enabled: true,
            custom_words: Vec::new(),
            word_correction_threshold: DEFAULT_WORD_CORRECTION_THRESHOLD,
            filler_filter_enabled: true,
            model_unload_timeout: ModelUnloadTimeout::Min10,
            append_trailing_space: false,
            auto_submit: false,
            audio_feedback_enabled: false,
        }
    }
}

/// Default fuzzy custom-word acceptance threshold. Kept in sync with the
/// dictation crate's `custom_words::DEFAULT_THRESHOLD` (the settings crate must
/// not depend on the engine crate, so the constant is duplicated).
pub const DEFAULT_WORD_CORRECTION_THRESHOLD: f64 = 0.18;

#[cfg(feature = "gpui")]
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

    /// The capture device name, or `None` for the system default. An empty
    /// string is treated as "default" so a hand-edited `input_device = ""` in
    /// the TOML doesn't try to open a device named "".
    pub fn device_name(&self) -> Option<String> {
        self.input_device
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
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
        if !dictation_languages::is_supported(&self.language) {
            self.language = "auto".to_string();
        }
        // Collapse a blank device name to "system default".
        self.input_device = self.device_name();
        // Trim custom words, drop blanks and duplicates (case-insensitive).
        let mut seen = std::collections::HashSet::new();
        self.custom_words = std::mem::take(&mut self.custom_words)
            .into_iter()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty() && seen.insert(w.to_lowercase()))
            .collect();
        // Clamp the threshold to a sane range; a NaN/negative reads as "off",
        // an over-1 value would accept anything.
        if !self.word_correction_threshold.is_finite() || self.word_correction_threshold < 0.0 {
            self.word_correction_threshold = 0.0;
        } else if self.word_correction_threshold > 1.0 {
            self.word_correction_threshold = 1.0;
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
            input_device: Some("MacBook Pro Microphone".into()),
            mode: DictationMode::Hold,
            vad_enabled: false,
            custom_words: vec!["OxiMux".into(), "ChargeBee".into()],
            word_correction_threshold: 0.2,
            filler_filter_enabled: false,
            model_unload_timeout: ModelUnloadTimeout::Immediately,
            append_trailing_space: true,
            auto_submit: true,
            audio_feedback_enabled: true,
        };
        let parsed = DictationSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed, s);
        assert_eq!(parsed.language_param(), Some("vi".into()));
        assert_eq!(parsed.device_name(), Some("MacBook Pro Microphone".into()));
        assert!(parsed.mode.is_hold());
    }

    #[test]
    fn missing_new_keys_default_to_system_default_toggle() {
        // A pre-existing dictation.toml (before these fields existed) must load.
        let legacy = "enabled = true\nmodel_id = \"whisper-small\"\nlanguage = \"auto\"\n";
        let s = DictationSettings::from_toml_str(legacy).expect("legacy parses");
        assert_eq!(s.input_device, None);
        assert_eq!(s.mode, DictationMode::Toggle);
        assert_eq!(s.device_name(), None);
        // A pre-VAD dictation.toml must default the new toggle on.
        assert!(s.vad_enabled, "vad defaults on for legacy stores");
        // The phase-05 keys must load absent, preserving prior behaviour: the
        // historical 10-minute teardown and no space/submit/sound side effects.
        assert_eq!(s.model_unload_timeout, ModelUnloadTimeout::Min10);
        assert!(!s.append_trailing_space);
        assert!(!s.auto_submit);
        assert!(!s.audio_feedback_enabled);
    }

    #[test]
    fn unload_timeout_maps_to_durations() {
        use std::time::Duration;
        // `None` = never unload; ZERO = unload right after each decode. The
        // controller keys off exactly these two sentinels.
        assert_eq!(ModelUnloadTimeout::Never.as_duration(), None);
        assert_eq!(
            ModelUnloadTimeout::Immediately.as_duration(),
            Some(Duration::ZERO)
        );
        assert_eq!(
            ModelUnloadTimeout::Min10.as_duration(),
            Some(Duration::from_secs(600)),
            "default must match the historical hardcoded teardown"
        );
        assert_eq!(
            ModelUnloadTimeout::Hour1.as_duration(),
            Some(Duration::from_secs(3600))
        );
        // Every variant is offered in the picker.
        assert_eq!(ModelUnloadTimeout::ALL.len(), 7);
    }

    #[test]
    fn unload_timeout_serializes_stably() {
        let s = DictationSettings {
            model_unload_timeout: ModelUnloadTimeout::Hour1,
            ..Default::default()
        };
        let toml = s.to_toml_string();
        assert!(toml.contains("model_unload_timeout = \"1h\""), "{toml}");
    }

    #[test]
    fn blank_device_name_is_system_default() {
        let s = DictationSettings {
            input_device: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(s.device_name(), None, "blank → default at read time");
        assert_eq!(s.sanitized().input_device, None, "sanitize collapses blank");
    }

    #[test]
    fn dictation_mode_serializes_lowercase() {
        let s = DictationSettings {
            mode: DictationMode::Hold,
            ..Default::default()
        };
        assert!(s.to_toml_string().contains("mode = \"hold\""));
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
            // "zz" is not a whisper language code — must clamp to auto. (A real
            // code like "fr" now stays, since the full set is accepted.)
            language: "ZZ".into(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.model_id, "whisper-small", "blank model → default");
        assert_eq!(s.language, "auto", "unknown language → auto");
    }

    #[test]
    fn sanitize_keeps_a_broad_whisper_language() {
        // Regression guard for the widened language set: a non-vi/en code the
        // legacy 3-item list would have clamped must now survive.
        let s = DictationSettings {
            language: "TH".into(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(s.language, "th", "thai is a valid whisper language");
        assert_eq!(s.language_param(), Some("th".into()));
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
