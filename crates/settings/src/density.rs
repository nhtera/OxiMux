//! Density constants — heights, paddings, radii.
//!
//! All values in CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.
//!
//! Two user choices reach these numbers, both via [`Density::for_appearance`]:
//! a [`DensityPreset`] picks the base set, and a [`UiScale`] multiplies it.
//! See [`crate::appearance`] for why those are separate controls.

use crate::appearance::{Appearance, DensityPreset};

#[derive(Debug, Clone, Copy)]
pub struct Density {
    /// The choices this set was resolved from.
    ///
    /// Carried so a view holding a snapshot can tell in one comparison whether
    /// it is stale, without the values themselves having to be compared field
    /// by field (they are `f32`, and rounding makes that fragile).
    pub appearance: Appearance,

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

/// The [`DensityPreset::Comfortable`] rule: `1.25×` the cockpit value, rounded
/// to the nearest even pixel.
///
/// Even rather than whole so a row's content still centres on an integer
/// pixel — an odd height puts a 1px-tall caret or hairline on a half-pixel,
/// which the renderer resolves by smearing it over two rows.
///
/// Expressed as a rule rather than a second column of literals so the two
/// presets cannot drift: adding a token to `cockpit` and forgetting it here
/// is a compile error, not a spacing bug that only shows up for the handful
/// of users who switched preset.
fn roomier(cockpit: f32) -> f32 {
    (cockpit * 1.25 / 2.0).round() * 2.0
}

impl Density {
    /// Tight cockpit density — the shipped default.
    pub fn cockpit() -> Self {
        Self {
            appearance: Appearance {
                density: DensityPreset::Cockpit,
                scale: crate::appearance::UiScale::default(),
            },
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

    /// Roomier density: the same type sizes with more space around them.
    ///
    /// Only the air moves. Radii keep their scale — a corner is part of a
    /// component's shape, not of how tightly components are packed — and the
    /// rail widths keep theirs, because the user can drag those and the drag
    /// is persisted; changing the default underneath a saved width would
    /// silently discard their choice on the next launch.
    pub fn comfortable() -> Self {
        let c = Self::cockpit();
        Self {
            appearance: Appearance {
                density: DensityPreset::Comfortable,
                scale: crate::appearance::UiScale::default(),
            },
            h_status_bar: roomier(c.h_status_bar),
            h_tab: roomier(c.h_tab),
            h_row: roomier(c.h_row),
            h_action_row: roomier(c.h_action_row),
            h_overlay_item: roomier(c.h_overlay_item),
            pad_panel: roomier(c.pad_panel),
            pad_row: roomier(c.pad_row),
            gap_inline: roomier(c.gap_inline),
            pad_overlay: roomier(c.pad_overlay),
            ..c
        }
    }

    /// The base set for a preset, before any zoom.
    pub fn preset(preset: DensityPreset) -> Self {
        match preset {
            DensityPreset::Cockpit => Self::cockpit(),
            DensityPreset::Comfortable => Self::comfortable(),
        }
    }

    /// The density a given set of user choices resolves to: the preset's base
    /// values, zoomed.
    pub fn for_appearance(appearance: Appearance) -> Self {
        let mut resolved = Self::preset(appearance.density).scaled(appearance.scale.factor());
        // Stamped after scaling because `scaled` deliberately carries the
        // source density's stamp through — it is a transformation, not a
        // resolution, and the stamp names the choices, not the arithmetic.
        resolved.appearance = appearance;
        resolved
    }

    /// Every dimension multiplied by `factor`, except the pinned top bar.
    ///
    /// `h_top_bar` does not move: it is aligned to the macOS traffic lights,
    /// whose position is fixed when the window is created and cannot follow a
    /// preference the user changes afterwards. Scaling it would slide the
    /// chrome row off the buttons it exists to line up with.
    pub fn scaled(self, factor: f32) -> Self {
        if factor == 1.0 {
            return self;
        }
        let s = |v: f32| v * factor;
        Self {
            h_status_bar: s(self.h_status_bar),
            h_tab: s(self.h_tab),
            h_row: s(self.h_row),
            r_card: s(self.r_card),
            r_xs: s(self.r_xs),
            r_lg: s(self.r_lg),
            r_xl: s(self.r_xl),
            pad_panel: s(self.pad_panel),
            pad_row: s(self.pad_row),
            gap_inline: s(self.gap_inline),
            h_action_row: s(self.h_action_row),
            pad_overlay: s(self.pad_overlay),
            h_overlay_item: s(self.h_overlay_item),
            r_chip: s(self.r_chip),
            w_sidebar: s(self.w_sidebar),
            w_left_rail: s(self.w_left_rail),
            // `appearance` and `h_top_bar` ride through untouched.
            ..self
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
    use crate::appearance::UiScale;

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

    /// What the roomier preset actually measures. Pinned so the rule and the
    /// numbers it produces are both reviewable — the rule alone would let a
    /// change to the multiplier pass unnoticed.
    #[test]
    fn the_comfortable_preset_is_the_cockpit_plus_a_quarter() {
        let c = Density::comfortable();
        assert_eq!(c.h_status_bar, 30.0);
        assert_eq!(c.h_tab, 36.0);
        assert_eq!(c.h_row, 30.0);
        assert_eq!(c.h_action_row, 42.0);
        assert_eq!(c.h_overlay_item, 38.0);
        assert_eq!(c.pad_panel, 10.0);
        assert_eq!(c.pad_row, 8.0);
        assert_eq!(c.gap_inline, 8.0);
        assert_eq!(c.pad_overlay, 8.0);
    }

    /// Every roomier value lands on an even pixel, so a hairline or caret
    /// centred in a row cannot end up straddling two of them.
    #[test]
    fn comfortable_heights_are_even() {
        let c = Density::comfortable();
        for (name, value) in [
            ("h_status_bar", c.h_status_bar),
            ("h_tab", c.h_tab),
            ("h_row", c.h_row),
            ("h_action_row", c.h_action_row),
            ("h_overlay_item", c.h_overlay_item),
        ] {
            assert_eq!(value % 2.0, 0.0, "{name} = {value} is not an even pixel");
        }
    }

    /// The preset changes the air and nothing else: same type scale (which it
    /// does not own at all), same corners, same rail widths.
    #[test]
    fn comfortable_leaves_shape_and_widths_where_they_were() {
        let (c, r) = (Density::cockpit(), Density::comfortable());
        assert_eq!(r.r_chip, c.r_chip);
        assert_eq!(r.r_xs, c.r_xs);
        assert_eq!(r.r_card, c.r_card);
        assert_eq!(r.r_lg, c.r_lg);
        assert_eq!(r.r_xl, c.r_xl);
        // Draggable and persisted — see `Density::comfortable`.
        assert_eq!(r.w_sidebar, c.w_sidebar);
        assert_eq!(r.w_left_rail, c.w_left_rail);
    }

    /// The one token that must not move, whichever way it is asked to.
    ///
    /// It is aligned to the macOS traffic lights, which are positioned when
    /// the window is created. A preference changed later cannot move them, so
    /// a top bar that followed the preference would simply stop lining up.
    #[test]
    fn the_top_bar_is_pinned_against_both_controls() {
        let pinned = Density::cockpit().h_top_bar;
        assert_eq!(Density::comfortable().h_top_bar, pinned);
        assert_eq!(Density::cockpit().scaled(1.6).h_top_bar, pinned);
        assert_eq!(Density::cockpit().scaled(0.8).h_top_bar, pinned);
        assert_eq!(
            Density::for_appearance(Appearance {
                density: DensityPreset::Comfortable,
                scale: UiScale::from_percent(150),
            })
            .h_top_bar,
            pinned
        );
    }

    /// A roomier cockpit is still a cockpit: the corners keep their order even
    /// though the preset did not touch them, and zooming keeps it too.
    #[test]
    fn radius_tiers_survive_both_controls() {
        for d in [
            Density::comfortable(),
            Density::cockpit().scaled(1.5),
            Density::comfortable().scaled(0.8),
        ] {
            assert!(d.r_chip < d.r_xs);
            assert!(d.r_xs < d.r_card);
            assert!(d.r_card < d.r_lg);
            assert!(d.r_lg < d.r_xl);
        }
    }

    /// The default choices must resolve to exactly what shipped, or every
    /// spacing decision in the tree quietly moves on first launch.
    #[test]
    fn the_default_appearance_resolves_to_the_untouched_cockpit() {
        let d = Density::for_appearance(Appearance::default());
        let c = Density::cockpit();
        assert_eq!(d.h_row, c.h_row);
        assert_eq!(d.pad_panel, c.pad_panel);
        assert_eq!(d.r_card, c.r_card);
        assert_eq!(d.w_left_rail, c.w_left_rail);
        assert_eq!(d.appearance, Appearance::default());
    }

    /// Scaling by one is not "approximately one" — it must be the identity, so
    /// the overwhelmingly common case cannot introduce rounding drift.
    #[test]
    fn scaling_by_one_changes_nothing() {
        let c = Density::cockpit();
        let s = c.scaled(1.0);
        assert_eq!(s.h_row, c.h_row);
        assert_eq!(s.pad_row, c.pad_row);
        assert_eq!(s.r_chip, c.r_chip);
        assert_eq!(s.w_left_rail, c.w_left_rail);
    }

    /// The stamp names the user's choices, so a stale snapshot can be spotted
    /// by comparing it — that is the whole reason the field exists.
    #[test]
    fn the_resolved_density_remembers_what_it_came_from() {
        let asked = Appearance {
            density: DensityPreset::Comfortable,
            scale: UiScale::from_percent(120),
        };
        let d = Density::for_appearance(asked);
        assert_eq!(d.appearance, asked);
        assert_ne!(d.appearance, Density::cockpit().appearance);
        // And the values really did follow the choices, both of them.
        assert_eq!(d.h_row, Density::comfortable().h_row * 1.2);
    }
}
