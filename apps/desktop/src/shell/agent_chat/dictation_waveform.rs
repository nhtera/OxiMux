//! Shared recording-waveform rendering.
//!
//! One helper both the composer's in-line recording bar and the global
//! "Listening…" HUD pill call, so the two surfaces animate identically. Bar
//! heights come from a [`super::dictation_ui::WaveformBuffer`]; the gpui element
//! is built here to keep the buffer pure/testable.

use gpui::{AnyElement, Hsla, IntoElement, ParentElement, Styled, div, px};

/// Geometry for one waveform. `bar_w`/`gap` are px; `height` is the full bar
/// area a max-amplitude sample fills (centered on the midline).
#[derive(Clone, Copy)]
pub struct WaveformStyle {
    pub height: f32,
    pub bar_w: f32,
    pub gap: f32,
    pub color: Hsla,
    /// When true the bar row is `w_full` and spreads its bars edge-to-edge
    /// (`justify_between`, ignoring `gap`) so the waveform fills the whole
    /// recording bar like the ChatGPT desktop app, whatever the composer width.
    /// When false the row is intrinsic-width with a fixed `gap` between bars.
    pub fill: bool,
}

/// Render `bars` (heights in 0..1, oldest first) as a row of centered rounded
/// bars — a ChatGPT-style live waveform. An empty slice renders a flat baseline
/// of minimum-height bars so the control keeps its footprint before audio
/// arrives.
pub fn render_waveform(bars: &[f32], style: WaveformStyle) -> AnyElement {
    // Minimum bar height so silence still reads as a fine dotted baseline (like
    // ChatGPT's), not a gap.
    let min_h = 1.5_f32;
    let make_bar = |h: f32| {
        let clamped = h.clamp(0.0, 1.0);
        let px_h = (style.height * clamped).max(min_h);
        div()
            .flex_none()
            .w(px(style.bar_w))
            .h(px(px_h))
            .rounded(px(style.bar_w / 2.0))
            .bg(style.color)
    };

    let mut row = div().flex().flex_row().items_center().h(px(style.height));
    // Fill mode spreads the bars across the full available width
    // (`justify_between`, no fixed gap); intrinsic mode packs them with `gap`.
    row = if style.fill {
        row.w_full().justify_between()
    } else {
        row.gap(px(style.gap))
    };

    if bars.is_empty() {
        // Placeholder baseline: a handful of flat bars.
        for _ in 0..12 {
            row = row.child(make_bar(0.0));
        }
    } else {
        for &h in bars {
            row = row.child(make_bar(h));
        }
    }
    row.into_any_element()
}
