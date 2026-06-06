//! Terminal settings pane — edits the `TerminalSettings` working copy.
//! Every control applies immediately: it mutates the copy and writes
//! `terminal.toml`; the live-reload watcher re-applies to open panes.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{BellStyle, Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{setting_row, stepper, value_chip};

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
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

    let bell = value_chip(
        "term-bell",
        bell_label(t.bell),
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.bell = match this.terminal.bell {
                BellStyle::Visual => BellStyle::Off,
                BellStyle::Off => BellStyle::Visual,
            };
            this.persist_terminal(cx);
        },
        cx,
    );

    let cursor_blink = toggle(
        "term-cursorblink",
        t.cursor_blink,
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.cursor_blink = !this.terminal.cursor_blink;
            this.persist_terminal(cx);
        },
        cx,
    );

    let osc52 = toggle(
        "term-osc52",
        t.osc52_clipboard,
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.osc52_clipboard = !this.terminal.osc52_clipboard;
            this.persist_terminal(cx);
        },
        cx,
    );

    let option_meta = toggle(
        "term-optmeta",
        t.option_as_meta,
        theme,
        density,
        typography,
        |this, _w, cx| {
            this.terminal.option_as_meta = !this.terminal.option_as_meta;
            this.persist_terminal(cx);
        },
        cx,
    );

    div()
        .flex()
        .flex_col()
        .child(setting_row("Scrollback (lines)", scrollback, theme, typography))
        .child(setting_row("Scroll multiplier", scroll_mult, theme, typography))
        .child(setting_row("Bell", bell, theme, typography))
        .child(setting_row("Cursor blink", cursor_blink, theme, typography))
        .child(setting_row("Blink interval", blink_interval, theme, typography))
        .child(setting_row(
            "Clipboard write (OSC 52)",
            osc52,
            theme,
            typography,
        ))
        .child(setting_row("Option as Meta", option_meta, theme, typography))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child("Changes save to terminal.toml and apply to open panes live."),
        )
        .into_any_element()
}

fn bell_label(b: BellStyle) -> &'static str {
    match b {
        BellStyle::Off => "Off",
        BellStyle::Visual => "Visual",
    }
}

/// A boolean toggle chip showing On/Off.
fn toggle(
    id: &'static str,
    value: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_click: impl Fn(&mut SettingsModal, &mut gpui::Window, &mut gpui::Context<SettingsModal>) + 'static,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    value_chip(
        id,
        if value { "On" } else { "Off" },
        theme,
        density,
        typography,
        on_click,
        cx,
    )
}
