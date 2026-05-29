//! User-tunable terminal behavior, loaded from `terminal.toml` in the app
//! data dir and held as a GPUI [`Global`] so every pane reads one source of
//! truth. Defaults reproduce the previously-hardcoded constants exactly, so
//! an absent or empty file is a no-op visual diff.
//!
//! Live-reload: a file watcher (in the app crate) reparses on change and
//! swaps the global. Render/event-time knobs (alphas, blink, scroll speed,
//! OSC 52, option-as-meta, bell) take effect on the next frame because the
//! terminal repaints continuously; `scrollback_lines` is sourced at spawn and
//! so applies to newly-opened panes (resizing a live grid's history is left
//! to a future pass).

use gpui::Global;
use serde::{Deserialize, Serialize};

/// How a terminal bell (BEL / `\a`) is surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BellStyle {
    /// Ignore the bell entirely.
    Off,
    /// Raise the pane/tab attention signal (the current behavior).
    #[default]
    Visual,
}

/// All user-tunable terminal knobs. `#[serde(default)]` lets a partial TOML
/// override only the keys it sets; unknown keys are ignored for forward-compat.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Off-screen rows retained per pane. Sourced at spawn.
    pub scrollback_lines: usize,
    /// Multiplier applied to wheel-scroll line counts (1.0 = one line per
    /// notch). Values > 1 scroll faster; clamped to a sane floor at use.
    pub scroll_multiplier: f32,
    /// Whether the block cursor blinks when the pane is focused.
    pub cursor_blink: bool,
    /// Cursor blink half-period in milliseconds.
    pub blink_interval_ms: u64,
    /// Foreground alpha for SGR-2 (dim) cells.
    pub dim_alpha: f32,
    /// Foreground alpha applied to an unfocused pane's text.
    pub unfocused_alpha: f32,
    /// Alpha for the ghost cursor drawn on an unfocused pane.
    pub unfocused_cursor_alpha: f32,
    /// Allow terminal output to set the system clipboard (OSC 52). Security:
    /// remote/relay output can overwrite the clipboard when this is on.
    pub osc52_clipboard: bool,
    /// Treat the macOS Option key as Meta (ESC-prefix). When false, Option
    /// composes the platform character (e.g. `å`) instead of ESC-prefixing.
    pub option_as_meta: bool,
    /// How a bell is surfaced.
    pub bell: BellStyle,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        // These MUST match the constants they replace so a fresh install has
        // no visual/behavior diff: scrollback 5000, blink 530ms, dim 0.7,
        // unfocused 0.4 / cursor 0.3, grid 100x32.
        Self {
            scrollback_lines: 5000,
            scroll_multiplier: 1.0,
            cursor_blink: true,
            blink_interval_ms: 530,
            dim_alpha: 0.7,
            unfocused_alpha: 0.4,
            unfocused_cursor_alpha: 0.3,
            osc52_clipboard: true,
            option_as_meta: true,
            bell: BellStyle::Visual,
        }
    }
}

impl Global for TerminalSettings {}

impl TerminalSettings {
    /// File name within the app data dir.
    pub const FILE_NAME: &'static str = "terminal.toml";

    /// Parse a TOML document into settings; missing keys fall back to default.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a pretty TOML document (used to seed a default file so the
    /// user has every key in front of them to edit).
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Clamp out-of-range values to safe bounds. Called after load so a
    /// hand-edited file with `scroll_multiplier = 0` or a negative alpha can't
    /// break rendering or freeze scrolling.
    pub fn sanitized(mut self) -> Self {
        self.scrollback_lines = self.scrollback_lines.clamp(0, 1_000_000);
        self.scroll_multiplier = self.scroll_multiplier.clamp(0.1, 50.0);
        self.blink_interval_ms = self.blink_interval_ms.clamp(60, 10_000);
        self.dim_alpha = self.dim_alpha.clamp(0.0, 1.0);
        self.unfocused_alpha = self.unfocused_alpha.clamp(0.0, 1.0);
        self.unfocused_cursor_alpha = self.unfocused_cursor_alpha.clamp(0.0, 1.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let original = TerminalSettings::default();
        let toml = original.to_toml_string();
        let parsed = TerminalSettings::from_toml_str(&toml).expect("round-trip parse");
        assert_eq!(original, parsed);
    }

    #[test]
    fn partial_toml_overrides_only_named_keys() {
        // A sparse file should keep every other default intact.
        let toml = "scrollback_lines = 20000\nbell = \"off\"\n";
        let s = TerminalSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.scrollback_lines, 20_000);
        assert_eq!(s.bell, BellStyle::Off);
        // Untouched keys keep the defaults.
        assert_eq!(s.blink_interval_ms, 530);
        assert!(s.osc52_clipboard);
        assert_eq!(s.scroll_multiplier, 1.0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let toml = "scrollback_lines = 1234\nnonexistent_future_key = 7\n";
        let s = TerminalSettings::from_toml_str(toml).expect("unknown keys tolerated");
        assert_eq!(s.scrollback_lines, 1234);
    }

    #[test]
    fn sanitize_clamps_dangerous_values() {
        let raw = TerminalSettings {
            scroll_multiplier: 0.0,
            blink_interval_ms: 1,
            dim_alpha: 5.0,
            unfocused_alpha: -1.0,
            ..TerminalSettings::default()
        };
        let s = raw.sanitized();
        assert!(s.scroll_multiplier >= 0.1, "zero multiplier would freeze scroll");
        assert!(s.blink_interval_ms >= 60, "1ms blink would thrash");
        assert_eq!(s.dim_alpha, 1.0, "alpha clamped to <= 1");
        assert_eq!(s.unfocused_alpha, 0.0, "negative alpha clamped to >= 0");
    }

    #[test]
    fn defaults_match_legacy_constants() {
        // Pin the exact legacy values so a future edit can't silently change
        // out-of-box behavior.
        let s = TerminalSettings::default();
        assert_eq!(s.scrollback_lines, 5000);
        assert_eq!(s.blink_interval_ms, 530);
        assert_eq!(s.dim_alpha, 0.7);
        assert_eq!(s.unfocused_alpha, 0.4);
        assert_eq!(s.unfocused_cursor_alpha, 0.3);
    }
}
