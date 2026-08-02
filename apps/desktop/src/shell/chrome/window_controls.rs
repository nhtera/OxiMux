//! Windows caption buttons — Minimize / Maximize-or-Restore / Close.
//!
//! With `appears_transparent: true` GPUI strips the entire native caption on
//! Windows, so the app must draw its own caption buttons and mark each with a
//! `WindowControlArea` hitbox. The platform layer then answers WM_NCHITTEST
//! with HTMINBUTTON / HTMAXBUTTON / HTCLOSE and Windows itself performs the
//! press/release handling — including Snap Layouts on max-button hover on
//! Win11 — which is why the buttons carry no click handlers of their own.
//!
//! `.occlude()` on every button is load-bearing: the surrounding chrome strip
//! is marked `WindowControlArea::Drag`, and the hit-test walk returns the
//! FIRST control area whose hitbox is under the cursor. An opaque button
//! hitbox keeps the strip's Drag hitbox out of the hover set, so hovering a
//! button resolves to the button, not to HTCAPTION.

use gpui::{
    App, InteractiveElement as _, IntoElement, ParentElement, RenderOnce, Rgba, Styled, Window,
    WindowControlArea, div, px, svg,
};
use oximux_settings::Theme;

/// One caption button's width — matches the chrome strip's
/// `TOGGLE_BUTTON_WIDTH` so the trailing cluster reads as one row of
/// same-size hit targets.
const BUTTON_WIDTH: f32 = 36.0;

/// Caption glyph size. Native Windows caption glyphs are small and thin;
/// 10px over a 36px button matches the reference chrome.
const ICON_SIZE: f32 = 10.0;

/// The Windows close-button hover red (same value native chrome uses).
const CLOSE_HOVER_RED: Rgba = Rgba {
    r: 232.0 / 255.0,
    g: 17.0 / 255.0,
    b: 32.0 / 255.0,
    a: 1.0,
};

/// The three-button caption cluster. Renders at the trailing edge of
/// whichever header strip currently owns the window's top-right corner.
#[derive(IntoElement)]
pub struct WindowsWindowControls {
    theme: Theme,
}

impl WindowsWindowControls {
    pub fn new(theme: Theme) -> Self {
        Self { theme }
    }
}

impl RenderOnce for WindowsWindowControls {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // Maximize and Restore share one button; the glyph tracks live
        // window state so un-maximizing shows the overlapping-squares icon.
        let max_icon = if window.is_maximized() {
            "icons/win-restore.svg"
        } else {
            "icons/win-maximize.svg"
        };
        let theme = self.theme;
        div()
            .id("windows-window-controls")
            .flex()
            .flex_row()
            .h_full()
            .flex_shrink_0()
            .child(caption_button(
                "caption-minimize",
                "icons/win-minimize.svg",
                WindowControlArea::Min,
                theme.hover_overlay,
                theme.fg_base,
                theme,
            ))
            .child(caption_button(
                "caption-maximize",
                max_icon,
                WindowControlArea::Max,
                theme.hover_overlay,
                theme.fg_base,
                theme,
            ))
            .child(
                // Close gets the native destructive treatment: red fill +
                // white glyph on hover instead of the neutral overlay.
                caption_button(
                    "caption-close",
                    "icons/win-close.svg",
                    WindowControlArea::Close,
                    CLOSE_HOVER_RED.into(),
                    gpui::white(),
                    theme,
                ),
            )
    }
}

/// One caption button. The glyph color must be set on the `svg` element
/// itself (a parent's `text_color` does NOT cascade into gpui's svg paint),
/// so the hover recolor goes through `.group()` on the button +
/// `.group_hover()` on the svg.
fn caption_button(
    id: &'static str,
    icon: &'static str,
    area: WindowControlArea,
    hover_bg: gpui::Hsla,
    hover_fg: gpui::Hsla,
    theme: Theme,
) -> impl IntoElement {
    div()
        .id(id)
        .group(id)
        .w(px(BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .occlude()
        .window_control_area(area)
        .hover(move |s| s.bg(hover_bg))
        .child(
            svg()
                .path(icon)
                .size(px(ICON_SIZE))
                .text_color(theme.fg_muted)
                .group_hover(id, move |s| s.text_color(hover_fg)),
        )
}
