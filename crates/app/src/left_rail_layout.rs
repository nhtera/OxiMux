//! Persistence + clamp for the left-rail (projects) width.
//!
//! The width is a user layout preference, stored as a decimal-string
//! `f32` in the global `SettingsRepo` key/value store (same shape as
//! `scm_layout_settings`). Bounds are fixed (not window-relative): the
//! rail is a narrow fixed-purpose column, so a simple 220–500px clamp
//! matches the design contract. Parse failures fall through to the
//! default; out-of-range values are clamped on both load and save so a
//! corrupt write can't leak past either direction.

use oximux_storage::SettingsRepo;

use crate::shell::left_rail::workspace_list_render::WorkspaceSortMode;

/// Settings key holding the left-rail width in pixels.
pub const KEY_LEFT_RAIL_WIDTH: &str = "left_rail_width";

/// Default width when no persisted value exists. Matches the cockpit
/// density's historical fixed rail width.
pub const DEFAULT_LEFT_RAIL_WIDTH: f32 = 250.0;

/// Minimum width the user can drag down to.
pub const MIN_LEFT_RAIL_WIDTH: f32 = 220.0;

/// Maximum width the user can drag up to.
pub const MAX_LEFT_RAIL_WIDTH: f32 = 500.0;

/// Clamp a candidate width into the fixed rail bounds.
pub fn clamp_left_rail_width(value: f32) -> f32 {
    let sane = if value.is_finite() {
        value
    } else {
        DEFAULT_LEFT_RAIL_WIDTH
    };
    sane.clamp(MIN_LEFT_RAIL_WIDTH, MAX_LEFT_RAIL_WIDTH)
}

/// Load the persisted width, falling back to the default on
/// miss / parse failure / DB error. Always returned within bounds.
pub fn load_left_rail_width(repo: &SettingsRepo) -> f32 {
    let raw = match repo.get(KEY_LEFT_RAIL_WIDTH) {
        Ok(Some(s)) => s,
        _ => return DEFAULT_LEFT_RAIL_WIDTH,
    };
    let parsed = raw.trim().parse::<f32>().unwrap_or(DEFAULT_LEFT_RAIL_WIDTH);
    clamp_left_rail_width(parsed)
}

/// Persist a width. Clamps before writing. A storage error is logged
/// and swallowed — a layout preference shouldn't take down the app.
pub fn save_left_rail_width(repo: &SettingsRepo, value: f32) {
    let encoded = format!("{}", clamp_left_rail_width(value));
    if let Err(err) = repo.set(KEY_LEFT_RAIL_WIDTH, &encoded) {
        tracing::warn!(
            target: "oximux_app::left_rail_layout",
            "failed to persist left_rail_width: {err}"
        );
    }
}

/// Settings key holding the comma-separated set of collapsed project ids.
pub const KEY_LEFT_RAIL_COLLAPSED: &str = "left_rail_collapsed_projects";

/// Load the set of collapsed project ids. Project ids are UUIDs (no
/// commas), so a plain comma-join is a safe encoding. Empty / missing /
/// error → empty list.
pub fn load_collapsed_projects(repo: &SettingsRepo) -> Vec<String> {
    let raw = match repo.get(KEY_LEFT_RAIL_COLLAPSED) {
        Ok(Some(s)) => s,
        _ => return Vec::new(),
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Persist the set of collapsed project ids (comma-joined). Error is
/// logged and swallowed.
pub fn save_collapsed_projects(repo: &SettingsRepo, ids: &[String]) {
    let encoded = ids.join(",");
    if let Err(err) = repo.set(KEY_LEFT_RAIL_COLLAPSED, &encoded) {
        tracing::warn!(
            target: "oximux_app::left_rail_layout",
            "failed to persist left_rail_collapsed_projects: {err}"
        );
    }
}

/// Settings key holding the workspace sort mode for the left rail.
pub const KEY_LEFT_RAIL_SORT_MODE: &str = "left_rail_sort_mode";

/// Load the persisted workspace sort mode. Missing / unknown / error →
/// `WorkspaceSortMode::default()`.
pub fn load_sort_mode(repo: &SettingsRepo) -> WorkspaceSortMode {
    match repo.get(KEY_LEFT_RAIL_SORT_MODE) {
        Ok(Some(s)) => WorkspaceSortMode::from_key(&s),
        _ => WorkspaceSortMode::default(),
    }
}

/// Persist the workspace sort mode. Error is logged and swallowed — a layout
/// preference shouldn't take down the app.
pub fn save_sort_mode(repo: &SettingsRepo, mode: WorkspaceSortMode) {
    if let Err(err) = repo.set(KEY_LEFT_RAIL_SORT_MODE, mode.as_key()) {
        tracing::warn!(
            target: "oximux_app::left_rail_layout",
            "failed to persist left_rail_sort_mode: {err}"
        );
    }
}

/// Settings key holding the compact-card display preference for the left rail.
pub const KEY_LEFT_RAIL_COMPACT_CARDS: &str = "left_rail_compact_cards";

/// Load the persisted compact-card preference. Missing / unparsable / error →
/// `false` (detailed two-line cards).
pub fn load_compact_cards(repo: &SettingsRepo) -> bool {
    matches!(repo.get(KEY_LEFT_RAIL_COMPACT_CARDS), Ok(Some(s)) if s.trim() == "true")
}

/// Persist the compact-card preference. Error is logged and swallowed — a
/// layout preference shouldn't take down the app.
pub fn save_compact_cards(repo: &SettingsRepo, compact: bool) {
    let encoded = if compact { "true" } else { "false" };
    if let Err(err) = repo.set(KEY_LEFT_RAIL_COMPACT_CARDS, encoded) {
        tracing::warn!(
            target: "oximux_app::left_rail_layout",
            "failed to persist left_rail_compact_cards: {err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_holds_bounds() {
        assert_eq!(clamp_left_rail_width(100.0), MIN_LEFT_RAIL_WIDTH);
        assert_eq!(clamp_left_rail_width(9999.0), MAX_LEFT_RAIL_WIDTH);
        assert_eq!(clamp_left_rail_width(300.0), 300.0);
    }

    #[test]
    fn clamp_non_finite_falls_back_to_default() {
        // Non-finite candidates (NaN / ±∞) are garbage — fall back to the
        // default rather than clamping to a bound.
        assert_eq!(clamp_left_rail_width(f32::NAN), DEFAULT_LEFT_RAIL_WIDTH);
        assert_eq!(clamp_left_rail_width(f32::INFINITY), DEFAULT_LEFT_RAIL_WIDTH);
        assert_eq!(
            clamp_left_rail_width(f32::NEG_INFINITY),
            DEFAULT_LEFT_RAIL_WIDTH
        );
    }
}
