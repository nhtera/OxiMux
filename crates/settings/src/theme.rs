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
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::charcoal()
    }
}
