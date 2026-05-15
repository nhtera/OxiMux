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

    // Sidebar default width
    pub w_sidebar: f32,
}

impl Density {
    /// Tight cockpit density. The only density in v1.
    pub fn cockpit() -> Self {
        Self {
            h_top_bar: 36.0,
            h_status_bar: 22.0,
            h_tab: 28.0,
            h_row: 24.0,
            r_card: 8.0,
            r_xs: 4.0,
            pad_panel: 8.0,
            pad_row: 6.0,
            gap_inline: 6.0,
            w_sidebar: 240.0,
        }
    }
}

impl Default for Density {
    fn default() -> Self {
        Self::cockpit()
    }
}
