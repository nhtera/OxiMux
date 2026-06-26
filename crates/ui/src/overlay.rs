//! Shared chrome for floating surfaces (popovers, pickers, dialogs, menus,
//! toasts).
//!
//! The recipe — overlay background, a 1px active-border outline, card-radius
//! corners — was hand-rolled identically across ~13 surfaces. Centralizing it
//! here means the surface contract lives in one place, and the top
//! inner-highlight (a 1px edge catching light that reads as elevation without a
//! shadow) is a one-line addition everywhere instead of a per-file change.
//!
//! Applied in-chain via [`FloatingSurface::floating_chrome`] so it drops in
//! wherever a surface already hand-rolled `bg_overlay` + `border_active` +
//! `r_card`, replacing those calls with one and leaving the surface's own
//! padding, sizing, layout, and content children untouched.

use gpui::{Div, ParentElement, Styled, div, px};

use oximux_settings::{Density, Theme};

/// The non-interactive top inner-highlight. A plain `div` with no mouse
/// handlers never registers a hitbox, and it is added before the surface's own
/// children (so it paints under them), meaning it cannot capture clicks meant
/// for the rows above it. Inset horizontally by the corner radius so it spans
/// only the straight part of the top edge and never overhangs the rounded
/// corners.
fn edge_highlight(theme: &Theme, density: &Density) -> Div {
    div()
        .absolute()
        .top_0()
        .left(px(density.r_card))
        .right(px(density.r_card))
        .h(px(1.))
        .bg(theme.edge_highlight)
}

/// Applies the floating-surface chrome to any `Div` mid-chain.
pub trait FloatingSurface {
    /// Paint this element as a floating surface: `bg_overlay`, a 1px
    /// `border_active` outline, `r_card` corners, a `relative` positioning
    /// context, and the top inner-highlight. Call where the surface previously
    /// hand-rolled those four chrome lines; keep the surrounding padding /
    /// sizing / `flex_*` / `.child(...)` calls as they were.
    fn floating_chrome(self, theme: &Theme, density: &Density) -> Self;
}

impl FloatingSurface for Div {
    fn floating_chrome(self, theme: &Theme, density: &Density) -> Div {
        self.relative()
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .child(edge_highlight(theme, density))
    }
}
