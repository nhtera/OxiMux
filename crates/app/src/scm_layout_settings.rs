//! Persistence + clamps for the SCM sidebar panel width and the
//! commit-graph section height (Phase 13).
//!
//! Both values live in the global `SettingsRepo` key/value store — they
//! are UX preferences attached to the user's window layout, not tied to
//! repo identity (so per-worktree storage in `worktree_settings` would
//! be the wrong fit).
//!
//! Storage shape: a single decimal-string-encoded `f32` per key. Parse
//! failures fall through to the default; out-of-range values are
//! clamped on load AND save so a corrupt write can't leak past either
//! direction.

use oximux_storage::SettingsRepo;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Settings key holding the right-sidebar panel width in pixels.
pub const KEY_SCM_PANEL_WIDTH: &str = "scm_panel_width";

/// Settings key holding the commit-graph section height in pixels.
pub const KEY_SCM_GRAPH_HEIGHT: &str = "scm_graph_height";

// ---------------------------------------------------------------------------
// Panel-width bounds
// ---------------------------------------------------------------------------

/// Default sidebar width when no persisted value exists.
pub const DEFAULT_PANEL_WIDTH: f32 = 360.0;

/// Minimum sidebar width the user can resize down to.
pub const MIN_PANEL_WIDTH: f32 = 220.0;

/// Reserved right-side space; cap = `window_width − MAX_PANEL_WIDTH_RESERVE`.
/// Keeps a usable strip of the main pane visible at every window size.
pub const MAX_PANEL_WIDTH_RESERVE: f32 = 320.0;

// ---------------------------------------------------------------------------
// Graph-height bounds
// ---------------------------------------------------------------------------

/// Default commit-graph section height when no persisted value exists.
/// Matches the pre-Phase-13 hard-coded value so existing users don't
/// see a layout shift on upgrade.
pub const DEFAULT_GRAPH_HEIGHT: f32 = 240.0;

/// Minimum graph height — enough to show ~3 rows + header chrome.
pub const MIN_GRAPH_HEIGHT: f32 = 96.0;

/// Maximum graph height expressed as a ratio of the window height.
/// 33vh keeps the file list + diff view above it from getting squeezed
/// off-screen on a small laptop display.
pub const MAX_GRAPH_HEIGHT_VH_RATIO: f32 = 0.33;

// ---------------------------------------------------------------------------
// Clamps
// ---------------------------------------------------------------------------

/// Clamp a candidate panel width against the bounds for the current
/// window width. The ceiling never drops below `MIN_PANEL_WIDTH` — on a
/// very narrow window the user keeps the floor at minimum width rather
/// than collapsing the panel entirely.
pub fn clamp_panel_width(value: f32, window_width: f32) -> f32 {
    let ceiling = (window_width - MAX_PANEL_WIDTH_RESERVE).max(MIN_PANEL_WIDTH);
    value.clamp(MIN_PANEL_WIDTH, ceiling)
}

/// Clamp a candidate graph height against the bounds for the current
/// window height. The ceiling never drops below `MIN_GRAPH_HEIGHT` so a
/// very short window still allows the minimum displayable graph.
pub fn clamp_graph_height(value: f32, window_height: f32) -> f32 {
    let ceiling = (window_height * MAX_GRAPH_HEIGHT_VH_RATIO).max(MIN_GRAPH_HEIGHT);
    value.clamp(MIN_GRAPH_HEIGHT, ceiling)
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// Load the persisted panel width, falling back to `DEFAULT_PANEL_WIDTH`
/// on miss / parse failure / DB error. The returned value is always
/// inside the clamp bounds for the current window width.
pub fn load_panel_width(repo: &SettingsRepo, window_width: f32) -> f32 {
    let raw = match repo.get(KEY_SCM_PANEL_WIDTH) {
        Ok(Some(s)) => s,
        _ => return clamp_panel_width(DEFAULT_PANEL_WIDTH, window_width),
    };
    let parsed = raw.trim().parse::<f32>().unwrap_or(DEFAULT_PANEL_WIDTH);
    let sane = if parsed.is_finite() {
        parsed
    } else {
        DEFAULT_PANEL_WIDTH
    };
    clamp_panel_width(sane, window_width)
}

/// Persist a panel width. Clamping is done by the caller against the
/// live window width — this helper only formats + writes. A storage
/// error is logged and swallowed; a layout preference shouldn't take
/// down the app.
pub fn save_panel_width(repo: &SettingsRepo, value: f32) {
    let encoded = format!("{value}");
    if let Err(err) = repo.set(KEY_SCM_PANEL_WIDTH, &encoded) {
        tracing::warn!(
            target: "oximux_app::scm_layout_settings",
            "failed to persist scm_panel_width: {err}"
        );
    }
}

/// Load the persisted graph height, falling back to
/// `DEFAULT_GRAPH_HEIGHT` on miss / parse failure / DB error. The
/// returned value is always inside the clamp bounds for the current
/// window height.
pub fn load_graph_height(repo: &SettingsRepo, window_height: f32) -> f32 {
    let raw = match repo.get(KEY_SCM_GRAPH_HEIGHT) {
        Ok(Some(s)) => s,
        _ => return clamp_graph_height(DEFAULT_GRAPH_HEIGHT, window_height),
    };
    let parsed = raw.trim().parse::<f32>().unwrap_or(DEFAULT_GRAPH_HEIGHT);
    let sane = if parsed.is_finite() {
        parsed
    } else {
        DEFAULT_GRAPH_HEIGHT
    };
    clamp_graph_height(sane, window_height)
}

/// Persist a graph height. See [`save_panel_width`] for error policy.
pub fn save_graph_height(repo: &SettingsRepo, value: f32) {
    let encoded = format!("{value}");
    if let Err(err) = repo.set(KEY_SCM_GRAPH_HEIGHT, &encoded) {
        tracing::warn!(
            target: "oximux_app::scm_layout_settings",
            "failed to persist scm_graph_height: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// Keyboard-resize step calculator (Phase 13)
// ---------------------------------------------------------------------------

/// Step size for an Arrow-key tick (pixels).
pub const KEY_STEP_PX: f32 = 16.0;

/// Step size for a Shift+Arrow tick (pixels).
pub const KEY_STEP_SHIFT_PX: f32 = 32.0;

/// Translate a keyboard event on the graph resize rail into a
/// candidate new height. Returns `None` for keys this handler does
/// not care about so the caller can avoid both notifies and
/// `cx.stop_propagation` for unrelated keystrokes.
///
/// The candidate is intentionally NOT clamped here — clamping needs
/// the live window height and runs in [`clamp_graph_height`]. Pure
/// helper so it can be exercised directly in unit tests.
pub fn next_graph_height(
    current: f32,
    key: &str,
    shift: bool,
    window_height: f32,
) -> Option<f32> {
    let step = if shift { KEY_STEP_SHIFT_PX } else { KEY_STEP_PX };
    match key {
        "up" => Some(current + step),
        "down" => Some(current - step),
        "home" => Some(MIN_GRAPH_HEIGHT),
        "end" => Some(window_height * MAX_GRAPH_HEIGHT_VH_RATIO),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::open_memory;

    fn repo() -> SettingsRepo {
        SettingsRepo::new(open_memory().expect("open memory"))
    }

    // ----- clamp_panel_width -----

    #[test]
    fn clamp_panel_width_under_min_snaps_to_min() {
        assert_eq!(clamp_panel_width(50.0, 1600.0), MIN_PANEL_WIDTH);
    }

    #[test]
    fn clamp_panel_width_over_max_snaps_to_ceiling() {
        // window 1600 → ceiling = 1600 - 320 = 1280
        assert_eq!(clamp_panel_width(9999.0, 1600.0), 1280.0);
    }

    #[test]
    fn clamp_panel_width_in_range_is_unchanged() {
        assert_eq!(clamp_panel_width(450.0, 1600.0), 450.0);
    }

    #[test]
    fn clamp_panel_width_tiny_window_keeps_floor() {
        // If window_width < MIN+RESERVE the ceiling would drop below the
        // floor; the function must keep ceiling ≥ MIN so .clamp doesn't
        // panic on (min > max).
        let w = clamp_panel_width(500.0, 400.0);
        assert!(w >= MIN_PANEL_WIDTH);
    }

    // ----- clamp_graph_height -----

    #[test]
    fn clamp_graph_height_under_min_snaps_to_min() {
        assert_eq!(clamp_graph_height(10.0, 900.0), MIN_GRAPH_HEIGHT);
    }

    #[test]
    fn clamp_graph_height_over_max_snaps_to_vh_ceiling() {
        // window 900 → ceiling = 900 * 0.33 = 297
        let h = clamp_graph_height(5000.0, 900.0);
        assert!((h - 297.0).abs() < 0.001);
    }

    #[test]
    fn clamp_graph_height_in_range_is_unchanged() {
        assert_eq!(clamp_graph_height(200.0, 900.0), 200.0);
    }

    // ----- load_panel_width -----

    #[test]
    fn load_panel_width_missing_returns_default() {
        let r = repo();
        assert_eq!(load_panel_width(&r, 1600.0), DEFAULT_PANEL_WIDTH);
    }

    #[test]
    fn load_panel_width_round_trips_through_save() {
        let r = repo();
        save_panel_width(&r, 420.0);
        assert_eq!(load_panel_width(&r, 1600.0), 420.0);
    }

    #[test]
    fn load_panel_width_garbage_returns_default() {
        let r = repo();
        r.set(KEY_SCM_PANEL_WIDTH, "not-a-number").unwrap();
        assert_eq!(load_panel_width(&r, 1600.0), DEFAULT_PANEL_WIDTH);
    }

    #[test]
    fn load_panel_width_out_of_range_is_clamped() {
        let r = repo();
        // Saving a too-large value, then loading it back, should yield
        // the clamped ceiling — defense in depth against corrupt writes
        // from prior versions.
        r.set(KEY_SCM_PANEL_WIDTH, "9999").unwrap();
        assert_eq!(load_panel_width(&r, 1600.0), 1280.0);
    }

    #[test]
    fn load_panel_width_non_finite_returns_default() {
        let r = repo();
        r.set(KEY_SCM_PANEL_WIDTH, "inf").unwrap();
        assert_eq!(load_panel_width(&r, 1600.0), DEFAULT_PANEL_WIDTH);
    }

    // ----- load_graph_height -----

    #[test]
    fn load_graph_height_missing_returns_default() {
        let r = repo();
        assert_eq!(load_graph_height(&r, 900.0), DEFAULT_GRAPH_HEIGHT);
    }

    #[test]
    fn load_graph_height_round_trips_through_save() {
        let r = repo();
        save_graph_height(&r, 180.0);
        assert_eq!(load_graph_height(&r, 900.0), 180.0);
    }

    #[test]
    fn load_graph_height_out_of_range_is_clamped() {
        let r = repo();
        r.set(KEY_SCM_GRAPH_HEIGHT, "5000").unwrap();
        let h = load_graph_height(&r, 900.0);
        assert!((h - 297.0).abs() < 0.001);
    }

    #[test]
    fn load_graph_height_garbage_returns_default() {
        let r = repo();
        r.set(KEY_SCM_GRAPH_HEIGHT, "garbage").unwrap();
        assert_eq!(load_graph_height(&r, 900.0), DEFAULT_GRAPH_HEIGHT);
    }

    // ----- next_graph_height key mapping -----

    #[test]
    fn next_graph_height_up_grows_by_step() {
        assert_eq!(next_graph_height(200.0, "up", false, 900.0), Some(216.0));
    }

    #[test]
    fn next_graph_height_down_shrinks_by_step() {
        assert_eq!(next_graph_height(200.0, "down", false, 900.0), Some(184.0));
    }

    #[test]
    fn next_graph_height_shift_arrow_doubles_step() {
        assert_eq!(next_graph_height(200.0, "up", true, 900.0), Some(232.0));
        assert_eq!(
            next_graph_height(200.0, "down", true, 900.0),
            Some(168.0)
        );
    }

    #[test]
    fn next_graph_height_home_snaps_to_min() {
        assert_eq!(
            next_graph_height(500.0, "home", false, 900.0),
            Some(MIN_GRAPH_HEIGHT)
        );
    }

    #[test]
    fn next_graph_height_end_returns_vh_ceiling_pre_clamp() {
        // The helper returns the un-clamped ceiling so the caller's
        // clamp_graph_height stays the single source of truth on
        // bounds. Caller is expected to clamp.
        let candidate = next_graph_height(200.0, "end", false, 900.0).unwrap();
        assert!((candidate - 297.0).abs() < 0.001);
    }

    #[test]
    fn next_graph_height_unknown_key_returns_none() {
        assert!(next_graph_height(200.0, "left", false, 900.0).is_none());
        assert!(next_graph_height(200.0, "space", false, 900.0).is_none());
        assert!(next_graph_height(200.0, "tab", false, 900.0).is_none());
    }

    #[test]
    fn next_graph_height_pipeline_is_clamped_by_caller() {
        // Composing next_graph_height with clamp_graph_height should
        // saturate when the candidate runs past either bound.
        let too_small = next_graph_height(
            MIN_GRAPH_HEIGHT + 5.0,
            "down",
            true, // shift = 32px step
            900.0,
        )
        .unwrap();
        assert_eq!(clamp_graph_height(too_small, 900.0), MIN_GRAPH_HEIGHT);

        let too_big = next_graph_height(
            900.0 * MAX_GRAPH_HEIGHT_VH_RATIO - 5.0,
            "up",
            true,
            900.0,
        )
        .unwrap();
        let ceiling = 900.0 * MAX_GRAPH_HEIGHT_VH_RATIO;
        assert!((clamp_graph_height(too_big, 900.0) - ceiling).abs() < 0.001);
    }
}
