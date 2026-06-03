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

    // Foregrounds
    pub fg_base: Hsla,
    pub fg_muted: Hsla,
    pub fg_subtle: Hsla,

    // Borders / focus / selection
    pub border_inactive: Hsla,
    pub border_active: Hsla,
    pub selection: Hsla,
    pub focus_ring: Hsla,

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

            fg_base: rgb(0xE6E8EB).into(),
            fg_muted: rgb(0x9AA0A6).into(),
            fg_subtle: rgb(0x6B7177).into(),

            border_inactive: rgb(0x26292E).into(),
            border_active: rgb(0x3A4047).into(),
            selection: rgb(0x2D3A4D).into(),
            focus_ring: rgb(0x4A6E9C).into(),

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
    /// `git.added` (#81b88b) at 16% alpha — the line scans as "added" while
    /// syntax/text reads cleanly through. Tuned up from a lighter-surface
    /// 10% baseline so the wash carries equal presence over OxiMux's darker
    /// charcoal base. Single source of truth so retuning lands in one place.
    pub fn diff_added_bg(&self) -> Hsla {
        Hsla { a: 0.16, ..self.git.added }
    }

    /// Translucent background for removed-line highlights — `git.deleted`
    /// (#c74e39) at 17% alpha (paired one notch above `diff_added_bg` since
    /// the brick hue reads slightly weaker than the sage at equal alpha).
    pub fn diff_removed_bg(&self) -> Hsla {
        Hsla { a: 0.17, ..self.git.deleted }
    }

    /// Brighter background for the changed *words* on a modified line,
    /// layered over the preserved syntax foreground so the exact change
    /// pops without recoloring the text. Stronger than the line tint.
    pub fn diff_word_added_bg(&self) -> Hsla {
        Hsla { a: 0.30, ..self.git.added }
    }

    /// Removed-side counterpart of `diff_word_added_bg`.
    pub fn diff_word_removed_bg(&self) -> Hsla {
        Hsla { a: 0.40, ..self.git.deleted }
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
    fn git_decorations_ignored_matches_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(t.git.ignored, t.fg_subtle);
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
    fn diff_added_bg_uses_16pct_alpha_over_git_added() {
        // 16% alpha over the git.added sage hue.
        let t = Theme::charcoal();
        let bg = t.diff_added_bg();
        assert!((bg.a - 0.16).abs() < f32::EPSILON);
        assert_eq!(bg.h, t.git.added.h);
        assert_eq!(bg.s, t.git.added.s);
        assert_eq!(bg.l, t.git.added.l);
    }

    #[test]
    fn diff_removed_bg_uses_17pct_alpha_over_git_deleted() {
        // 17% alpha over the git.deleted brick hue.
        let t = Theme::charcoal();
        let bg = t.diff_removed_bg();
        assert!((bg.a - 0.17).abs() < f32::EPSILON);
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
