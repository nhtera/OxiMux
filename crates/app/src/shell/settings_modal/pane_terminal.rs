//! Terminal settings pane — edits the `TerminalSettings` working copy.
//! Every control applies immediately: it mutates the copy and writes
//! `terminal.toml`; the live-reload watcher re-applies to open panes.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{BellStyle, Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{stepper, toggle_switch};
use super::layout::{SettingEntry, entries_card, entry};
use super::segmented::{Segment, segmented};

/// Render the Terminal pane: a borderless group of setting rows plus a quiet
/// save-location caption.
pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(entries_card(
            theme,
            density,
            typography,
            entries(modal, theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Changes save to terminal.toml and apply to open panes live."),
        )
        .into_any_element()
}

/// The Terminal pane's settings as reusable entries (label + description +
/// live control). Used by the pane render and by global search.
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let t = modal.terminal;

    let scrollback = stepper(
        "term-scrollback",
        format!("{}", t.scrollback_lines),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.scrollback_lines = this.terminal.scrollback_lines.saturating_sub(1000);
            this.persist_terminal(cx);
        },
        |this, _w, cx| {
            this.terminal.scrollback_lines = (this.terminal.scrollback_lines + 1000).min(1_000_000);
            this.persist_terminal(cx);
        },
        cx,
    );

    let scroll_mult = stepper(
        "term-scrollmult",
        format!("{:.1}", t.scroll_multiplier),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.scroll_multiplier = (this.terminal.scroll_multiplier - 0.5).max(0.1);
            this.persist_terminal(cx);
        },
        |this, _w, cx| {
            this.terminal.scroll_multiplier = (this.terminal.scroll_multiplier + 0.5).min(50.0);
            this.persist_terminal(cx);
        },
        cx,
    );

    let blink_interval = stepper(
        "term-blink",
        format!("{} ms", t.blink_interval_ms),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.blink_interval_ms =
                this.terminal.blink_interval_ms.saturating_sub(50).max(60);
            this.persist_terminal(cx);
        },
        |this, _w, cx| {
            this.terminal.blink_interval_ms = (this.terminal.blink_interval_ms + 50).min(10_000);
            this.persist_terminal(cx);
        },
        cx,
    );

    let bell = segmented(
        "term-bell",
        vec![
            Segment::new("Off", t.bell == BellStyle::Off, |this, _w, cx| {
                this.terminal.bell = BellStyle::Off;
                this.persist_terminal(cx);
            }),
            Segment::new("Visual", t.bell == BellStyle::Visual, |this, _w, cx| {
                this.terminal.bell = BellStyle::Visual;
                this.persist_terminal(cx);
            }),
        ],
        theme,
        density,
        typography,
        cx,
    );

    let cursor_blink = toggle_switch(
        "term-cursorblink",
        t.cursor_blink,
        theme,
        |this, _w, cx| {
            this.terminal.cursor_blink = !this.terminal.cursor_blink;
            this.persist_terminal(cx);
        },
        cx,
    );

    let osc52 = toggle_switch(
        "term-osc52",
        t.osc52_clipboard,
        theme,
        |this, _w, cx| {
            this.terminal.osc52_clipboard = !this.terminal.osc52_clipboard;
            this.persist_terminal(cx);
        },
        cx,
    );

    let option_meta = toggle_switch(
        "term-optmeta",
        t.option_as_meta,
        theme,
        |this, _w, cx| {
            this.terminal.option_as_meta = !this.terminal.option_as_meta;
            this.persist_terminal(cx);
        },
        cx,
    );

    vec![
        entry(
            "Scrollback",
            "Maximum lines kept in scrollback history.",
            scrollback,
        ),
        entry(
            "Scroll multiplier",
            "Lines scrolled per mouse-wheel notch.",
            scroll_mult,
        ),
        entry("Bell", "Visual flash on the terminal bell, or off.", bell),
        entry(
            "Cursor blink",
            "Blink the cursor while the terminal is focused.",
            cursor_blink,
        ),
        entry(
            "Blink interval",
            "Cursor blink period in milliseconds.",
            blink_interval,
        ),
        entry(
            "Clipboard write (OSC 52)",
            "Let programs set the system clipboard via OSC 52.",
            osc52,
        ),
        entry(
            "Option as Meta",
            "Send the macOS Option key as Meta/Alt to the shell.",
            option_meta,
        ),
    ]
}
