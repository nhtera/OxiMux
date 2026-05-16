//! Typography scale — sizes, weights, font stacks.
//!
//! Sizes are CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

use gpui::{Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, SharedString};

#[derive(Debug, Clone)]
pub struct Typography {
    // Sizes
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

impl Typography {
    pub fn cockpit() -> Self {
        Self {
            t_label_xs: 10.0,
            t_label_caps: 10.5,
            t_body_sm: 11.0,
            t_brand: 12.0,
            t_body_md: 13.0,
            t_body_lg: 14.0,

            w_regular: FontWeight::NORMAL,
            w_medium: FontWeight::MEDIUM,
            w_semibold: FontWeight::SEMIBOLD,

            // `Menlo` is the primary because it's the only mono face we can
            // guarantee on every macOS 13+ install (Geist Mono is opt-in,
            // and "SF Mono" registers as `.SF NS Mono` / `SFMono-Regular`
            // which font-kit's family selector misses). It carries the
            // full Block Elements range (U+2580–259F) and Box Drawing
            // (U+2500–257F), which is required for half-block pixel art
            // — Claude Code's mascot is the canonical regression case.
            //
            // `FontFallbacks` in GPUI only cascades for *individual glyph*
            // lookups inside an already-loaded family, not when the
            // primary family fails to load entirely. So the primary MUST
            // resolve, and the fallbacks act as glyph-coverage backups
            // (e.g. SF Mono / Monaco for non-Latin scripts).
            family_mono: "Menlo".into(),
            family_ui: "Helvetica Neue".into(),
            mono_fallbacks: vec!["SF Mono".into(), "Monaco".into()],
            ui_fallbacks: vec!["Helvetica".into()],
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
            // behavior correct if the user later swaps to JetBrains Mono,
            // Fira Code, etc. Zed does the same in their terminal element
            // (`terminal_element.rs:903–955`).
            features: FontFeatures::disable_ligatures(),
            fallbacks: Some(FontFallbacks::from_fonts(
                self.mono_fallbacks
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
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
