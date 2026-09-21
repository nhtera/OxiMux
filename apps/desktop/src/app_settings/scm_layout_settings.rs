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

use oximux_core::ViewMode;
use oximux_settings::Density;
use oximux_storage::SettingsRepo;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Settings key holding the right-sidebar panel width in pixels.
pub const KEY_SCM_PANEL_WIDTH: &str = "scm_panel_width";

/// Settings key holding the commit-graph section height in pixels.
pub const KEY_SCM_GRAPH_HEIGHT: &str = "scm_graph_height";

/// Settings key holding the stash section's body height in pixels.
pub const KEY_SCM_STASH_HEIGHT: &str = "scm_stash_height";

/// Settings key holding the stash section's file layout — `"flat"` or
/// `"tree"`, per [`ViewMode::as_str`].
///
/// **Global, not per-worktree.** The CHANGES list keeps its own layout in the
/// V006 `worktree_settings` row, which is right for it: those files belong to
/// one worktree. `refs/stash` lives in the git *common* directory, so every
/// worktree of a repo shows the same stack — a per-worktree key would paint
/// one list two different ways in two sibling windows.
pub const KEY_SCM_STASH_VIEW_MODE: &str = "scm_stash_view_mode";

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
/// 70vh lets the user pull the graph up to fill most of the panel (the
/// VS Code gesture) while still leaving the file list a usable strip; the
/// `MIN_GRAPH_HEIGHT` floor on the *other* sections is implicit — the
/// commit area + filter row never collapse because they sit above the
/// `flex_1` file list that absorbs the squeeze first.
pub const MAX_GRAPH_HEIGHT_VH_RATIO: f32 = 0.7;

// ---------------------------------------------------------------------------
// Stash-height bounds
// ---------------------------------------------------------------------------

/// Default stash-section body height when no persisted value exists.
///
/// Roughly what the Phase 1 eight-row cap worked out to at cockpit density,
/// so the section keeps the size users already know when the cap is replaced
/// by a real drag handle.
pub const DEFAULT_STASH_HEIGHT: f32 = 240.0;

/// Minimum stash-section height — about two rows plus a scroll hint. Lower
/// than [`MIN_GRAPH_HEIGHT`] because a stash row carries no timeline gutter
/// to keep legible; the list still reads as a list at this size.
pub const MIN_STASH_HEIGHT: f32 = 68.0;

// ---------------------------------------------------------------------------
// The two sections share one budget
// ---------------------------------------------------------------------------

/// Combined ceiling for the stash section and the graph, as a ratio of the
/// window height.
///
/// The same 70vh the graph alone used to get, now shared: the SCM column's
/// only flexible child is the changed-files block, so every pixel the two
/// `flex_shrink_0` sections take comes out of it. It stops giving at
/// [`files_floor`], after which the sections themselves are pushed past the
/// column's `overflow_hidden` and clipped — taking the lower section's drag
/// handle, the escape valve a cramped panel depends on, off-screen with it.
///
/// Deliberately not raised now that there are two sections. Two tall sections
/// squeeze the file list exactly as hard as one did; splitting a bigger
/// allowance between them would only move the clip further out, not remove it.
pub const MAX_SECTIONS_VH_RATIO: f32 = MAX_GRAPH_HEIGHT_VH_RATIO;

/// Divide the shared budget between the two sections, returning
/// `(stash_ceiling, graph_ceiling)`.
///
/// Each argument is that section's **chosen** height — what the user dragged
/// to — or `None` when it is showing no body at all (collapsed, or hidden by
/// the current scope), in which case it costs its sibling nothing.
///
/// # Why both at once, and why from the chosen heights
///
/// The obvious shape — "this section's ceiling is the budget minus whatever
/// the other one is currently painting" — **oscillates**. Each section's
/// painted height is `min(chosen, ceiling)`, so deriving one ceiling from the
/// other's painted height closes a loop: with two sections chosen at 500 in a
/// 630 budget it alternates 500/500 → 130/130 → 500/500 on consecutive
/// frames, forever, and the panel visibly flickers. (Simulated before this
/// function was written, and pinned by `the_fit_does_not_oscillate` below.)
///
/// Taking the *chosen* heights breaks the loop: they are inputs the render
/// pass never writes, so the result is the same on every frame.
///
/// # How the deficit is split
///
/// In proportion to what each section asked for, rather than by draining one
/// first. Neither is the junior section: a user who dragged the stash list
/// tall did so on purpose, and so did one who dragged the graph tall. Both
/// floors are then honoured, and the pair still sums to exactly the budget.
pub fn fit_sections(stash: Option<f32>, graph: Option<f32>, window_height: f32) -> (f32, f32) {
    let budget = window_height * MAX_SECTIONS_VH_RATIO;
    let (s, g) = (stash.unwrap_or(0.0), graph.unwrap_or(0.0));
    if s + g <= budget {
        // They fit. Each section's ceiling is the slack its sibling is not
        // using, so either can still be dragged into the free space.
        return ((budget - g).max(MIN_STASH_HEIGHT), (budget - s).max(MIN_GRAPH_HEIGHT));
    }
    // Over budget — which a window resize or a pair of heights restored from
    // a taller monitor can do without anyone dragging anything.
    let budget = budget.max(MIN_STASH_HEIGHT + MIN_GRAPH_HEIGHT);
    let fitted_stash =
        (s * budget / (s + g)).clamp(MIN_STASH_HEIGHT, budget - MIN_GRAPH_HEIGHT);
    (fitted_stash, budget - fitted_stash)
}

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

/// [`clamp_graph_height`]'s counterpart for the stash section.
pub fn clamp_stash_height(value: f32, window_height: f32) -> f32 {
    let ceiling = (window_height * MAX_SECTIONS_VH_RATIO).max(MIN_STASH_HEIGHT);
    value.clamp(MIN_STASH_HEIGHT, ceiling)
}

// ---------------------------------------------------------------------------
// Changed-files floor
// ---------------------------------------------------------------------------

/// Extra vertical space a changed-file row carries over a plain `h_row`.
/// Mirrors the `+ 2.0` in `git_panel::row_renderer` (`:313`), which is the
/// single-line file-row height the panel actually paints. Kept in sync by
/// derivation, not by memory: the floor has to be measured in the same unit
/// the rows are.
const FILE_ROW_EXTRA: f32 = 2.0;

/// Number of file rows the floor guarantees below the section header. Two,
/// not one: a single row plus a header reads as a rendering glitch, while two
/// reads as a list that has been squeezed and can be scrolled.
const FILES_FLOOR_ROWS: f32 = 2.0;

/// Minimum height the changed-files section may be squeezed to: one section
/// header plus [`FILES_FLOOR_ROWS`] file rows.
///
/// The SCM column's file block is the only `flex_1` child, so it absorbs the
/// entire height deficit when the stash section and the graph are both
/// expanded. With `min_h(0)` it collapsed to near-zero and `overflow_hidden`
/// guillotined the CHANGES header mid-row. This floor stops the squeeze while
/// there is still a header and a usable strip of list; past it the inner
/// `git-panel-scroll` region scrolls instead.
///
/// Density-derived on purpose — a literal floor is right at 100% zoom and
/// cramped at 150%, which is exactly the failure `source_control::style`
/// documents. See that module for why a helper that ignores [`Density`] is
/// not acceptable on this surface.
///
/// The floor only clips rather than overflowing because the SCM body column
/// carries `overflow_hidden`; without it the floor pushes the sections below
/// the visible panel and takes the graph's drag handle off-screen with them.
pub fn files_floor(density: &Density) -> f32 {
    density.h_row + FILES_FLOOR_ROWS * (density.h_row + FILE_ROW_EXTRA)
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

/// [`load_graph_height`]'s counterpart for the stash section.
pub fn load_stash_height(repo: &SettingsRepo, window_height: f32) -> f32 {
    let raw = match repo.get(KEY_SCM_STASH_HEIGHT) {
        Ok(Some(s)) => s,
        _ => return clamp_stash_height(DEFAULT_STASH_HEIGHT, window_height),
    };
    let parsed = raw.trim().parse::<f32>().unwrap_or(DEFAULT_STASH_HEIGHT);
    let sane = if parsed.is_finite() {
        parsed
    } else {
        DEFAULT_STASH_HEIGHT
    };
    clamp_stash_height(sane, window_height)
}

/// Persist a stash-section height. See [`save_panel_width`] for error policy.
pub fn save_stash_height(repo: &SettingsRepo, value: f32) {
    let encoded = format!("{value}");
    if let Err(err) = repo.set(KEY_SCM_STASH_HEIGHT, &encoded) {
        tracing::warn!(
            target: "oximux_app::scm_layout_settings",
            "failed to persist scm_stash_height: {err}"
        );
    }
}

/// Read the stash section's file layout. Anything unrecognised — including a
/// missing key and a corrupt value — decodes to `Flat`, which is
/// [`ViewMode::from_str`]'s documented contract for a free-form store.
pub fn load_stash_view_mode(repo: &SettingsRepo) -> ViewMode {
    match repo.get(KEY_SCM_STASH_VIEW_MODE) {
        Ok(Some(s)) => ViewMode::from_str(s.trim()),
        _ => ViewMode::default(),
    }
}

/// Persist the stash section's file layout. See [`save_panel_width`] for the
/// error policy: a failed write costs the preference on next launch, never the
/// interaction the user just made.
pub fn save_stash_view_mode(repo: &SettingsRepo, mode: ViewMode) {
    if let Err(err) = repo.set(KEY_SCM_STASH_VIEW_MODE, mode.as_str()) {
        tracing::warn!(
            target: "oximux_app::scm_layout_settings",
            "failed to persist scm_stash_view_mode: {err}"
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
    next_section_height(current, key, shift, MIN_GRAPH_HEIGHT, window_height)
}

/// The section-agnostic form. `min` is the section's own floor, which is what
/// Home means; End means "as tall as the shared budget allows", left to the
/// caller's ceiling to trim against the sibling.
pub fn next_section_height(
    current: f32,
    key: &str,
    shift: bool,
    min: f32,
    window_height: f32,
) -> Option<f32> {
    let step = if shift { KEY_STEP_SHIFT_PX } else { KEY_STEP_PX };
    match key {
        "up" => Some(current + step),
        "down" => Some(current - step),
        "home" => Some(min),
        "end" => Some(window_height * MAX_SECTIONS_VH_RATIO),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::{Appearance, DensityPreset, UiScale};
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
        // window 900 → ceiling = 900 * MAX_GRAPH_HEIGHT_VH_RATIO
        let h = clamp_graph_height(5000.0, 900.0);
        assert!((h - 900.0 * MAX_GRAPH_HEIGHT_VH_RATIO).abs() < 0.001);
    }

    #[test]
    fn clamp_graph_height_in_range_is_unchanged() {
        assert_eq!(clamp_graph_height(200.0, 900.0), 200.0);
    }

    // ----- files_floor -----

    fn density_at(preset: DensityPreset, percent: u16) -> Density {
        Density::for_appearance(Appearance {
            density: preset,
            scale: UiScale::from_percent(percent),
            ..Appearance::default()
        })
    }

    #[test]
    fn files_floor_is_a_header_plus_two_rows() {
        let d = Density::cockpit();
        assert_eq!(files_floor(&d), d.h_row + 2.0 * (d.h_row + FILE_ROW_EXTRA));
    }

    #[test]
    fn files_floor_leaves_room_for_the_header_it_protects() {
        // The whole point of the floor: whatever else gets squeezed, a full
        // section header still fits inside it with rows to spare.
        let d = Density::cockpit();
        assert!(files_floor(&d) > d.h_row);
    }

    #[test]
    fn files_floor_grows_with_zoom() {
        // The literal-pixel failure this function exists to avoid: right at
        // 100%, cramped at 150%. A density-derived floor scales with the rows
        // it is measured in.
        let base = files_floor(&density_at(DensityPreset::Cockpit, 100));
        let zoomed = files_floor(&density_at(DensityPreset::Cockpit, 150));
        assert!(
            zoomed > base,
            "floor must follow UI scale: {base} → {zoomed}"
        );
    }

    #[test]
    fn files_floor_grows_with_a_roomier_preset() {
        let cockpit = files_floor(&density_at(DensityPreset::Cockpit, 100));
        let roomy = files_floor(&density_at(DensityPreset::Comfortable, 100));
        assert!(
            roomy > cockpit,
            "floor must follow the density preset: {cockpit} → {roomy}"
        );
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
        assert!((h - 900.0 * MAX_GRAPH_HEIGHT_VH_RATIO).abs() < 0.001);
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
        assert!((candidate - 900.0 * MAX_GRAPH_HEIGHT_VH_RATIO).abs() < 0.001);
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

    // -----------------------------------------------------------------
    // Two sections, one budget (Phase 5)
    // -----------------------------------------------------------------

    /// Painting rule the sections actually use, mirrored here so the tests
    /// below reason about what lands on screen.
    fn painted(chosen: Option<f32>, ceiling: f32, min: f32) -> f32 {
        chosen.map_or(0.0, |c| c.min(ceiling).max(min))
    }

    #[test]
    fn a_lone_section_still_gets_the_whole_budget() {
        // The graph was the only resizable section before Phase 5, and the
        // 70vh gesture must not silently shrink for users who never open the
        // stash list.
        let (_, graph) = fit_sections(None, Some(200.0), 900.0);
        assert!((graph - 900.0 * MAX_SECTIONS_VH_RATIO).abs() < 0.001);
        assert!((graph - 900.0 * MAX_GRAPH_HEIGHT_VH_RATIO).abs() < 0.001);
    }

    #[test]
    fn a_sibling_that_fits_leaves_the_rest_as_headroom() {
        let budget = 900.0 * MAX_SECTIONS_VH_RATIO;
        let (stash, graph) = fit_sections(Some(150.0), Some(200.0), 900.0);
        assert!((stash - (budget - 200.0)).abs() < 0.001);
        assert!((graph - (budget - 150.0)).abs() < 0.001);
    }

    // The failure this whole seam exists to prevent: two sections tall enough
    // that together they push the lower one's drag handle out of the panel.
    #[test]
    fn two_oversized_sections_are_fitted_back_into_the_budget() {
        let window = 900.0;
        let budget = window * MAX_SECTIONS_VH_RATIO;
        let (cs, cg) = fit_sections(Some(500.0), Some(500.0), window);
        let (s, g) = (
            painted(Some(500.0), cs, MIN_STASH_HEIGHT),
            painted(Some(500.0), cg, MIN_GRAPH_HEIGHT),
        );
        assert!((s + g - budget).abs() < 0.001, "{s} + {g} != {budget}");
        assert!(s >= MIN_STASH_HEIGHT && g >= MIN_GRAPH_HEIGHT);
        // Equal requests, equal treatment — neither section is the junior one.
        assert!((s - g).abs() < 0.001, "{s} vs {g}");
    }

    #[test]
    fn the_deficit_is_split_in_proportion_to_what_each_asked_for() {
        let window = 900.0;
        let (cs, cg) = fit_sections(Some(600.0), Some(200.0), window);
        let s = painted(Some(600.0), cs, MIN_STASH_HEIGHT);
        let g = painted(Some(200.0), cg, MIN_GRAPH_HEIGHT);
        assert!(s > g, "the section that asked for more should keep more: {s} vs {g}");
        assert!((s + g - window * MAX_SECTIONS_VH_RATIO).abs() < 0.001);
    }

    /// The bug this function was rewritten for. Deriving each ceiling from
    /// the sibling's *painted* height closes a feedback loop through the
    /// render pass: 500/500 in a 630 budget alternated 500/500 → 130/130
    /// every frame. Taking the chosen heights makes the fit a pure function
    /// of values the render never writes, so it has to be a fixpoint.
    #[test]
    fn the_fit_does_not_oscillate() {
        let window = 900.0;
        let (chosen_s, chosen_g) = (Some(500.0), Some(500.0));
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (cs, cg) = fit_sections(chosen_s, chosen_g, window);
            seen.push((
                painted(chosen_s, cs, MIN_STASH_HEIGHT),
                painted(chosen_g, cg, MIN_GRAPH_HEIGHT),
            ));
        }
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "painted heights moved between frames: {seen:?}"
        );
    }

    // On a window too short for both floors, each section keeps its minimum
    // rather than being clamped to nothing — a section with no height has no
    // drag handle, and no way back.
    #[test]
    fn a_short_window_still_leaves_both_floors() {
        let (stash, graph) = fit_sections(Some(400.0), Some(400.0), 120.0);
        assert!(stash >= MIN_STASH_HEIGHT, "{stash}");
        assert!(graph >= MIN_GRAPH_HEIGHT, "{graph}");
    }

    #[test]
    fn a_collapsed_section_costs_its_sibling_nothing() {
        let budget = 900.0 * MAX_SECTIONS_VH_RATIO;
        let (stash, graph) = fit_sections(None, None, 900.0);
        assert!((stash - budget).abs() < 0.001);
        assert!((graph - budget).abs() < 0.001);
    }

    #[test]
    fn stash_height_round_trips_through_settings() {
        let repo = repo();
        save_stash_height(&repo, 184.0);
        assert_eq!(load_stash_height(&repo, 900.0), 184.0);
    }

    #[test]
    fn a_missing_or_corrupt_stash_height_falls_back_to_the_default() {
        let repo = repo();
        assert_eq!(
            load_stash_height(&repo, 900.0),
            clamp_stash_height(DEFAULT_STASH_HEIGHT, 900.0)
        );
        repo.set(KEY_SCM_STASH_HEIGHT, "not a number").unwrap();
        assert_eq!(
            load_stash_height(&repo, 900.0),
            clamp_stash_height(DEFAULT_STASH_HEIGHT, 900.0)
        );
        // NaN parses fine as an f32 and would otherwise sail through every
        // comparison in `clamp`, landing an un-renderable height in state.
        repo.set(KEY_SCM_STASH_HEIGHT, "NaN").unwrap();
        assert_eq!(
            load_stash_height(&repo, 900.0),
            clamp_stash_height(DEFAULT_STASH_HEIGHT, 900.0)
        );
    }

    #[test]
    fn a_stash_height_persisted_on_a_taller_monitor_is_clamped_on_load() {
        let repo = repo();
        save_stash_height(&repo, 2000.0);
        let loaded = load_stash_height(&repo, 600.0);
        assert!(loaded <= 600.0 * MAX_SECTIONS_VH_RATIO + 0.001, "{loaded}");
    }

    #[test]
    fn the_section_keyboard_step_honours_each_sections_own_floor() {
        assert_eq!(
            next_section_height(200.0, "home", false, MIN_STASH_HEIGHT, 900.0),
            Some(MIN_STASH_HEIGHT)
        );
        assert_eq!(
            next_section_height(200.0, "home", false, MIN_GRAPH_HEIGHT, 900.0),
            Some(MIN_GRAPH_HEIGHT)
        );
        // Unchanged for the graph, which now routes through the shared form.
        assert_eq!(
            next_graph_height(200.0, "home", false, 900.0),
            Some(MIN_GRAPH_HEIGHT)
        );
        assert_eq!(next_section_height(200.0, "escape", false, 1.0, 900.0), None);
    }

    // ---- stash layout -------------------------------------------------

    #[test]
    fn an_unset_stash_layout_is_flat() {
        assert_eq!(load_stash_view_mode(&repo()), ViewMode::Flat);
    }

    #[test]
    fn a_stash_layout_round_trips() {
        let r = repo();
        save_stash_view_mode(&r, ViewMode::Tree);
        assert_eq!(load_stash_view_mode(&r), ViewMode::Tree);
        save_stash_view_mode(&r, ViewMode::Flat);
        assert_eq!(load_stash_view_mode(&r), ViewMode::Flat);
    }

    #[test]
    fn a_corrupt_stash_layout_decodes_to_flat_rather_than_failing() {
        // A hand-edited or half-written value must cost the preference, not
        // the panel: there is no error path a layout toggle could take.
        let r = repo();
        r.set(KEY_SCM_STASH_VIEW_MODE, "treeish").unwrap();
        assert_eq!(load_stash_view_mode(&r), ViewMode::Flat);
    }
}
