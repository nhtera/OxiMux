//! Source Control panel style tokens, resolved from the user's appearance.
//!
//! The Source Control surface intentionally runs a touch looser than the
//! global cockpit density: 12px horizontal padding instead of 8, 12px body
//! text instead of 11. That intent is preserved here — but as a *ratio* to a
//! [`Density`] / [`Typography`] token rather than a literal, so the panel
//! follows both appearance controls instead of standing still while the rest
//! of the cockpit moves.
//!
//! # Why a resolved struct and not `const`
//!
//! These were bare constants until a live pass at 120% zoom caught the
//! consequence: every other surface grew and the SCM panel did not, so its
//! 12px rows sat inside chrome sized for 18px type. A helper that took a
//! `Density` and ignored it stood in for the conversion; this is the
//! conversion.
//!
//! Everything moves together on purpose. Scaling the padding alone would grow
//! the panel's horizontal air at 150% while row heights stayed put, which
//! reads worse than a panel that is uniformly unscaled — so the whole module
//! converts in one step or not at all.
//!
//! # Deriving, not re-listing
//!
//! Each field below names the token it comes from. Two rules decide which:
//!
//! * A value with a real scale to belong to — a type size, a padding, a row
//!   height — takes the token, so it follows *both* the density preset and
//!   the zoom.
//! * A value that is just how big a thing is — a 14px glyph, a 2px gap inside
//!   an icon cluster — passes through [`Density::scale`], which applies the
//!   zoom but deliberately not the preset. See that method for why inventing
//!   a token nobody else shares would be worse.

use oximux_settings::{Density, Typography};

/// Line-height ratio for the panel's body type: 12px text on 16px leading,
/// which is what `text-xs` means in the reference layout. Used to size the
/// commit composer from the type rather than from a remembered pixel count.
const BODY_LINE_HEIGHT: f32 = 4.0 / 3.0;

/// Hairline border width. Named so the composer's height arithmetic reads as
/// "two lines, padding, and the box around them".
const BORDER: f32 = 1.0;

/// The SCM panel's spacing and type, resolved for one appearance.
///
/// `Copy` and all-`f32`, so free render helpers take it by value instead of
/// threading a `Density` and a `&Typography` separately.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScmStyle {
    /// Outer horizontal padding for tabs, toolbar, filter row, commit area,
    /// and section headers. `1.5×` the cockpit panel padding — the ratio the
    /// shipped 12px already sat at against 8px, so this is a no-op at the
    /// default and follows both controls everywhere else.
    pub pad_h: f32,
    /// Vertical padding for the branch-compare toolbar.
    pub pad_v: f32,
    /// Vertical padding for the filter row and other tight stacks.
    pub pad_v_tight: f32,
    /// Fixed row height for the scope-tabs strip. Definite height keeps the
    /// row from being compressed by flex pressure when the file list expands
    /// below. Combined with `items_end`, the active tab's underline lands on
    /// the row's bottom border so the two lines unify visually.
    pub tab_h: f32,
    /// Row height for the branch-compare toolbar.
    pub toolbar_h: f32,
    /// Minimum row height for one commit in the graph list. Lets rows with a
    /// ref-badge sub-row (e.g. "(main)") grow taller, while non-ref rows stay
    /// compact. The timeline column distributes connector lines across this
    /// height; without a definite parent height the lines collapse to 2-3px.
    pub commit_row_h: f32,
    /// Commit message textarea height. A compact ~2-line composer that
    /// scrolls for longer bodies rather than ballooning the panel — keeps the
    /// file list and graph above the fold on a typical 13" display.
    pub commit_h: f32,
    /// Primary text size: tabs, toolbar copy, filter input, file rows, commit
    /// subject placeholder, graph subject lines.
    ///
    /// **Why not `t_body_sm` (11px)?** The SCM panel intentionally runs a
    /// notch larger so file names and commit subjects scan from arm's length
    /// while the operator works the cockpit — the file explorer and terminal
    /// stay at 11px.
    pub body_text: f32,
    /// Metadata text for parent paths and graph author/date columns.
    pub graph_meta_text: f32,
    /// Uppercase section headers ("STAGED CHANGES", "GRAPH").
    pub caps_text: f32,
    /// Small fixed annotations hanging off a row: the conflict-kind sub-label
    /// on unmerged files (e.g. "both modified") and the short OID in the graph.
    /// Both read as parentheticals to the row they sit beside.
    pub sub_label_text: f32,
    /// The panel's corner radius. One value: every rounded thing here is a
    /// control or a badge, and the panel has no surface large enough to want
    /// a second tier.
    pub corner: f32,
    /// The smallest text the panel uses — the scope-tab count badge, the AI
    /// agent name under the generate button, the subject character counter.
    /// Below `sub_label_text` because none of these is read, only glanced at.
    pub micro_text: f32,
    /// Inline icon size for the toolbar / filter / split-button chevron. The
    /// reference uses Lucide `size-3.5`, which is 14px.
    pub icon: f32,
    /// Horizontal gap between the `+A` (green) and `-B` (red) line-count
    /// fragments beside a file name. Tight enough that the pair reads as one
    /// decoration, loose enough that the minus sign doesn't blur into the
    /// digits before it.
    pub line_count_gap: f32,
    /// Tight gap inside dense icon-button clusters (toolbar view-mode group,
    /// graph commit-row actions, commit-area trailing icons). Smaller than
    /// `density.gap_inline` because icon buttons inside a cluster need to read
    /// as one element row, not three separate buttons.
    pub icon_cluster_gap: f32,
}

impl ScmStyle {
    /// Resolve the panel's style for one appearance.
    pub fn new(density: Density, typography: &Typography) -> Self {
        let pad_v_tight = density.pad_row;
        let body_text = typography.t_body_base;
        Self {
            pad_h: density.pad_panel * 1.5,
            pad_v: density.pad_panel,
            pad_v_tight,
            // One token for all three chrome rows. They shipped as 32 / 32 /
            // 34 — a near-miss nobody chose — and `h_action_row` is what a row
            // of inline action buttons is measured by everywhere else in the
            // cockpit, which is exactly what the toolbar and the tab strip are.
            tab_h: density.h_action_row,
            toolbar_h: density.h_action_row,
            commit_row_h: density.h_action_row,
            commit_h: (body_text * BODY_LINE_HEIGHT + pad_v_tight + BORDER) * 2.0,
            body_text,
            graph_meta_text: typography.t_body_sm,
            caps_text: typography.t_label_caps,
            sub_label_text: typography.t_label_xs,
            corner: density.r_xs,
            micro_text: typography.t_sub_label,
            icon: density.scale(14.0),
            line_count_gap: density.scale(4.0),
            icon_cluster_gap: density.scale(2.0),
        }
    }
}

impl Default for ScmStyle {
    fn default() -> Self {
        Self::new(Density::cockpit(), &Typography::cockpit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::{Appearance, DensityPreset, UiScale};

    fn resolved(density: DensityPreset, percent: u16) -> ScmStyle {
        let appearance = Appearance {
            density,
            scale: UiScale::from_percent(percent),
            ..Appearance::default()
        };
        ScmStyle::new(
            Density::for_appearance(appearance),
            &Typography::for_appearance(appearance),
        )
    }

    // The conversion has to be invisible at the shipped default, or it is a
    // redesign wearing a refactor's clothes. Every value here is the literal
    // this module carried before it derived anything.
    #[test]
    fn the_default_matches_the_pixels_that_shipped() {
        let s = ScmStyle::default();
        assert_eq!(s.pad_h, 12.0);
        assert_eq!(s.pad_v, 8.0);
        assert_eq!(s.pad_v_tight, 6.0);
        assert_eq!(s.commit_row_h, 34.0);
        assert_eq!(s.commit_h, 46.0);
        assert_eq!(s.body_text, 12.0);
        assert_eq!(s.graph_meta_text, 11.0);
        assert_eq!(s.sub_label_text, 10.0);
        assert_eq!(s.icon, 14.0);
        assert_eq!(s.line_count_gap, 4.0);
        assert_eq!(s.icon_cluster_gap, 2.0);
    }

    // The two deliberate departures, pinned so they read as decisions rather
    // than drift: the chrome rows unify on `h_action_row`, and the section
    // headers take the caps token they were always describing.
    #[test]
    fn the_chrome_rows_share_one_height() {
        let s = ScmStyle::default();
        assert_eq!(s.tab_h, 34.0, "was 32, now h_action_row");
        assert_eq!(s.toolbar_h, s.tab_h);
        assert_eq!(s.commit_row_h, s.tab_h);
    }

    #[test]
    fn section_headers_take_the_caps_token() {
        assert_eq!(ScmStyle::default().caps_text, 10.5, "was 11");
    }

    // The bug this module was rewritten for: at 150% every value has to move,
    // because type moves and chrome sized for 12px type will not hold 18px.
    #[test]
    fn zoom_reaches_every_field() {
        let base = ScmStyle::default();
        let big = resolved(DensityPreset::Cockpit, 150);
        for (name, a, b) in [
            ("pad_h", base.pad_h, big.pad_h),
            ("pad_v", base.pad_v, big.pad_v),
            ("pad_v_tight", base.pad_v_tight, big.pad_v_tight),
            ("tab_h", base.tab_h, big.tab_h),
            ("toolbar_h", base.toolbar_h, big.toolbar_h),
            ("commit_row_h", base.commit_row_h, big.commit_row_h),
            ("commit_h", base.commit_h, big.commit_h),
            ("body_text", base.body_text, big.body_text),
            ("graph_meta_text", base.graph_meta_text, big.graph_meta_text),
            ("caps_text", base.caps_text, big.caps_text),
            ("sub_label_text", base.sub_label_text, big.sub_label_text),
            ("icon", base.icon, big.icon),
            ("line_count_gap", base.line_count_gap, big.line_count_gap),
            (
                "icon_cluster_gap",
                base.icon_cluster_gap,
                big.icon_cluster_gap,
            ),
        ] {
            assert!(b > a, "{name} did not grow at 150%: {a} -> {b}");
        }
    }

    // The composer holds two lines of body type plus its own padding. Assert
    // the relationship rather than a number, so the two cannot drift apart at
    // a zoom level nobody wrote a case for.
    #[test]
    fn the_composer_holds_two_lines_at_any_zoom() {
        for percent in [100, 120, 150, 200] {
            let s = resolved(DensityPreset::Cockpit, percent);
            let two_lines = s.body_text * BODY_LINE_HEIGHT * 2.0;
            assert!(
                s.commit_h > two_lines,
                "at {percent}%: {} does not clear two {} lines",
                s.commit_h,
                s.body_text
            );
        }
    }

    // A preset changes the air, not the type — so the padding and rows move
    // while the text sizes hold. This is the distinction that keeps Density
    // and Typography from being two names for one control.
    #[test]
    fn the_preset_moves_the_air_and_leaves_the_type() {
        let cockpit = ScmStyle::default();
        let roomy = resolved(DensityPreset::Comfortable, 100);
        assert!(roomy.pad_h > cockpit.pad_h);
        assert!(roomy.pad_v_tight > cockpit.pad_v_tight);
        assert!(roomy.tab_h > cockpit.tab_h);
        assert_eq!(roomy.body_text, cockpit.body_text);
        assert_eq!(roomy.caps_text, cockpit.caps_text);
        assert_eq!(roomy.icon, cockpit.icon, "a glyph is not spacing");
    }
}
