//! Smoke test: proves `#[gpui::test]` harness compiles and runs against the
//! dual-`gpui::App` risk documented in `docs/gpui-pins.md`. Uses `build_row`
//! (pure render fn) inside a minimal `Render` fixture — no PTY process
//! spawned, no live `TerminalView::mount`.
//!
//! If this test ever fails to link with a duplicate-symbol error on
//! `gpui::App`, the lockfile has split the `gpui` graph (two source revs
//! in `Cargo.lock`). Resolution: `cargo update -p gpui` until the graph
//! converges; do NOT add a `rev = "..."` to the workspace `Cargo.toml`.

use gpui::{Context, IntoElement, Render, TestAppContext, Window, px};
use oximux_app::shell::terminal_row::build_row;
use oximux_pty::Cell;
use oximux_settings::Theme;

struct RowFixtureView {
    row: Vec<Cell>,
    theme: Theme,
}

impl RowFixtureView {
    fn new() -> Self {
        let mut row = vec![Cell::default(); 10];
        row[0].ch = 'H';
        row[1].ch = 'i';
        Self {
            row,
            theme: Theme::default(),
        }
    }
}

impl Render for RowFixtureView {
    fn render(&mut self, _win: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        build_row(&self.row, 0, (0, 0), None, &self.theme, px(20.))
    }
}

#[gpui::test]
async fn row_fixture_renders_without_panic(cx: &mut TestAppContext) {
    let window = cx.add_window(|_win, _cx| RowFixtureView::new());
    cx.run_until_parked();
    // Read back the root view — fails iff the window died or type-mismatched.
    cx.read(|app| {
        let view = window.read(app).expect("root view should be alive");
        assert_eq!(view.row.len(), 10);
        assert_eq!(view.row[0].ch, 'H');
        assert_eq!(view.row[1].ch, 'i');
    });
}
