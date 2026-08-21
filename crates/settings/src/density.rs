//! Density constants — heights, paddings, radii.
//!
//! All values in CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

#[derive(Debug, Clone, Copy)]
pub struct Density {
    // Heights
    pub h_top_bar: f32,
    pub h_status_bar: f32,
    pub h_tab: f32,
    pub h_row: f32,

    // Radii.
    //
    // Every value below sits on one scale: a 10px base with ratio steps
    // xs `0.2×` (2) / sm `0.6×` (6) / md `0.8×` (8) / lg `1×` (10) /
    // xl `1.4×` (14). The names stay semantic rather than t-shirt-sized
    // because `r_card` says what it is for and `r_md` does not — but each one
    // now resolves to a step, so corners across the cockpit agree.
    //
    // Before this, the three shipped radii were 8 / 4 / 3 and only the first
    // was on the scale, which is why chrome corners never quite matched.
    pub r_card: f32,
    pub r_xs: f32,
    /// Large radius — `1×` the base. For surfaces that read as their own
    /// sheet rather than a card in a list: modals, floating panels.
    pub r_lg: f32,
    /// Extra-large — `1.4×`. Reserved for the largest floating surfaces.
    pub r_xl: f32,

    // Padding / spacing
    pub pad_panel: f32,
    pub pad_row: f32,
    pub gap_inline: f32,

    // Action-row + overlay chrome (added in design-system tightening pass).
    /// Row that hosts inline action buttons (stash entry with Apply/Pop/
    /// Drop, worktree entry with Remove, empty placeholders that need
    /// to occupy the same height). Taller than a plain `h_row` so the
    /// 22px xsmall buttons sit centred without crowding the row border.
    /// Replaces hand-coded `h_row * 1.4` arithmetic.
    pub h_action_row: f32,
    /// Inner padding for floating cards (context menus, pickers,
    /// dropdowns). Slightly tighter than `pad_panel` because overlays
    /// sit on their own surface with a border and don't need the same
    /// internal breathing room.
    pub pad_overlay: f32,
    /// Row height for items inside a floating card (context menu rows,
    /// picker rows). Shared across pane / adapter / commit-context
    /// menus so the click targets feel like one component family.
    pub h_overlay_item: f32,
    /// Chip corner radius — the `0.2×` step. Intentionally smaller than
    /// `r_xs` (which is for inputs / buttons): chips are inline badges and
    /// read tighter at a smaller radius. SCM ref chips, search-toggle pills,
    /// diff hunk action chips all share this value. Moved 3 → 2 to land on
    /// the scale; the tier distinction it encodes is preserved.
    pub r_chip: f32,

    // Sidebar default width (legacy — superseded by w_left_rail; kept for
    // backward compat with existing tests / phase-0 stub).
    pub w_sidebar: f32,
    /// Left rail width (workspaces + nav). Default 250px; min 220, max 500.
    pub w_left_rail: f32,
}

impl Density {
    /// Tight cockpit density. The only density in v1.
    pub fn cockpit() -> Self {
        Self {
            // Per-column chrome row height. Sized so the chrome row's
            // vertical center (y=16) lines up with where the macOS
            // traffic-light glyphs are drawn — `traffic_light_position
            // = point(12, 10)` puts the 12-px button at y=10..22,
            // center y=16. Tab strip lives in its own row BELOW this
            // one — see `workspace_root.rs` — keeping chip drag
            // delivery clear of any AppKit title-bar drag region.
            h_top_bar: 32.0,
            // 24 (not 22): room for the metric strip without descender clipping.
            h_status_bar: 24.0,
            h_tab: 28.0,
            h_row: 24.0,
            // 0.8× — unchanged, it was already the one radius on the scale.
            r_card: 8.0,
            // 0.6× — was 4.0, which sat between the xs and sm steps.
            r_xs: 6.0,
            r_lg: 10.0,
            r_xl: 14.0,
            pad_panel: 8.0,
            pad_row: 6.0,
            gap_inline: 6.0,
            h_action_row: 34.0,
            pad_overlay: 6.0,
            h_overlay_item: 30.0,
            // 0.2× — was 3.0, just off the scale's tightest step.
            r_chip: 2.0,
            w_sidebar: 240.0,
            w_left_rail: 250.0,
        }
    }
}

impl Default for Density {
    fn default() -> Self {
        Self::cockpit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base the radius scale is derived from. Stated once here so the
    /// test below fails if someone edits a radius without moving the base.
    const RADIUS_BASE: f32 = 10.0;

    /// Every shipped radius must be a documented step off the base.
    ///
    /// This is the invariant the design guidelines describe and the tree did
    /// not hold: before this test the radii were 8 / 4 / 3, of which only 8
    /// was on the scale. Corners that disagree by a pixel or two read as
    /// sloppiness rather than as a decision, and the drift is invisible in
    /// review — which is exactly what a test is for.
    #[test]
    fn every_radius_sits_on_the_ratio_scale() {
        let d = Density::cockpit();
        let steps = [0.2_f32, 0.6, 0.8, 1.0, 1.4];
        let allowed: Vec<f32> = steps.iter().map(|s| s * RADIUS_BASE).collect();

        for (name, value) in [
            ("r_chip", d.r_chip),
            ("r_xs", d.r_xs),
            ("r_card", d.r_card),
            ("r_lg", d.r_lg),
            ("r_xl", d.r_xl),
        ] {
            assert!(
                allowed.iter().any(|a| (a - value).abs() < f32::EPSILON),
                "{name} = {value} is not a step on the {RADIUS_BASE}px scale {allowed:?}"
            );
        }
    }

    /// The tiers have to stay ordered, or the semantic names stop meaning
    /// anything: a chip must not be rounder than the card it sits in.
    #[test]
    fn radius_tiers_stay_ordered() {
        let d = Density::cockpit();
        assert!(d.r_chip < d.r_xs, "chips must read tighter than inputs");
        assert!(d.r_xs < d.r_card, "inputs must read tighter than cards");
        assert!(d.r_card < d.r_lg, "cards must read tighter than sheets");
        assert!(d.r_lg < d.r_xl);
    }
}
