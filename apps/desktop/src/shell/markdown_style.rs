//! One answer to "how should GFM markdown be styled", shared by every surface
//! that renders it.
//!
//! # Why this is not a `Default::default()`
//!
//! `TextViewStyle`'s own default is a **light** code highlight theme. That is
//! the wrong answer under a dark palette — code blocks read washed-out — so
//! every markdown surface in the app overrode it. Each did so by pinning
//! `is_dark: true` and `HighlightTheme::default_dark()`, which was correct for
//! exactly as long as the app had one palette.
//!
//! It no longer does. A pinned dark syntax theme under the paper palette is the
//! same defect the pinned light default was under charcoal, in the other
//! direction: dark-theme code colors on a light card. The fix is not another
//! constant but a function of the palette, which is what this is.
//!
//! `is_dark` and `highlight_theme` move together on purpose — `is_dark` sets
//! the renderer's own surface treatment and the highlight theme colors the
//! fences. Splitting them is how a surface ends up half-converted, so the two
//! are decided here in one place and never separately at a call site.

use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::TextViewStyle;
use oximux_settings::Theme;

/// The GFM renderer's style under `theme`'s polarity.
///
/// Call this rather than building a `TextViewStyle` inline: a surface that
/// constructs its own is a surface that will not follow the next palette.
pub fn gfm_style(theme: &Theme) -> TextViewStyle {
    let is_dark = !theme.is_light();
    TextViewStyle {
        is_dark,
        highlight_theme: if is_dark {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: both shipped palettes get their own polarity, rather
    /// than one of them getting the other's syntax colors.
    #[test]
    fn each_palette_gets_its_own_polarity() {
        assert!(gfm_style(&Theme::charcoal()).is_dark);
        assert!(!gfm_style(&Theme::paper()).is_dark);
    }

    /// `is_dark` and the highlight theme are one decision. If a refactor ever
    /// lets them disagree, a light card gets dark-theme code colors — the exact
    /// bug this module was written to remove, and one that no type would catch.
    #[test]
    fn the_surface_and_the_fences_agree_on_polarity() {
        for theme in [Theme::charcoal(), Theme::paper()] {
            let style = gfm_style(&theme);
            let expected = if style.is_dark {
                HighlightTheme::default_dark()
            } else {
                HighlightTheme::default_light()
            };
            assert_eq!(
                style.highlight_theme.name, expected.name,
                "{} palette: fences disagree with the surface",
                if theme.is_light() { "paper" } else { "charcoal" },
            );
        }
    }
}
