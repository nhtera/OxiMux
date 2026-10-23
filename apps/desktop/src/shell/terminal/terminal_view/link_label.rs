//! The hovered-hyperlink label's state: which OSC 8 link the pointer is on,
//! updated on mouse-move so the label appears and goes as the pointer does
//! (not only on the next output repaint). Painting is `link_hover.rs`.

use super::*;

impl TerminalView {
    /// Recompute the label for the cell under `pos`.
    pub(super) fn refresh_link_label(&mut self, pos: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let (row, col) = self.cell_at(pos, window);
        let label = crate::shell::link_hover::osc8_label(&self.snapshot, row, col);
        self.set_link_label(label, cx);
    }

    /// Record the label, repainting only when it changed.
    pub(super) fn set_link_label(&mut self, label: Option<crate::shell::link_hover::LinkLabel>, cx: &mut Context<Self>) {
        if label != self.link_label {
            self.link_label = label;
            cx.notify();
        }
    }
}
