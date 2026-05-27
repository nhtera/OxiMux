//! Density constants — heights, paddings, radii.
//!
//! All values in CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

#[derive(Debug, Clone, Copy)]
pub struct Density {
    // Heights
    pub h_top_bar: f32,
    pub h_status_bar: f32,
    pub h_tab: f32,
    pub h_row: f32,

    // Radii
    pub r_card: f32,
    pub r_xs: f32,

    // Padding / spacing
    pub pad_panel: f32,
    pub pad_row: f32,
    pub gap_inline: f32,

    // Sidebar default width (legacy — superseded by w_left_rail; kept for
    // backward compat with existing tests / phase-0 stub).
    pub w_sidebar: f32,
    /// Left rail width (workspaces + nav). Default 250px; min 220, max 500.
    pub w_left_rail: f32,
}

impl Density {
    /// Tight cockpit density. The only density in v1.
    pub fn cockpit() -> Self {
        Self {
            // Per-column chrome row height. Sized so the chrome row's
            // vertical center (y=16) lines up with where the macOS
            // traffic-light glyphs are drawn — `traffic_light_position
            // = point(12, 10)` puts the 12-px button at y=10..22,
            // center y=16. Tab strip lives in its own row BELOW this
            // one — see `workspace_root.rs` — keeping chip drag
            // delivery clear of any AppKit title-bar drag region.
            h_top_bar: 32.0,
            // 24 (not 22): room for the metric strip without descender clipping.
            h_status_bar: 24.0,
            h_tab: 28.0,
            h_row: 24.0,
            r_card: 8.0,
            r_xs: 4.0,
            pad_panel: 8.0,
            pad_row: 6.0,
            gap_inline: 6.0,
            w_sidebar: 240.0,
            w_left_rail: 250.0,
        }
    }
}

impl Default for Density {
    fn default() -> Self {
        Self::cockpit()
    }
}
