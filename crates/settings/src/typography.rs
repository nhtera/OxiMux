//! Typography scale — sizes, weights, font stacks.
//!
//! Sizes are CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

use gpui::{Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, SharedString};

#[derive(Debug, Clone)]
pub struct Typography {
    // Sizes
    /// Sub-label text — metadata annotations that read subordinate to
    /// `t_label_xs`. Used for the tiny "vs main · 3 commits" sub-row
    /// under workspace cards in the left rail and similar secondary
    /// labels. Replaces hand-coded `t_body_sm * 0.85` arithmetic.
    pub t_sub_label: f32,
    pub t_label_xs: f32,
    pub t_label_caps: f32,
    pub t_body_sm: f32,
    pub t_brand: f32,
    pub t_body_md: f32,
    pub t_body_lg: f32,

    // Weights
    pub w_regular: FontWeight,
    pub w_medium: FontWeight,
    pub w_semibold: FontWeight,

    // Font families. `family_mono` / `family_ui` are the *primary* family
    // name passed to GPUI's `font_family()`. GPUI does NOT parse CSS-style
    // comma fallback lists — it looks the string up verbatim. Fallbacks
    // are held separately in `mono_fallbacks` / `ui_fallbacks` and applied
    // via `.font(font_mono(...))` at the call site, which threads them
    // into GPUI's `FontFallbacks` resolution chain.
    pub family_mono: SharedString,
    pub family_ui: SharedString,
    pub mono_fallbacks: Vec<SharedString>,
    pub ui_fallbacks: Vec<SharedString>,
}

/// The faces each platform is guaranteed to ship.
///
/// GPUI looks `Font::family` up verbatim, and `FontFallbacks` only cascades
/// for *individual glyph* lookups inside a family that already loaded — a
/// primary which fails to resolve at all drops to the platform's default UI
/// face, which is proportional. In the terminal that is not a subtle
/// degradation: `terminal_canvas` pins every glyph to a `cell_width`
/// measured from `'m'`, so a proportional fallback leaves narrow characters
/// (`i`, `l`, `1`, `:`) left-aligned in a cell sized for the widest
/// lowercase glyph, and the whole grid reads as randomly spaced. The primary
/// therefore has to be a face the OS always has.
#[cfg(target_os = "macos")]
mod platform_fonts {
    /// Menlo is the only mono face guaranteeable on every macOS 13+ install
    /// (Geist Mono is opt-in, and "SF Mono" registers as `.SF NS Mono` /
    /// `SFMono-Regular`, which font-kit's family selector misses). It
    /// carries the full Block Elements range (U+2580–259F) and Box Drawing
    /// (U+2500–257F), which half-block pixel art needs — Claude Code's
    /// mascot is the canonical regression case.
    pub const MONO: &str = "Menlo";
    pub const MONO_FALLBACKS: &[&str] = &["SF Mono", "Monaco"];
    pub const UI: &str = "Helvetica Neue";
    pub const UI_FALLBACKS: &[&str] = &["Helvetica"];
}

#[cfg(target_os = "windows")]
mod platform_fonts {
    /// Consolas ships with every Windows since Vista. It covers Box Drawing
    /// in full, and of Block Elements it carries exactly the eight that
    /// half-block rendering uses (▀ ▄ █ ▌ ▐ ░ ▒ ▓) — the 24 it lacks are the
    /// eighth-fraction blocks that sparkline-style output wants. Cascadia
    /// Mono covers those plus Braille, but only arrives with Windows
    /// Terminal rather than with Windows, so it sits in the fallback slot
    /// where being absent costs nothing.
    pub const MONO: &str = "Consolas";
    pub const MONO_FALLBACKS: &[&str] = &["Cascadia Mono", "Segoe UI Symbol"];
    /// Segoe UI is what the Helvetica Neue lookup was already landing on by
    /// accident. Naming it is a no-op visually and stops the UI chrome from
    /// depending on where GPUI's default happens to point.
    pub const UI: &str = "Segoe UI";
    pub const UI_FALLBACKS: &[&str] = &["Tahoma"];
}

/// Not a platform we ship, but the crate should still build and render
/// something monospaced if someone compiles for it.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform_fonts {
    pub const MONO: &str = "DejaVu Sans Mono";
    pub const MONO_FALLBACKS: &[&str] = &["Liberation Mono", "Noto Sans Mono"];
    pub const UI: &str = "DejaVu Sans";
    pub const UI_FALLBACKS: &[&str] = &["Liberation Sans"];
}

impl Typography {
    pub fn cockpit() -> Self {
        Self {
            t_sub_label: 9.5,
            t_label_xs: 10.0,
            t_label_caps: 10.5,
            t_body_sm: 11.0,
            t_brand: 12.0,
            t_body_md: 13.0,
            t_body_lg: 14.0,

            w_regular: FontWeight::NORMAL,
            w_medium: FontWeight::MEDIUM,
            w_semibold: FontWeight::SEMIBOLD,

            // Per-platform because the primary MUST resolve — see
            // `platform_fonts` for why a missing primary degrades the
            // terminal grid rather than just swapping a typeface. The
            // fallbacks are glyph-coverage backups only.
            family_mono: platform_fonts::MONO.into(),
            family_ui: platform_fonts::UI.into(),
            mono_fallbacks: platform_fonts::MONO_FALLBACKS
                .iter()
                .map(|name| (*name).into())
                .collect(),
            ui_fallbacks: platform_fonts::UI_FALLBACKS
                .iter()
                .map(|name| (*name).into())
                .collect(),
        }
    }

    /// Build a GPUI `Font` for terminal/editor surfaces with the configured
    /// fallback chain. Call sites use `.font(typography.mono_font())` instead
    /// of `.font_family(...)` so a missing primary (e.g. Geist Mono not
    /// installed) cascades through `mono_fallbacks` before GPUI's built-in
    /// system default kicks in.
    pub fn mono_font(&self) -> Font {
        Font {
            family: self.family_mono.clone(),
            // Ligatures off: terminals must show literal byte sequences.
            // `->` should be two characters, not `→`; `==` two equals, not
            // `⩵`. Menlo doesn't ligature anyway, but locking this in keeps
            // behavior correct if the user later swaps to a ligature font
            // (JetBrains Mono, Fira Code, etc.) — terminals disable
            // ligatures so glyph cells stay 1:1 with character cells.
            features: FontFeatures::disable_ligatures(),
            fallbacks: Some(FontFallbacks::from_fonts(
                self.mono_fallbacks.iter().map(|s| s.to_string()).collect(),
            )),
            weight: self.w_regular,
            style: FontStyle::Normal,
        }
    }

    /// UI-chrome counterpart of `mono_font` for sidebar, top bar, etc.
    pub fn ui_font(&self) -> Font {
        Font {
            family: self.family_ui.clone(),
            features: FontFeatures::default(),
            fallbacks: Some(FontFallbacks::from_fonts(
                self.ui_fallbacks.iter().map(|s| s.to_string()).collect(),
            )),
            weight: self.w_regular,
            style: FontStyle::Normal,
        }
    }
}

impl Default for Typography {
    fn default() -> Self {
        Self::cockpit()
    }
}
