//! Theme tokens (charcoal palette, dark only in v1).
//!
//! Hex values are owned by `docs/design-guidelines.md`. Keep this file and
//! that doc in sync — the doc is the contract.

use gpui::{Hsla, rgb};

/// Resolved theme handed to every view. Built once at startup and stashed in
/// a global state context.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // Backgrounds
    pub bg_base: Hsla,
    pub bg_panel: Hsla,
    pub bg_panel_alt: Hsla,
    pub bg_overlay: Hsla,

    /// Left-rail surface — deliberately LIGHTER than `bg_panel` so the
    /// rail reads as a raised, intentional slab beside the near-black
    /// content canvas (terminal/diff). Without the lift, the rail's
    /// empty tail under the project list reads as dead space instead of
    /// surface. Rail-scoped on purpose: re-leveling `bg_panel` itself
    /// would flatten the global panel < panel_alt < overlay ladder that
    /// every other surface depends on.
    pub bg_rail: Hsla,

    // Foregrounds
    pub fg_base: Hsla,
    pub fg_muted: Hsla,
    pub fg_subtle: Hsla,

    // Borders / focus / selection
    pub border_inactive: Hsla,
    pub border_active: Hsla,
    pub selection: Hsla,
    pub focus_ring: Hsla,

    /// Resting border for text inputs — alpha-white like `border_inactive`
    /// but strong enough to read as an affordance edge ("type here"), not
    /// just a divider. The FOCUSED state keeps its existing solid
    /// treatment so focus stays unambiguous.
    pub border_input: Hsla,

    /// Transient hover fill for interactive rows, cards, and menu items.
    /// White at low alpha so the same token composites into a uniform
    /// perceived brightness step over ANY surface tier (`bg_base`,
    /// `bg_panel`, `bg_overlay`) — a flat hex swap reads correct on one
    /// layer and inverted on a lighter one (it DARKENED rows on overlay
    /// cards). Hover only; persistent fills (selected row, nested panel)
    /// stay on `bg_panel_alt` so hover and selection remain two visibly
    /// different states.
    pub hover_overlay: Hsla,

    /// Non-interactive 1px top edge painted on floating surfaces (popovers,
    /// pickers, dialogs, menus, toasts). White at very low alpha so it reads
    /// as a physical edge catching light — conveying elevation without a
    /// shadow or blur. Kept under ~6% or it stops reading as a hint and
    /// becomes a drawn line.
    pub edge_highlight: Hsla,

    // Search match highlight. `current` is the cycled / "you are here" match
    // (bright amber, dark fg for high contrast). `other` is every other
    // match in scrollback (dim amber, default fg) — visible enough to scan
    // but de-emphasized so the eye finds `current` first.
    pub match_bg_current: Hsla,
    pub match_bg_other: Hsla,
    pub match_fg: Hsla,

    // Status palette (single accent layer)
    pub status_ok: Hsla,
    pub status_warn: Hsla,
    pub status_error: Hsla,
    pub status_info: Hsla,
    pub status_muted: Hsla,

    // SCM diff/banner palette. Intentionally distinct from `status_warn`
    // (which is a general-purpose hazard amber): `status_warning` is the
    // softer banner amber used by the conflict / in-progress operation
    // strips and reads as "ongoing state" rather than "alert". The
    // `_added` / `_removed` pair is consumed by the file-row +N / -N
    // diff counts and the diff-view gutter — kept off `git.added` /
    // `git.deleted` so SCM text contrast can move independently of the
    // muted file-explorer badge palette.
    pub status_added: Hsla,
    pub status_removed: Hsla,
    pub status_warning: Hsla,

    /// Commit-graph lane palette — a colour-blind-safe 5-hue cycle for
    /// multi-lane DAG rendering (lane index % 5). The commit timeline is a
    /// single flat lane today (dot = `focus_ring`); these tokens exist so
    /// lane work lands on a stable palette instead of inventing hues.
    pub graph_lane_colors: [Hsla; 5],

    /// Git status decoration palette — used by the workspace card status
    /// dot in the left rail and (later) the file explorer status badges.
    pub git: GitDecorations,
}

/// Per-status colors for git-decorated UI (workspace card dot,
/// file-explorer badges, diff markers).
#[derive(Debug, Clone, Copy)]
pub struct GitDecorations {
    pub modified: Hsla,
    pub added: Hsla,
    pub deleted: Hsla,
    pub renamed: Hsla,
    pub untracked: Hsla,
    pub copied: Hsla,
    pub ignored: Hsla,
}

impl Theme {
    /// The one OxiMux theme: monochrome charcoal.
    pub fn charcoal() -> Self {
        Self {
            bg_base: rgb(0x0E0F11).into(),
            bg_panel: rgb(0x15171A).into(),
            bg_panel_alt: rgb(0x1B1E22).into(),
            bg_overlay: rgb(0x22262B).into(),

            // Between `bg_panel_alt` and `bg_overlay`: high enough to
            // clearly separate from the content canvas, low enough that
            // overlay cards still float above it.
            bg_rail: rgb(0x1D2024).into(),

            fg_base: rgb(0xE6E8EB).into(),
            fg_muted: rgb(0x9AA0A6).into(),
            fg_subtle: rgb(0x6B7177).into(),

            // Dividers and inactive card/panel edges are white at low alpha
            // rather than a solid grey hex. An alpha-over-white edge composites
            // against whatever background sits under it (`bg_base`, `bg_panel`,
            // `bg_panel_alt`), so the same token reads as a hairline catching
            // light on every layer instead of a flat drawn line. Tuned to the
            // lowest alpha that stays visible on near-black `bg_base` (the worst
            // case). `border_active` stays a solid hex below so focus/selection
            // edges remain unambiguous against these faint dividers.
            border_inactive: Hsla { h: 0., s: 0., l: 1., a: 0.08 },
            border_active: rgb(0x3A4047).into(),
            selection: rgb(0x2D3A4D).into(),
            focus_ring: rgb(0x4A6E9C).into(),

            // Currently the same value as `edge_highlight` by coincidence,
            // not by contract — hover strength and edge-elevation strength
            // are independent knobs; tune them separately.
            hover_overlay: Hsla { h: 0., s: 0., l: 1., a: 0.06 },

            border_input: Hsla { h: 0., s: 0., l: 1., a: 0.15 },

            edge_highlight: Hsla { h: 0., s: 0., l: 1., a: 0.06 },

            match_bg_current: rgb(0xD9A441).into(),
            match_bg_other: rgb(0x5A5358).into(),
            match_fg: rgb(0x0E0F11).into(),

            status_ok: rgb(0x6FA86A).into(),
            status_warn: rgb(0xD9A441).into(),
            status_error: rgb(0xD26464).into(),
            status_info: rgb(0x5B97C9).into(),
            status_muted: rgb(0x6B7177).into(),

            // `status_removed` aliases `status_error` deliberately — one
            // canonical red prevents palette sprawl when "deletion" and
            // "destructive op" need to look identical.
            graph_lane_colors: [
                rgb(0xFFB000).into(),
                rgb(0xDC267F).into(),
                rgb(0x994F00).into(),
                rgb(0x40B0A6).into(),
                rgb(0xB66DFF).into(),
            ],

            status_added: rgb(0x64D26B).into(),
            status_removed: rgb(0xD26464).into(),
            status_warning: rgb(0xD2A864).into(),

            git: GitDecorations {
                modified: rgb(0xD9A441).into(),
                // Conventional dark-editor git-decoration palette — sage
                // green + brick red. Drives the diff gutter sliver, line
                // tints, and +N/-N chips.
                added: rgb(0x81B88B).into(),
                deleted: rgb(0xC74E39).into(),
                renamed: rgb(0x5B97C9).into(),
                untracked: rgb(0x9CC79A).into(),
                copied: rgb(0x4DA8A8).into(),
                ignored: rgb(0x6B7177).into(),
            },
        }
    }

    /// Translucent background for added-line highlights in diff views.
    /// `git.added` (#81b88b) at 20% alpha — the line scans clearly as
    /// "added" (a saturated two-tier wash like reference editors) while
    /// syntax/text still reads through. Single source of truth so retuning
    /// lands in one place; the changed-word box (`diff_word_added_bg`) must
    /// stay visibly stronger so the exact change still out-pops the line.
    pub fn diff_added_bg(&self) -> Hsla {
        Hsla { a: 0.20, ..self.git.added }
    }

    /// Translucent background for removed-line highlights — `git.deleted`
    /// (#c74e39) at 22% alpha (paired one notch above `diff_added_bg` since
    /// the brick hue reads slightly weaker than the sage at equal alpha).
    pub fn diff_removed_bg(&self) -> Hsla {
        Hsla { a: 0.22, ..self.git.deleted }
    }

    /// Brighter background for the changed *words* on a modified line,
    /// layered over the preserved syntax foreground so the exact change
    /// pops without recoloring the text. Tuned distinctly above the line
    /// tint for the two-tier look — the word box clearly out-pops the wash.
    pub fn diff_word_added_bg(&self) -> Hsla {
        Hsla { a: 0.40, ..self.git.added }
    }

    /// Removed-side counterpart of `diff_word_added_bg`. The brick hue reads
    /// weaker at equal alpha, so it runs hotter (still readable on charcoal).
    pub fn diff_word_removed_bg(&self) -> Hsla {
        Hsla { a: 0.60, ..self.git.deleted }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::charcoal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_decorations_modified_matches_status_warn() {
        let t = Theme::charcoal();
        assert_eq!(t.git.modified, t.status_warn);
    }

    #[test]
    fn git_decorations_added_deleted_are_diff_palette() {
        // git.added/deleted are the diff sage/brick used by the gutter
        // sliver, line tints, and stat chips — intentionally decoupled from
        // `status_ok` (general OK green).
        let t = Theme::charcoal();
        assert_eq!(t.git.added, rgb(0x81B88B).into());
        assert_eq!(t.git.deleted, rgb(0xC74E39).into());
        assert_ne!(t.git.added, t.status_ok);
    }

    #[test]
    fn graph_lane_colors_are_five_distinct_hues() {
        let t = Theme::charcoal();
        for (i, a) in t.graph_lane_colors.iter().enumerate() {
            for b in t.graph_lane_colors.iter().skip(i + 1) {
                assert_ne!(a, b, "lane palette has duplicate hues");
            }
        }
    }

    #[test]
    fn git_decorations_ignored_matches_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(t.git.ignored, t.fg_subtle);
    }

    #[test]
    fn hover_overlay_is_alpha_white() {
        // Hover must composite over any surface tier — a flat (opaque)
        // value regresses to the "darkens rows on overlay cards" bug the
        // token exists to fix. Pin white hue + sub-10% alpha.
        let t = Theme::charcoal();
        assert_eq!(t.hover_overlay.l, 1.0);
        assert_eq!(t.hover_overlay.s, 0.0);
        assert!(t.hover_overlay.a > 0.0 && t.hover_overlay.a < 0.10);
    }

    #[test]
    fn border_input_is_alpha_white() {
        // Resting input borders must composite over any host surface and
        // stay clearly stronger than the 8% hairline dividers.
        let t = Theme::charcoal();
        assert_eq!(t.border_input.l, 1.0);
        assert_eq!(t.border_input.s, 0.0);
        assert!(t.border_input.a > t.border_inactive.a);
        assert!(t.border_input.a < 0.25);
    }

    #[test]
    fn status_removed_aliases_status_error() {
        // `status_removed` is intentionally the same red as
        // `status_error` — one canonical destructive/danger color.
        let t = Theme::charcoal();
        assert_eq!(t.status_removed, t.status_error);
    }

    #[test]
    fn status_added_distinct_from_git_added() {
        // The SCM diff-count green is brighter than the muted
        // file-explorer badge green; they must NOT collapse to the same
        // token, or text contrast on the diff-count chips suffers.
        let t = Theme::charcoal();
        assert_ne!(t.status_added, t.git.added);
    }

    #[test]
    fn status_warning_distinct_from_status_warn() {
        // `status_warning` is the SCM banner amber (softer, "ongoing
        // state"); `status_warn` is the general hazard amber. Must
        // stay distinct so banner UI can tune contrast independently.
        let t = Theme::charcoal();
        assert_ne!(t.status_warning, t.status_warn);
    }

    #[test]
    fn diff_added_bg_uses_20pct_alpha_over_git_added() {
        // 20% alpha over the git.added sage hue (two-tier wash).
        let t = Theme::charcoal();
        let bg = t.diff_added_bg();
        assert!((bg.a - 0.20).abs() < f32::EPSILON);
        assert_eq!(bg.h, t.git.added.h);
        assert_eq!(bg.s, t.git.added.s);
        assert_eq!(bg.l, t.git.added.l);
    }

    #[test]
    fn diff_removed_bg_uses_22pct_alpha_over_git_deleted() {
        // 22% alpha over the git.deleted brick hue.
        let t = Theme::charcoal();
        let bg = t.diff_removed_bg();
        assert!((bg.a - 0.22).abs() < f32::EPSILON);
        assert_eq!(bg.h, t.git.deleted.h);
        assert_eq!(bg.s, t.git.deleted.s);
        assert_eq!(bg.l, t.git.deleted.l);
    }

    #[test]
    fn diff_word_bgs_are_stronger_than_line_tints() {
        // Changed-word backgrounds must out-weigh the line tint so the
        // exact change pops over the softer whole-line wash.
        let t = Theme::charcoal();
        assert!(t.diff_word_added_bg().a > t.diff_added_bg().a);
        assert!(t.diff_word_removed_bg().a > t.diff_removed_bg().a);
    }
}
