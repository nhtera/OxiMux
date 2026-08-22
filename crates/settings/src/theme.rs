//! Theme tokens — two palettes, one shape.
//!
//! Hex values are owned by `docs/design-guidelines.md`. Keep this file and
//! that doc in sync — the doc is the contract.
//!
//! # Two palettes, and the tokens that cannot simply be inverted
//!
//! [`Theme::charcoal`] and [`Theme::paper`] fill the same struct, so no render
//! path branches on which one is in force. Most tokens are a straight
//! substitution. Three groups are not, and they are where a light theme
//! usually goes wrong:
//!
//! * **Alpha overlays.** `hover_overlay`, `border_inactive`, `border_input`
//!   are white at low alpha on charcoal, deliberately, so one token
//!   composites to the same perceived step over every surface tier. White
//!   over paper is invisible. They flip to black at alpha — and not at the
//!   same alpha, because a dark veil over white reads stronger than a light
//!   veil over black at equal opacity.
//! * **Elevation.** On charcoal a floating card is *lighter* than what it
//!   sits on, and `edge_highlight` is a white top rule reading as an edge
//!   catching light. On paper the canvas is already the lightest thing in the
//!   window; a card cannot get lighter, so elevation reads as a border and a
//!   shadow, and the highlight becomes a faint dark rule instead.
//! * **The surface ladder.** Charcoal runs darkest-canvas → lighter-chrome.
//!   Paper does not invert that into darkest-chrome: the content canvas is
//!   white (that is what a page of code should be), the chrome is a light
//!   grey *below* it, and overlays return to white. So `bg_rail` is lighter
//!   than `bg_panel` on charcoal and darker than it on paper — both meaning
//!   "this slab is distinct from the canvas beside it".

use gpui::{Hsla, rgb};

use crate::appearance::{Appearance, ThemeChoice};

/// Resolved theme handed to every view. Built once at startup and stashed in
/// a global state context.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Which palette this is.
    ///
    /// Carried so a surface that must branch on polarity can ask, rather than
    /// reaching for the `Appearance` global from a pure function that was
    /// handed a `Theme` and nothing else. The terminal's sixteen named colors
    /// are the case that needs it — see `terminal_palette`.
    pub choice: ThemeChoice,

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

    /// Token foregrounds for every code surface — chat fences today, the
    /// diff body once it migrates off its bundled `.tmTheme`.
    pub syntax: SyntaxPalette,
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

/// Token foregrounds, one per neutral highlight kind.
///
/// Named for what a span *is*, never for a grammar's scope string — the
/// highlighter hands out a small closed set of kinds precisely so a palette can
/// be written once instead of per language. Foreground only: a syntax color that
/// changed weight or size would change measured height, and highlighting is
/// computed off the layout path exactly because it cannot.
#[derive(Debug, Clone, Copy)]
pub struct SyntaxPalette {
    pub keyword: Hsla,
    pub function: Hsla,
    pub type_name: Hsla,
    pub string: Hsla,
    pub escape: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    pub constant: Hsla,
    pub operator: Hsla,
    pub punctuation: Hsla,
    pub variable: Hsla,
    pub attribute: Hsla,
    pub namespace: Hsla,
    pub tag: Hsla,
}

impl Theme {
    /// The palette a set of appearance choices resolves to.
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance.theme {
            ThemeChoice::Charcoal => Self::charcoal(),
            ThemeChoice::Paper => Self::paper(),
        }
    }

    /// The original OxiMux theme: monochrome charcoal.
    pub fn charcoal() -> Self {
        Self {
            choice: ThemeChoice::Charcoal,
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

            // The conventional dark-editor token hues the diff body already
            // draws (keyword blue, string orange, comment green), lifted off
            // the bundled `.tmTheme` and onto kinds so a palette change stops
            // meaning a re-tokenization. Keeping the values identical is what
            // lets a chat fence and a diff of the same file look like the same
            // editor.
            syntax: SyntaxPalette {
                keyword: rgb(0x569CD6).into(),
                function: rgb(0xDCDCAA).into(),
                type_name: rgb(0x4EC9B0).into(),
                string: rgb(0xCE9178).into(),
                escape: rgb(0xD7BA7D).into(),
                number: rgb(0xB5CEA8).into(),
                comment: rgb(0x6A9955).into(),
                constant: rgb(0x4FC1FF).into(),
                // Operators and punctuation stay at the code default rather
                // than taking a hue: coloring every `.`, `(` and `,` is what
                // makes a highlighted block read as noise.
                operator: rgb(0xD4D4D4).into(),
                punctuation: rgb(0xD4D4D4).into(),
                variable: rgb(0x9CDCFE).into(),
                attribute: rgb(0x9CDCFE).into(),
                namespace: rgb(0x4EC9B0).into(),
                tag: rgb(0x569CD6).into(),
            },
        }
    }

    /// The light counterpart: charcoal drawn on paper.
    ///
    /// Not an inversion. See the module docs for the three groups of token
    /// that cannot be flipped — the alpha overlays, the elevation cues, and
    /// the surface ladder — and why each one is the way it is here.
    pub fn paper() -> Self {
        Self {
            choice: ThemeChoice::Paper,
            // The content canvas is white, because that is what a page of
            // code should be, and the chrome sits a step *below* it. Overlays
            // return to white and lean on their border and shadow to float.
            bg_base: rgb(0xFFFFFF).into(),
            bg_panel: rgb(0xF4F5F7).into(),
            bg_panel_alt: rgb(0xE8EBEF).into(),
            bg_overlay: rgb(0xFFFFFF).into(),

            // Distinct from the canvas beside it, which on paper means a
            // touch darker rather than a touch lighter — see the module docs.
            bg_rail: rgb(0xEDEFF3).into(),

            // Near-black rather than black: pure #000 on pure #FFF is a
            // contrast the eye reads as a glare rather than as text.
            fg_base: rgb(0x1A1D21).into(),
            fg_muted: rgb(0x5C6570).into(),
            fg_subtle: rgb(0x8A929C).into(),

            // Black at alpha, the polarity flip of charcoal's white-at-alpha,
            // and at a lower number: a dark veil over white reads stronger
            // than a light veil over black at the same opacity.
            border_inactive: Hsla { h: 0., s: 0., l: 0., a: 0.10 },
            border_active: rgb(0xAEB6C0).into(),
            selection: rgb(0xD3E3F7).into(),
            focus_ring: rgb(0x2F6FB5).into(),

            hover_overlay: Hsla { h: 0., s: 0., l: 0., a: 0.05 },

            border_input: Hsla { h: 0., s: 0., l: 0., a: 0.18 },

            // Charcoal's white top rule reads as an edge catching light. On
            // paper nothing is lighter than the surface, so the same job
            // falls to a faint dark rule and the card's own shadow.
            edge_highlight: Hsla { h: 0., s: 0., l: 0., a: 0.05 },

            // The current match keeps its amber; the resting ones drop to a
            // pale wash, because on white a mid-tone fill reads as loud as
            // the highlight it is supposed to sit behind.
            match_bg_current: rgb(0xF2C14E).into(),
            match_bg_other: rgb(0xF0E6C8).into(),
            match_fg: rgb(0x1A1D21).into(),

            // Every status hue darkens: the charcoal set is tuned to carry on
            // near-black, and the same values on white are pastel.
            status_ok: rgb(0x2E7D32).into(),
            status_warn: rgb(0xB37400).into(),
            status_error: rgb(0xC0392B).into(),
            status_info: rgb(0x1F6FB2).into(),
            status_muted: rgb(0x8A929C).into(),

            // Same colour-blind-safe five hues, darkened to carry on white.
            graph_lane_colors: [
                rgb(0xB37400).into(),
                rgb(0xC2185B).into(),
                rgb(0x7A3E00).into(),
                rgb(0x00796B).into(),
                rgb(0x7B3FBF).into(),
            ],

            status_added: rgb(0x1E7A3C).into(),
            status_removed: rgb(0xC0392B).into(),
            status_warning: rgb(0xA97A20).into(),

            git: GitDecorations {
                modified: rgb(0xB37400).into(),
                added: rgb(0x2E7D32).into(),
                deleted: rgb(0xC0392B).into(),
                renamed: rgb(0x1F6FB2).into(),
                untracked: rgb(0x3E8E41).into(),
                copied: rgb(0x00796B).into(),
                ignored: rgb(0x8A929C).into(),
            },

            // The charcoal set is VS Code's Dark+ token hues; this is Light+,
            // deliberately, so the pair is the same well-worn relationship
            // rather than two independently invented palettes. A reader who
            // knows one editor's light theme already knows this one.
            syntax: SyntaxPalette {
                keyword: rgb(0x0000FF).into(),
                function: rgb(0x795E26).into(),
                type_name: rgb(0x267F99).into(),
                string: rgb(0xA31515).into(),
                escape: rgb(0xEE0000).into(),
                number: rgb(0x098658).into(),
                comment: rgb(0x008000).into(),
                constant: rgb(0x0070C1).into(),
                // Same reasoning as charcoal: operators and punctuation stay
                // at the code default rather than taking a hue.
                operator: rgb(0x1A1D21).into(),
                punctuation: rgb(0x1A1D21).into(),
                variable: rgb(0x001080).into(),
                attribute: rgb(0x001080).into(),
                namespace: rgb(0x267F99).into(),
                tag: rgb(0x800000).into(),
            },
        }
    }

    /// True when this palette paints dark text on a light ground.
    pub fn is_light(&self) -> bool {
        self.choice.is_light()
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
    // ---- Paper, and what has to stay true of both palettes ---------------

    /// The choices resolve to the palettes they name, and each palette knows
    /// which one it is — the terminal's named colors branch on it.
    #[test]
    fn a_choice_resolves_to_its_palette() {
        for choice in ThemeChoice::ALL {
            let t = Theme::for_appearance(Appearance {
                theme: choice,
                ..Appearance::default()
            });
            assert_eq!(t.choice, choice);
        }
        assert!(!Theme::charcoal().is_light());
        assert!(Theme::paper().is_light());
    }

    /// Text has to read on its own ground. The cheap version of a contrast
    /// check — the two palettes are near-neutral, so lightness stands in for
    /// luminance — but it is the check that catches a palette pasted in from
    /// the wrong polarity, which is the mistake worth catching automatically.
    #[test]
    fn each_palette_puts_its_text_at_the_far_end_from_its_ground() {
        let dark = Theme::charcoal();
        assert!(dark.bg_base.l < 0.15, "charcoal ground is dark");
        assert!(dark.fg_base.l > 0.85, "charcoal text is light");

        let light = Theme::paper();
        assert!(light.bg_base.l > 0.95, "paper ground is light");
        assert!(light.fg_base.l < 0.20, "paper text is dark");

        // And the muted tiers stay between the two, in order, or "muted"
        // stops meaning anything.
        for t in [dark, light] {
            let span = (t.fg_base.l - t.bg_base.l).abs();
            let muted = (t.fg_muted.l - t.bg_base.l).abs();
            let subtle = (t.fg_subtle.l - t.bg_base.l).abs();
            assert!(subtle < muted && muted < span, "fg tiers out of order");
        }
    }

    /// The alpha overlays are the tokens a light theme gets wrong. They are
    /// white over charcoal and must be black over paper — an unflipped
    /// overlay is not subtly wrong, it is invisible.
    #[test]
    fn alpha_overlays_flip_polarity() {
        let dark = Theme::charcoal();
        for (name, c) in [
            ("hover_overlay", dark.hover_overlay),
            ("border_inactive", dark.border_inactive),
            ("border_input", dark.border_input),
            ("edge_highlight", dark.edge_highlight),
        ] {
            assert_eq!(c.l, 1.0, "{name} should be white over charcoal");
            assert!(c.a > 0.0 && c.a < 1.0, "{name} should be an alpha veil");
        }

        let light = Theme::paper();
        for (name, c) in [
            ("hover_overlay", light.hover_overlay),
            ("border_inactive", light.border_inactive),
            ("border_input", light.border_input),
            ("edge_highlight", light.edge_highlight),
        ] {
            assert_eq!(c.l, 0.0, "{name} should be black over paper");
            assert!(c.a > 0.0 && c.a < 1.0, "{name} should be an alpha veil");
        }
    }

    /// Elevation reads in opposite directions, and the ladder is what says so.
    ///
    /// Charcoal lifts each tier lighter. Paper cannot: the canvas is already
    /// the lightest thing on screen, so chrome sits below it and a floating
    /// card returns to the canvas value and leans on its border instead.
    #[test]
    fn the_surface_ladder_runs_the_way_each_palette_needs() {
        let dark = Theme::charcoal();
        assert!(dark.bg_base.l < dark.bg_panel.l);
        assert!(dark.bg_panel.l < dark.bg_panel_alt.l);
        assert!(dark.bg_panel_alt.l < dark.bg_overlay.l);
        assert!(dark.bg_rail.l > dark.bg_panel.l, "rail lifts off the panel");

        let light = Theme::paper();
        assert!(light.bg_base.l > light.bg_panel.l, "chrome sits below the canvas");
        assert!(light.bg_panel.l > light.bg_panel_alt.l);
        assert_eq!(light.bg_overlay.l, light.bg_base.l, "a card returns to canvas white");
        assert!(light.bg_rail.l < light.bg_panel.l, "rail settles under the panel");
    }

    /// A status hue exists to be noticed. On paper the charcoal values are
    /// pastel, so every one of them has to have come down.
    #[test]
    fn paper_darkens_every_accent_it_inherited() {
        let (dark, light) = (Theme::charcoal(), Theme::paper());
        for (name, d, l) in [
            ("status_ok", dark.status_ok, light.status_ok),
            ("status_warn", dark.status_warn, light.status_warn),
            ("status_error", dark.status_error, light.status_error),
            ("status_info", dark.status_info, light.status_info),
            ("status_added", dark.status_added, light.status_added),
            ("status_removed", dark.status_removed, light.status_removed),
            ("git.added", dark.git.added, light.git.added),
            ("git.deleted", dark.git.deleted, light.git.deleted),
            ("git.modified", dark.git.modified, light.git.modified),
        ] {
            assert!(l.l < d.l, "{name} must darken for paper ({} -> {})", d.l, l.l);
            assert!(l.l < 0.5, "{name} must carry on white (l = {})", l.l);
        }
        for (i, lane) in light.graph_lane_colors.iter().enumerate() {
            assert!(lane.l < 0.5, "graph lane {i} must carry on white");
        }
    }

    /// Every syntax kind has to read on its own ground too — a token palette
    /// lifted from the wrong polarity is the most obvious tell there is.
    #[test]
    fn syntax_tokens_carry_on_their_own_ground() {
        for (t, lo, hi) in [(Theme::charcoal(), 0.4, 1.0), (Theme::paper(), 0.0, 0.55)] {
            let s = t.syntax;
            for (name, c) in [
                ("keyword", s.keyword),
                ("function", s.function),
                ("type_name", s.type_name),
                ("string", s.string),
                ("escape", s.escape),
                ("number", s.number),
                ("comment", s.comment),
                ("constant", s.constant),
                ("operator", s.operator),
                ("punctuation", s.punctuation),
                ("variable", s.variable),
                ("attribute", s.attribute),
                ("namespace", s.namespace),
                ("tag", s.tag),
            ] {
                assert!(
                    c.l >= lo && c.l <= hi,
                    "{name} at l = {} is outside {lo}..{hi} for {:?}",
                    c.l,
                    t.choice
                );
            }
        }
    }

    /// The diff washes are derived, so they follow the palette for free — but
    /// only if the derivation stays alpha-over-hue rather than a second hex.
    #[test]
    fn the_diff_washes_follow_whichever_palette_they_came_from() {
        for t in [Theme::charcoal(), Theme::paper()] {
            assert_eq!(t.diff_added_bg().h, t.git.added.h);
            assert_eq!(t.diff_removed_bg().h, t.git.deleted.h);
            assert!(t.diff_word_added_bg().a > t.diff_added_bg().a);
        }
    }
}
