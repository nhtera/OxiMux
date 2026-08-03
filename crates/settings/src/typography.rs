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
/// GPUI looks `Font::family` up verbatim, and `FontFallbacks` only cascades for
/// *individual glyph* lookups inside a family that already loaded. It does not
/// rescue a primary that fails to resolve at all — that goes to
/// `TextSystem::resolve_font`, which walks a hardcoded stack inside gpui whose
/// only monospace entry is the `.ZedMono` sentinel. So the primary should still
/// be a face the OS always has: a miss means the grid is drawn in a typeface
/// nobody chose, and `terminal_canvas` pins every glyph to a `cell_width`
/// measured from `'m'`, so any width mismatch shows up as uneven spacing.
///
/// `apps/desktop` bundles Lilex, which is what `.ZedMono` resolves to, so the
/// floor under that path is at least monospace rather than the proportional
/// `Segoe UI` it used to land on. Keeping the platform primary correct is still
/// the first line of defence; the bundled font is the net, not the plan.
///
/// Lilex is last in every `MONO_FALLBACKS` for the *other* reason — glyphs
/// missing from a family that did load. It maps all 32 Block Elements at its
/// ASCII advance, so it backstops Consolas's 8-of-32.
#[cfg(target_os = "macos")]
mod platform_fonts {
    /// Menlo is the only mono face guaranteeable on every macOS 13+ install
    /// (Geist Mono is opt-in, and "SF Mono" registers as `.SF NS Mono` /
    /// `SFMono-Regular`, which font-kit's family selector misses). It
    /// carries the full Block Elements range (U+2580–259F) and Box Drawing
    /// (U+2500–257F), which half-block pixel art needs — Claude Code's
    /// mascot is the canonical regression case.
    pub const MONO: &str = "Menlo";
    pub const MONO_FALLBACKS: &[&str] = &["SF Mono", "Monaco", "Lilex"];
    pub const UI: &str = "Helvetica Neue";
    pub const UI_FALLBACKS: &[&str] = &["Helvetica"];
}

#[cfg(target_os = "windows")]
mod platform_fonts {
    /// Consolas ships with every Windows since Vista. It covers Box Drawing in
    /// full, and of Block Elements it carries exactly the eight that half-block
    /// rendering uses (▀ ▄ █ ▌ ▐ ░ ▒ ▓) — the 24 it lacks are the
    /// eighth-fraction blocks that sparkline-style output wants. Bundled Lilex
    /// covers those (32/32), so unlike the other two fallbacks it is not a
    /// maybe. Cascadia Mono is still worth naming ahead of it for Braille,
    /// which neither Consolas nor Lilex has, but it arrives with Windows
    /// Terminal rather than with Windows.
    pub const MONO: &str = "Consolas";
    pub const MONO_FALLBACKS: &[&str] = &["Cascadia Mono", "Segoe UI Symbol", "Lilex"];
    /// Segoe UI is what the Helvetica Neue lookup was already landing on by
    /// accident. Naming it is a no-op visually and stops the UI chrome from
    /// depending on where GPUI's default happens to point.
    pub const UI: &str = "Segoe UI";
    pub const UI_FALLBACKS: &[&str] = &["Tahoma"];
}

/// Not a platform we ship, but the crate should still build and render
/// something monospaced if someone compiles for it. Unlike macOS and Windows
/// there is no face a Linux install is *guaranteed* to have — a minimal
/// container can lack all three of these — which is the case bundled Lilex
/// exists for.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform_fonts {
    pub const MONO: &str = "DejaVu Sans Mono";
    pub const MONO_FALLBACKS: &[&str] = &["Liberation Mono", "Noto Sans Mono", "Lilex"];
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
    /// fallback chain. Call sites use `.font(typography.mono_font())` rather
    /// than `.font_family(...)` so glyphs the primary lacks cascade through
    /// `mono_fallbacks` instead of painting as tofu.
    pub fn mono_font(&self) -> Font {
        Font {
            family: self.family_mono.clone(),
            // Ligatures off: terminals must show literal byte sequences.
            // `->` should be two characters, not `→`; `==` two equals, not
            // `⩵`. This stopped being hypothetical when Lilex joined the
            // chain — it ligates by default, and a ligature spanning two
            // character cells cannot survive `shape_line`'s per-glyph
            // `force_width`, which is what keeps glyph cells 1:1 with
            // character cells.
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
