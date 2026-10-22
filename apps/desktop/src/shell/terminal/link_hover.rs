//! Hovering an OSC 8 hyperlink shows where it really goes.
//!
//! A program sets a hyperlink's visible text and its target independently, so
//! "https://bank.com" can point anywhere. The label names the actual target,
//! and leads with ⚠ when the visible text claims a different host (see
//! `terminal_links::osc8_text_misleads`). Painted by the canvas like the
//! exit-code chip rather than as a GPUI tooltip, so no hitbox steals the
//! mouse-down that starts a selection.

use gpui::{App, Bounds, Pixels, Point, SharedString, Size, TextAlign, TextRun, Window, fill, point, px};
use oximux_pty::TerminalSnapshot;

use crate::shell::terminal_links::{classify_osc8_target, osc8_text_misleads, Osc8Action};

/// Longest target shown before it is cut with `…`.
const MAX_LABEL_CHARS: usize = 120;

/// The label for the OSC 8 link under a cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkLabel {
    pub row: usize,
    pub col: usize,
    pub text: String,
    /// The visible text names a different destination than the target.
    pub warn: bool,
}

/// The label for the OSC 8 span covering `(row, col)`, if any.
pub fn osc8_label(snapshot: &TerminalSnapshot, row: usize, col: usize) -> Option<LinkLabel> {
    let span = snapshot
        .links
        .iter()
        .find(|l| l.row == row && col >= l.col_start && col <= l.col_end)?;
    let shown: String = snapshot
        .cells
        .get(row)?
        .iter()
        .skip(span.col_start)
        .take(span.col_end + 1 - span.col_start)
        .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
        .collect();
    let warn = osc8_text_misleads(&shown, &span.uri);
    // The target is program-chosen text too: strip what could disguise it.
    // Capped to the columns right of the link too, so it stays in the pane.
    let room = (snapshot.cols as usize).saturating_sub(span.col_start).clamp(16, MAX_LABEL_CHARS);
    let mut target = oximux_pty::program_status::sanitize_display_text(&span.uri);
    if target.chars().count() > room {
        target = target.chars().take(room - 1).collect::<String>() + "…";
    }
    let verdict = match classify_osc8_target(&span.uri) {
        Osc8Action::Open(_) => "",
        Osc8Action::OpenInEditor(_) => " — opens in the editor",
        Osc8Action::Blocked(_) => " — blocked",
    };
    let text = format!("{}{target}{verdict}", if warn { "⚠ " } else { "" });
    Some(LinkLabel { row, col: span.col_start, text, warn })
}

/// Paint the label as a chip on the row below the link, or above it when the
/// link sits on the last row. `origin` is the grid's top-left.
pub fn paint_link_label(
    label: &LinkLabel,
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    p: &crate::shell::terminal_canvas::PaintParams,
    window: &mut Window,
    cx: &mut App,
) {
    let rows = p.snapshot.cells.len();
    let row = if label.row + 1 < rows { label.row + 1 } else { label.row.saturating_sub(1) };
    let color = if label.warn { p.theme.status_error } else { p.theme.fg_base };
    let run = TextRun {
        len: label.text.len(),
        font: p.typography.mono_font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(SharedString::from(label.text.clone()), px(p.typography.t_body_sm), &[run], None);
    let pad_x = px(6.0);
    let chip = Bounds {
        origin: point(origin.x + cell_w * label.col as f32, origin.y + line_h * row as f32),
        size: Size { width: shaped.width + pad_x * 2.0, height: line_h },
    };
    window.paint_quad(
        fill(chip, p.theme.bg_overlay)
            .corner_radii(px(3.0))
            .border_widths(px(1.0))
            .border_color(if label.warn { p.theme.status_error } else { p.theme.border_inactive }),
    );
    let _ = shaped.paint(point(chip.origin.x + pad_x, chip.origin.y), line_h, TextAlign::Left, None, window, cx);
}

#[cfg(test)]
mod tests {
    use oximux_pty::snapshot::HyperlinkSpan;
    use oximux_pty::Cell;

    use super::*;

    fn snapshot_with(text: &str, uri: &str) -> TerminalSnapshot {
        let cells: Vec<Cell> = text.chars().map(|ch| Cell { ch, ..Cell::default() }).collect();
        TerminalSnapshot {
            links: vec![HyperlinkSpan { row: 0, col_start: 0, col_end: cells.len() - 1, uri: uri.into() }],
            cells: vec![cells],
            cols: 80,
            ..TerminalSnapshot::default()
        }
    }

    #[test]
    fn the_label_names_the_real_target_and_flags_a_lying_text() {
        let honest = osc8_label(&snapshot_with("docs", "https://docs.rs/x"), 0, 1).unwrap();
        assert_eq!(honest.text, "https://docs.rs/x");
        assert!(!honest.warn);

        let spoof = osc8_label(&snapshot_with("https://bank.com", "https://evil.example/"), 0, 3).unwrap();
        assert!(spoof.warn);
        assert!(spoof.text.starts_with("⚠ https://evil.example/"));

        let blocked = osc8_label(&snapshot_with("run", "x-app:go"), 0, 0).unwrap();
        assert!(blocked.text.ends_with("— blocked"));
        assert!(osc8_label(&snapshot_with("run", "x-app:go"), 1, 0).is_none());
    }

    #[test]
    fn the_label_is_cut_to_the_room_right_of_the_link() {
        let mut snap = snapshot_with("x", &format!("https://e.com/{}", "a".repeat(200)));
        snap.cols = 40;
        let label = osc8_label(&snap, 0, 0).unwrap();
        assert_eq!(label.text.chars().count(), 40);
        assert!(label.text.ends_with('…'));
    }
}
