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

    // Action-row + overlay chrome (added in design-system tightening pass).
    /// Row that hosts inline action buttons (stash entry with Apply/Pop/
    /// Drop, worktree entry with Remove, empty placeholders that need
    /// to occupy the same height). Taller than a plain `h_row` so the
    /// 22px xsmall buttons sit centred without crowding the row border.
    /// Replaces hand-coded `h_row * 1.4` arithmetic.
    pub h_action_row: f32,
    /// Inner padding for floating cards (context menus, pickers,
    /// dropdowns). Slightly tighter than `pad_panel` because overlays
    /// sit on their own surface with a border and don't need the same
    /// internal breathing room.
    pub pad_overlay: f32,
    /// Row height for items inside a floating card (context menu rows,
    /// picker rows). Shared across pane / adapter / commit-context
    /// menus so the click targets feel like one component family.
    pub h_overlay_item: f32,
    /// Chip corner radius. Intentionally smaller than `r_xs` (which is
    /// for inputs / buttons) — chips are inline badges and read tighter
    /// at a smaller radius. SCM ref chips, search-toggle pills, diff
    /// hunk action chips all share this value.
    pub r_chip: f32,

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
            h_action_row: 34.0,
            pad_overlay: 6.0,
            h_overlay_item: 30.0,
            r_chip: 3.0,
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
