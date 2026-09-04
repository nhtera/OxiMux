//! Reproduction probe for issue #3 — "Double Keystroke Output in Terminal".
//!
//! Isolates the exact wiring the terminal pane uses on the alt-screen: an
//! `on_key_down` listener that encodes the key itself, PLUS a platform
//! `InputHandler` registered every paint via `window.handle_input`. Both of
//! those are real; the question is whether ONE keystroke reaches BOTH.
//!
//! GPUI's macOS window treats a key event whose listeners let it propagate as
//! *unhandled*, and then hands the same native event to the platform text-input
//! path (`gpui_macos::window::handle_key_event`). So if the key listener writes
//! bytes without calling `cx.stop_propagation()`, the character is written a
//! second time — once by the listener, once by `replace_text_in_range`.
//!
//! This cannot be shown with a `#[gpui::test]`: the test platform has no
//! `NSTextInputContext`, so the second delivery — the whole bug — does not
//! exist there. It needs a real window.
//!
//! ```sh
//! # the pre-fix wiring: expect 2 writes per keystroke
//! cargo run -p oximux-app --example key_double_repro
//! # what `TerminalView::on_key_down` now does: expect 1 write
//! REPRO_STOP_PROPAGATION=1 cargo run -p oximux-app --example key_double_repro
//! ```
//!
//! Writes one line per delivery to `/tmp/oximux_key_double_repro.log`.

use std::io::Write as _;
use std::ops::Range;

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InputHandler,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Point, Render,
    Styled, UTF16Selection, Window, WindowBounds, WindowOptions, canvas, div, hsla, point, px,
    size,
};

const LOG: &str = "/tmp/oximux_key_double_repro.log";

fn log(msg: &str) {
    println!("{msg}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Set to mirror the fix: the key listener claims the keystroke instead of
/// letting it fall through to the platform input context.
fn stop_propagation_enabled() -> bool {
    std::env::var_os("REPRO_STOP_PROPAGATION").is_some()
}

struct Probe {
    focus: FocusHandle,
}

impl Probe {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
        }
    }

    /// Stands in for `TerminalView::on_key_down` on the alt-screen: every
    /// keystroke is encoded to bytes here, with no deferral to the IME.
    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.platform {
            return;
        }
        let bytes = ks
            .key_char
            .clone()
            .unwrap_or_else(|| ks.key.clone())
            .into_bytes();
        log(&format!(
            "WRITE via=on_key_down key={} held={} bytes={:?}",
            ks.key,
            ev.is_held,
            String::from_utf8_lossy(&bytes)
        ));
        if stop_propagation_enabled() {
            cx.stop_propagation();
        }
    }
}

impl Focusable for Probe {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus = self.focus.clone();
        let grid = canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, _: (), window, cx| {
                // Same registration point as `TerminalView`'s canvas paint.
                window.handle_input(
                    &focus,
                    ProbeInputHandler {
                        _view: entity.clone(),
                    },
                    cx,
                );
            },
        )
        .size_full();

        div()
            .id("probe")
            .track_focus(&self.focus)
            .size_full()
            .bg(hsla(0.0, 0.0, 0.1, 1.0))
            .text_color(hsla(0.0, 0.0, 0.9, 1.0))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.on_key_down(ev, window, cx);
            }))
            .child(grid)
    }
}

/// Mirrors `TerminalInputHandler` with the terminal on the alt-screen:
/// `selected_text_range` is `None` (IME nominally off) and press-and-hold is
/// disabled, exactly as the terminal registers it.
struct ProbeInputHandler {
    _view: Entity<Probe>,
}

impl InputHandler for ProbeInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        // Alt-screen: the terminal reports "no text input here".
        None
    }

    fn marked_text_range(&mut self, _window: &mut Window, _cx: &mut App) -> Option<Range<usize>> {
        None
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        log(&format!("WRITE via=ime_commit bytes={text:?}"));
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        log(&format!("mark {new_text:?}"));
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut App) {}

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn main() {
    let _ = std::fs::remove_file(LOG);
    let mode = if stop_propagation_enabled() {
        "stop_propagation=ON (the shipped wiring)"
    } else {
        "stop_propagation=OFF (the issue #3 defect)"
    };
    log(&format!("--- key_double_repro: {mode} ---"));

    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds {
            origin: point(px(200.0), px(200.0)),
            size: size(px(520.0), px(220.0)),
        };
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| cx.new(Probe::new),
            )
            .expect("open window");
        // Focus the probe so `handle_input` is not a no-op.
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle(cx), cx);
            })
            .expect("focus probe");
        cx.activate(true);
    });
}
