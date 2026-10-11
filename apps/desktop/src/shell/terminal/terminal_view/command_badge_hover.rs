//! Hovering a command-status mark labels it with the command's exit code.
//!
//! The mark is a 2px bar in the left padding gutter, so the hover target is
//! that gutter on a badged row — never the grid itself, where the pointer
//! means text selection. The label is painted by the canvas
//! (`PaintParams::hovered_badge`) rather than built as a GPUI tooltip: a
//! tooltip needs a hitbox over the gutter, and that hitbox would take the
//! mouse-down that starts a selection there.

use super::*;

impl TerminalView {
    /// The painted badge whose gutter `pos` is in, under the `mode` setting.
    pub(super) fn badge_under(&self, pos: Point<Pixels>, mode: ExitBadges, window: &Window) -> Option<CommandBadge> {
        let bounds = self.canvas_bounds.get();
        let pad = self.density.pad_panel;
        // Most moves are over the grid: answer those before measuring cells
        // or collecting badges.
        if !in_gutter_column(pos, bounds, pad) {
            return None;
        }
        let line_h = CellMetrics::measure(&self.typography, window).line_height;
        badge_in_gutter(pos, bounds, pad, line_h, &self.visible_command_badges(mode))
    }

    /// The badge to label this frame. It follows the pointer's position NOW,
    /// not the badge recorded at the last mouse-move: a scroll, `clear`, mode
    /// change, resize, or tab switch since then moves a mark under (or out
    /// from under) a pointer that hasn't moved. Only the pane the pointer is
    /// over shows a label.
    pub(super) fn label_badge(&self, badges: &[CommandBadge], window: &Window) -> Option<CommandBadge> {
        if !self.pointer_in_view {
            return None;
        }
        let line_h = CellMetrics::measure(&self.typography, window).line_height;
        badge_in_gutter(window.mouse_position(), self.canvas_bounds.get(), self.density.pad_panel, line_h, badges)
    }

    /// Record the hovered badge, repainting only when it changed.
    pub(super) fn set_hovered_badge(&mut self, badge: Option<CommandBadge>, cx: &mut Context<Self>) {
        if badge != self.hovered_badge {
            self.hovered_badge = badge;
            cx.notify();
        }
    }
}

/// Pure hit test: `pos` must lie in the left padding gutter (`pad` wide) of
/// the canvas `bounds`, on a row that carries one of `badges`. Rows start
/// below the top padding, as `paint_grid` lays them out; the top padding
/// itself and an unpainted (zero-sized) canvas hit nothing.
pub(super) fn badge_in_gutter(
    pos: Point<Pixels>,
    bounds: Bounds<Pixels>,
    pad: f32,
    line_h: f32,
    badges: &[CommandBadge],
) -> Option<CommandBadge> {
    if line_h <= 0.0 || !in_gutter_column(pos, bounds, pad) {
        return None;
    }
    let rel_y = f32::from(pos.y) - f32::from(bounds.origin.y) - pad;
    if rel_y < 0.0 || pos.y >= bounds.origin.y + bounds.size.height {
        return None;
    }
    let row = (rel_y / line_h).floor() as usize;
    badges.iter().copied().find(|b| b.row == row)
}

/// Whether `pos` is within the left padding column of a painted canvas.
fn in_gutter_column(pos: Point<Pixels>, bounds: Bounds<Pixels>, pad: f32) -> bool {
    let rel_x = f32::from(pos.x) - f32::from(bounds.origin.x);
    bounds.size.width > px(0.0) && (0.0..pad).contains(&rel_x)
}
