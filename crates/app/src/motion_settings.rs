//! App-side installer for the [`Motion`] global (animation durations +
//! reduced-motion gate).
//!
//! The reduced-motion preference is resolved **once at startup** and installed
//! as the [`Motion`] global, so every animated surface reads one source of
//! truth (see `oximux_settings::motion`). Reading once per launch matches how
//! the OS exposes its own "reduce motion" accessibility flag — it isn't a
//! live-tunable knob like the terminal render settings, so this needs neither a
//! toml file nor a file-watch.
//!
//! Source of the flag today: the `OXIMUX_REDUCE_MOTION` env var (`1`/`true`/
//! `yes`/`on` ⇒ reduced). This is the seam a future OS-preference query
//! (macOS `accessibilityDisplayShouldReduceMotion`) or a settings toggle would
//! plug into — call sites never change because they read the resolved global.

use gpui::App;
use oximux_settings::Motion;

/// Env var that forces the reduced-motion variant when truthy.
const REDUCE_MOTION_ENV: &str = "OXIMUX_REDUCE_MOTION";

/// True for `1` / `true` / `yes` / `on` (case-insensitive); anything else —
/// including an unset var — is false.
fn env_requests_reduced() -> bool {
    std::env::var(REDUCE_MOTION_ENV)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Resolve the reduced-motion preference and install the [`Motion`] global.
/// Call once from the app's `run` closure, before any window opens.
pub fn install(cx: &mut App) {
    cx.set_global(Motion::resolve(env_requests_reduced()));
}

/// Snapshot the active [`Motion`] tokens. Falls back to [`Motion::cockpit`] if
/// the global was never installed (headless tests, early startup) so render
/// paths stay total.
pub fn active(cx: &App) -> Motion {
    cx.try_global::<Motion>().copied().unwrap_or_default()
}
