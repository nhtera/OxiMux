//! How long the app may hold a rate-limited turn before giving it back.
//!
//! Retrying a closed usage window means waiting for the provider's reset, which
//! can be hours away for a five-hour window and days for a weekly one. Waiting
//! is usually what the user wants — the alternative is coming back to a failed
//! turn and pressing send by hand. Waiting *silently for days* is not: a queued
//! turn and a forgotten one look identical, and a turn that fires two days late
//! lands in a context the user has moved on from.
//!
//! So the wait is capped. A reset farther out than the cap is not scheduled at
//! all; the failure surfaces as an ordinary error the user can act on.

use std::time::Duration;

#[cfg(feature = "gpui")]
use gpui::Global;

/// The choices offered, in the order the settings pane lists them.
///
/// A closed enum rather than a free-form duration: the meaningful answers are
/// "shorter than a five-hour window", "long enough for one", and "however long
/// it takes", and a spinner inviting `7h13m` would only add ways to be wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaxAutomaticWait {
    /// Long enough for a five-hour window to reopen, not for a weekly one.
    SixHours,
    /// The default: covers every five-hour window and a same-day weekly reset.
    #[default]
    OneDay,
    /// Wait however long the provider says, including a weekly reset days out.
    NoLimit,
}

impl MaxAutomaticWait {
    /// Every variant, for rendering a picker.
    pub const ALL: &'static [Self] = &[Self::SixHours, Self::OneDay, Self::NoLimit];

    /// The cap as a duration; `None` for [`Self::NoLimit`].
    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::SixHours => Some(Duration::from_secs(6 * 60 * 60)),
            Self::OneDay => Some(Duration::from_secs(24 * 60 * 60)),
            Self::NoLimit => None,
        }
    }

    /// Label for the picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::SixHours => "Up to 6 hours",
            Self::OneDay => "Up to 24 hours",
            Self::NoLimit => "No limit",
        }
    }

    /// Stable token for persistence. Parsed by [`Self::from_token`], which
    /// falls back to the default rather than failing — an unreadable setting
    /// must not stop the app from starting.
    pub fn token(self) -> &'static str {
        match self {
            Self::SixHours => "six-hours",
            Self::OneDay => "one-day",
            Self::NoLimit => "no-limit",
        }
    }

    /// Inverse of [`Self::token`]; anything unrecognised is the default.
    pub fn from_token(token: &str) -> Self {
        Self::ALL.iter().copied().find(|v| v.token() == token).unwrap_or_default()
    }
}

/// Retry behaviour, as configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentRetrySettings {
    /// When false, a failed turn is never rescheduled — it surfaces as an error
    /// immediately, which is the pre-retry behaviour.
    pub enabled: bool,
    pub max_automatic_wait: MaxAutomaticWait,
}

impl Default for AgentRetrySettings {
    fn default() -> Self {
        Self::shipped()
    }
}

impl AgentRetrySettings {
    /// File this is persisted to, beside the other per-app settings.
    pub const FILE_NAME: &'static str = "agent_retry.toml";

    /// The shipped default: on, capped at a day.
    pub fn shipped() -> Self {
        Self { enabled: true, max_automatic_wait: MaxAutomaticWait::OneDay }
    }

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("agent retry settings serialize")
    }
}

#[cfg(feature = "gpui")]
impl Global for AgentRetrySettings {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_is_on_and_capped_at_a_day() {
        let s = AgentRetrySettings::shipped();
        assert!(s.enabled);
        assert_eq!(s.max_automatic_wait.duration(), Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn tokens_round_trip_and_unknown_falls_back() {
        for v in MaxAutomaticWait::ALL {
            assert_eq!(MaxAutomaticWait::from_token(v.token()), *v);
        }
        assert_eq!(MaxAutomaticWait::from_token("forever-and-ever"), MaxAutomaticWait::OneDay);
        assert_eq!(MaxAutomaticWait::from_token(""), MaxAutomaticWait::OneDay);
    }

    #[test]
    fn settings_round_trip_through_toml() {
        for wait in MaxAutomaticWait::ALL {
            let s = AgentRetrySettings { enabled: false, max_automatic_wait: *wait };
            assert_eq!(AgentRetrySettings::from_toml_str(&s.to_toml_string()).expect("parses"), s);
        }
    }

    /// A file written by an older build carries fewer keys; the rest must
    /// default rather than fail the load and silently disable retries.
    #[test]
    fn a_partial_file_loads_with_defaults() {
        let parsed = AgentRetrySettings::from_toml_str("enabled = false").expect("parses");
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_automatic_wait, MaxAutomaticWait::OneDay);
    }

    /// The on-disk key names are user-facing — `docs/agent-retry.md` prints this
    /// exact file. A rename here silently invalidates that example.
    #[test]
    fn the_documented_toml_keys_are_the_real_ones() {
        let text = AgentRetrySettings::shipped().to_toml_string();
        assert!(text.contains("enabled = true"), "got: {text}");
        assert!(text.contains(r#"max_automatic_wait = "one-day""#), "got: {text}");
    }

    #[test]
    fn no_limit_has_no_duration() {
        assert_eq!(MaxAutomaticWait::NoLimit.duration(), None);
    }
}
