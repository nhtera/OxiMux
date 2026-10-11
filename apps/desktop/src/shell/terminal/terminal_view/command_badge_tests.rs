//! The `exit_badges` setting decides which finished commands get a gutter
//! mark, and hovering a mark names its exit code. A green bar on every
//! successful prompt read as a diff's "added line", so the default marks
//! failures only.

use gpui::{Bounds, TestAppContext, point, px, size};

use super::command_badge_hover::badge_in_gutter;
use super::command_mark_tests::mount_view;
use super::*;

/// Four finished commands on rows 1, 4, 7, 10 exiting 0, 1, 0, 130, plus one
/// still running on row 12.
fn seed_marks(view: &mut TerminalView) {
    for (line, exit) in [(1_u64, 0), (4, 1), (7, 0), (10, 130)] {
        view.apply_command_mark(CommandMarkKind::PromptStart, None, line);
        view.apply_command_mark(CommandMarkKind::CommandEnd, Some(exit), line);
    }
    view.apply_command_mark(CommandMarkKind::PromptStart, None, 12);
}

fn rows(badges: Vec<CommandBadge>) -> Vec<(usize, i32)> {
    badges.into_iter().map(|b| (b.row, b.exit)).collect()
}

#[gpui::test]
async fn each_mode_paints_its_own_set_of_marks(cx: &mut TestAppContext) {
    let window = mount_view(cx);
    cx.run_until_parked();
    window
        .update(cx, |view, _win, _cx| {
            seed_marks(view);
            assert_eq!(
                rows(view.visible_command_badges(ExitBadges::Failures)),
                vec![(4, 1), (10, 130)],
                "the default marks failures only"
            );
            assert_eq!(
                rows(view.visible_command_badges(ExitBadges::All)),
                vec![(1, 0), (4, 1), (7, 0), (10, 130)],
                "all marks every finished command; the running one still has no exit"
            );
            assert!(view.visible_command_badges(ExitBadges::Off).is_empty());
        })
        .expect("window update");
}

// "Send last output to agent" reads the marks, not the painted badges, so
// hiding them must not forget them.
#[gpui::test]
async fn hiding_marks_keeps_them_recorded(cx: &mut TestAppContext) {
    let window = mount_view(cx);
    cx.run_until_parked();
    window
        .update(cx, |view, _win, _cx| {
            seed_marks(view);
            assert!(view.visible_command_badges(ExitBadges::Off).is_empty());
            assert_eq!(view.command_marks.len(), 5);
        })
        .expect("window update");
}

/// A canvas at (100, 50), 400×300, with an 8px gutter and 20px rows.
fn canvas() -> Bounds<Pixels> {
    Bounds { origin: point(px(100.0), px(50.0)), size: size(px(400.0), px(300.0)) }
}

const PAD: f32 = 8.0;
const LINE_H: f32 = 20.0;
const FAILED_ROW_4: CommandBadge = CommandBadge { row: 4, exit: 1 };

#[test]
fn the_gutter_of_a_marked_row_hits_its_badge() {
    // Row 4 spans y = 50 + 8 + 80 .. +20 → 138..158.
    let hit = badge_in_gutter(point(px(103.0), px(140.0)), canvas(), PAD, LINE_H, &[FAILED_ROW_4]);
    assert_eq!(hit, Some(FAILED_ROW_4));
}

#[test]
fn the_grid_an_unmarked_row_and_the_top_padding_hit_nothing() {
    let badges = [FAILED_ROW_4];
    // Column 0 of the grid, on the marked row: that is text, not the mark.
    assert_eq!(badge_in_gutter(point(px(109.0), px(140.0)), canvas(), PAD, LINE_H, &badges), None);
    // The gutter of row 3.
    assert_eq!(badge_in_gutter(point(px(103.0), px(120.0)), canvas(), PAD, LINE_H, &badges), None);
    // The top padding, above row 0, with a mark on row 0.
    let row0 = [CommandBadge { row: 0, exit: 2 }];
    assert_eq!(badge_in_gutter(point(px(103.0), px(53.0)), canvas(), PAD, LINE_H, &row0), None);
    // Left of the canvas.
    assert_eq!(badge_in_gutter(point(px(99.0), px(140.0)), canvas(), PAD, LINE_H, &badges), None);
    // The bottom padding, below the last row (13 spans y 318..338; the canvas
    // ends at 350), with a mark on that last row.
    let last = [CommandBadge { row: 13, exit: 1 }];
    assert_eq!(badge_in_gutter(point(px(103.0), px(345.0)), canvas(), PAD, LINE_H, &last), None);
    assert_eq!(badge_in_gutter(point(px(103.0), px(330.0)), canvas(), PAD, LINE_H, &last), Some(last[0]));
}

#[test]
fn an_unpainted_canvas_hits_nothing() {
    let unpainted = Bounds::default();
    let hit = badge_in_gutter(point(px(3.0), px(90.0)), unpainted, PAD, LINE_H, &[FAILED_ROW_4]);
    assert_eq!(hit, None);
}
