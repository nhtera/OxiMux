//! iOS Simulator panel settings, loaded from `simulator.toml` in the app data
//! dir and held as a GPUI [`Global`] so the panel, the stream row, and the
//! agent-control gate all read one source of truth.
//!
//! Mirrors the `computer_use`/`terminal` settings contract: `from_toml_str` /
//! `to_toml_string` / `sanitized`, a `FILE_NAME`, and a debounced-FSEvents
//! live-reload watcher, installed once from the app's `run` closure.
//!
//! # Fails closed
//!
//! [`SimulatorSettings::agent_control`] gates every `oximux sim …` control verb
//! (P8). A **missing** file is the ordinary fresh-install case and loads as
//! [`SimulatorSettings::default`], which has agent control on — the same as
//! every other default-on convenience toggle. A file that exists but is
//! **unreadable or fails to parse** is different: it must not be read as "on"
//! just because the bytes were corrupted mid-write or truncated by a crash, so
//! that path falls back to [`SimulatorSettings::fail_closed`], which is the
//! default with `agent_control` forced to `false`. This is the one direction
//! the corrupt-file fallback may go, the same rule `ComputerUseSettings`
//! documents for its own master switch.
//!
//! Approvals for *which device* an agent may drive are not stored here at all
//! — per P8 they live in SQLite, keyed per device, because this file sits in
//! the user's home directory where an agent's own shell tool can write it.
//! `agent_control` is only the global kill switch.

use std::path::PathBuf;

use gpui::{App, Global};
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const DEBOUNCE_MS: u64 = 250;

/// Stream resolution, as a fraction of the device's native pixel size.
/// `Half` is the default (P5 mockup), chosen for encode/decode cost, not
/// fidelity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    #[default]
    Half,
    Full,
}

impl Resolution {
    /// The multiplier to apply to the device's native frame size.
    pub fn scale(self) -> f32 {
        match self {
            Resolution::Half => 0.5,
            Resolution::Full => 1.0,
        }
    }
}

/// The frame rates the stream row offers. `clamp_fps` snaps any other value
/// (a hand-edited TOML, or a future build's now-removed option) to the
/// nearest of these rather than silently falling back to the default,
/// so "close to what was asked for" survives a round-trip better than
/// "whatever 30 happens to mean this release".
pub const ALLOWED_FPS: [u32; 3] = [15, 30, 60];

fn clamp_fps(fps: u32) -> u32 {
    ALLOWED_FPS
        .iter()
        .copied()
        .min_by_key(|allowed| allowed.abs_diff(fps))
        .unwrap_or(30)
}

/// Stream-row settings (`stream_row.rs`, P5): frame rate, resolution, and the
/// optional FPS readout. Persisted only inside [`SimulatorSettings`] — this is
/// the single store, not a per-field one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    /// 15, 30, or 60. Anything else read from disk is snapped to the nearest
    /// of those by [`SimulatorSettings::sanitized`].
    pub fps: u32,
    pub resolution: Resolution,
    /// Show the live FPS readout in the toolbar.
    pub show_fps: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            fps: 30,
            resolution: Resolution::Half,
            show_fps: false,
        }
    }
}

impl StreamSettings {
    /// The multiplier to apply to the device's native frame size — `0.5` for
    /// `Half`, `1.0` for `Full`. Named for the call site (`agent_ops.rs`'s
    /// pixel↔normalized conversion, the bezel's stream surface) rather than
    /// exposing the enum's `scale()` directly, so a future third resolution
    /// tier is one match arm, not a signature change at every call site.
    pub fn effective_scale(&self) -> f32 {
        self.resolution.scale()
    }
}

/// iOS Simulator panel settings (P5 header/stream row + P8 kill switch),
/// persisted to `simulator.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimulatorSettings {
    /// Master switch. `false` hides the `RightTab::Simulator` tab entirely,
    /// on top of the macOS-arm64 platform gate.
    pub enabled: bool,
    /// Auto-open the panel when an agent's first `sim` verb needs consent.
    pub auto_open: bool,
    /// Global kill switch for every `oximux sim` control verb. See the module
    /// doc for the fail-closed contract on a corrupt file.
    ///
    /// A file that exists but lacks this key reads it as **off**: a write
    /// cut short, or a hand-trimmed file, must never grant control. Only a
    /// missing file (a fresh install) gets the on-by-default value.
    #[serde(default)]
    pub agent_control: bool,
    /// UDID of the device to auto-pick on attach when none is remembered per
    /// worktree yet. `None` means "let the picker decide" (first booted, else
    /// first available).
    pub default_device: Option<String>,
    /// Idle minutes before an OxiMux-booted (not user-booted) device is shut
    /// back down.
    pub idle_shutdown_minutes: u32,
    pub stream: StreamSettings,
}

impl Default for SimulatorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_open: true,
            agent_control: true,
            default_device: None,
            idle_shutdown_minutes: 10,
            stream: StreamSettings::default(),
        }
    }
}

impl Global for SimulatorSettings {}

impl SimulatorSettings {
    pub const FILE_NAME: &'static str = "simulator.toml";

    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// The default settings, but with [`agent_control`](Self::agent_control)
    /// forced off. Used only for a `simulator.toml` that exists but could not
    /// be read or parsed — never for a missing file, which is the ordinary
    /// fresh-install default instead. See the module doc.
    pub fn fail_closed() -> Self {
        Self {
            agent_control: false,
            ..Self::default()
        }
    }

    /// Trim + normalize hand-edited values: snap an out-of-set fps to the
    /// nearest allowed value and drop a blank device id.
    pub fn sanitized(mut self) -> Self {
        self.stream.fps = clamp_fps(self.stream.fps);
        if self
            .default_device
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            self.default_device = None;
        } else if let Some(d) = &self.default_device {
            let trimmed = d.trim();
            if trimmed != d {
                self.default_device = Some(trimmed.to_string());
            }
        }
        self
    }
}

fn data_dir() -> Option<PathBuf> {
    crate::app_paths::data_dir()
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(SimulatorSettings::FILE_NAME))
}

/// Read + sanitize settings from disk. A missing file loads as the ordinary
/// default (agent control on); a file that exists but is unreadable or fails
/// to parse fails closed. See the module doc.
fn load() -> SimulatorSettings {
    let Some(path) = settings_path() else {
        return SimulatorSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match SimulatorSettings::from_toml_str(&text) {
            Ok(parsed) => parsed.sanitized(),
            Err(err) => {
                tracing::warn!(?path, %err, "simulator.toml parse failed; agent control stays off");
                SimulatorSettings::fail_closed()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SimulatorSettings::default(),
        Err(err) => {
            tracing::warn!(?path, %err, "simulator.toml unreadable; agent control stays off");
            SimulatorSettings::fail_closed()
        }
    }
}

/// Persist `settings` to `simulator.toml`, atomically (temp file + rename), so
/// a crash mid-write can never leave a truncated file for the fail-closed
/// loader to misread. The live-reload watcher reparses and installs the same
/// value; a caller may also `set_global` it at once for an immediate UI.
pub fn save(settings: &SimulatorSettings) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for simulator.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, settings.to_toml_string())?;
    std::fs::rename(&tmp, &path)
}

fn apply(cx: &mut App, settings: SimulatorSettings) {
    cx.set_global(settings);
}

/// True when a debounced batch touched `simulator.toml`.
fn batch_touches_settings(result: &DebounceEventResult) -> bool {
    let Ok(events) = result else { return false };
    events.iter().any(|ev| {
        ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == SimulatorSettings::FILE_NAME)
        })
    })
}

/// Load settings, install the global, and start the live-reload watcher.
/// Call once from the app's `run` closure, before any window opens, so the
/// panel and the agent-control gate both read a real value from the start.
pub fn install(cx: &mut App) {
    apply(cx, load());

    let Some(dir) = data_dir() else { return };
    let (tx, mut rx) = mpsc::unbounded_channel::<DebounceEventResult>();
    let debouncer = match new_debouncer(
        std::time::Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    ) {
        Ok(mut d) => {
            if let Err(err) = d.watch(&dir, RecursiveMode::NonRecursive) {
                tracing::warn!(?dir, %err, "simulator settings watch failed; live-reload off");
                return;
            }
            d
        }
        Err(err) => {
            tracing::warn!(%err, "could not create simulator settings watcher; live-reload off");
            return;
        }
    };
    // Leak the debouncer so its FSEvents thread lives for the whole process
    // (same strategy the sibling settings modules use).
    let _: &'static mut Debouncer<RecommendedWatcher, FileIdMap> = Box::leak(Box::new(debouncer));

    cx.spawn(async move |cx| {
        while let Some(result) = rx.recv().await {
            if !batch_touches_settings(&result) {
                continue;
            }
            let next = load();
            cx.update(|cx| {
                apply(cx, next);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_on_with_a_ten_minute_idle_shutdown_and_half_res_30fps() {
        let s = SimulatorSettings::default();
        assert!(s.enabled);
        assert!(s.auto_open);
        assert!(s.agent_control);
        assert_eq!(s.default_device, None);
        assert_eq!(s.idle_shutdown_minutes, 10);
        assert_eq!(s.stream.fps, 30);
        assert_eq!(s.stream.resolution, Resolution::Half);
        assert!(!s.stream.show_fps);
        assert_eq!(s.stream.effective_scale(), 0.5);
    }

    #[test]
    fn round_trips_through_toml() {
        let s = SimulatorSettings {
            enabled: true,
            auto_open: false,
            agent_control: false,
            default_device: Some("ABCD-1234".to_string()),
            idle_shutdown_minutes: 15,
            stream: StreamSettings {
                fps: 60,
                resolution: Resolution::Full,
                show_fps: true,
            },
        };
        let parsed = SimulatorSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed, s);
    }

    #[test]
    fn a_partial_file_fills_missing_keys_from_defaults() {
        // Old builds, or a user who hand-trims the file, must still load —
        // an absent key is not an error.
        let s = SimulatorSettings::from_toml_str("enabled = false\n").expect("parses");
        assert!(!s.enabled);
        // Every other field falls back to its default — except agent
        // control, which a file without it reads as off (fail closed).
        assert!(!s.agent_control);
        assert!(s.auto_open);
        assert_eq!(s.stream.fps, 30);
    }

    #[test]
    fn an_empty_file_loads_with_agent_control_off() {
        // An empty file is a write cut short: everything defaults except the
        // one permission, which stays closed.
        let s = SimulatorSettings::from_toml_str("").expect("empty parses");
        assert_eq!(s, SimulatorSettings { agent_control: false, ..SimulatorSettings::default() });
    }

    #[test]
    fn a_corrupt_file_does_not_parse_and_the_caller_must_fail_closed() {
        // `load()` itself needs the app data dir to exercise end to end, so
        // this pins the piece that does not: a bad parse plus the fallback
        // the caller is required to use.
        assert!(SimulatorSettings::from_toml_str("not valid = = toml").is_err());
        let fallback = SimulatorSettings::fail_closed();
        assert!(!fallback.agent_control);
        // Fails closed on exactly one field — everything else stays the
        // ordinary default, so a corrupt file does not also hide the tab.
        assert!(fallback.enabled);
        assert!(fallback.auto_open);
    }

    #[test]
    fn fps_snaps_to_the_nearest_allowed_value() {
        assert_eq!(clamp_fps(0), 15);
        assert_eq!(clamp_fps(20), 15);
        assert_eq!(clamp_fps(24), 30);
        assert_eq!(clamp_fps(45), 30);
        assert_eq!(clamp_fps(50), 60);
        assert_eq!(clamp_fps(1000), 60);
        for allowed in ALLOWED_FPS {
            assert_eq!(clamp_fps(allowed), allowed);
        }
    }

    #[test]
    fn sanitizing_snaps_fps_and_trims_a_blank_or_padded_device_id() {
        let s = SimulatorSettings {
            default_device: Some("   ".to_string()),
            stream: StreamSettings {
                fps: 24,
                ..StreamSettings::default()
            },
            ..SimulatorSettings::default()
        }
        .sanitized();
        assert_eq!(s.default_device, None);
        assert_eq!(s.stream.fps, 30);

        let s = SimulatorSettings {
            default_device: Some("  ABCD-1234  ".to_string()),
            ..SimulatorSettings::default()
        }
        .sanitized();
        assert_eq!(s.default_device, Some("ABCD-1234".to_string()));
    }

    #[test]
    fn resolution_scale_is_half_or_full() {
        assert_eq!(Resolution::Half.scale(), 0.5);
        assert_eq!(Resolution::Full.scale(), 1.0);
    }
}
