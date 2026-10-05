//! Terminal settings pane — edits the `TerminalSettings` working copy.
//! Every control applies immediately: it mutates the copy and writes
//! `terminal.toml`; the live-reload watcher re-applies to open panes.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{
    BellStyle, Density, PerTerminalHistory, Theme, Typography, WindowsPowerShell, WindowsShell,
};

use super::SettingsModal;
use super::controls::{stepper, toggle_switch};
use super::layout::{SettingEntry, entries_card, entry};
use super::segmented::{Segment, segmented};

mod daemon;

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
            settings_entries(modal, theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Changes save to terminal.toml and apply to open panes live."),
        )
        .child(daemon::render(theme, density, typography, cx))
        .into_any_element()
}

/// Every row the Terminal pane shows, the daemon section's included — what
/// global search matches.
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut rows = settings_entries(modal, theme, density, typography, cx);
    rows.extend(daemon::entries(theme, density, typography, cx));
    rows
}

/// The Terminal pane's settings as reusable entries (label + description +
/// live control): the pane's first card.
fn settings_entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let t = &modal.terminal;

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
            Segment::new("Notify", t.bell == BellStyle::Notify, |this, _w, cx| {
                this.terminal.bell = BellStyle::Notify;
                this.persist_terminal(cx);
            }),
        ],
        theme,
        density,
        typography,
        cx,
    );

    // Windows-only: which shell family new panes run, and which PowerShell
    // binary the PowerShell family resolves to. Built unconditionally (the
    // enums are cross-platform) but only surfaced as rows on Windows below.
    let windows_shell = segmented(
        "term-winshell",
        vec![
            Segment::new(
                "PowerShell",
                t.windows_shell == WindowsShell::PowerShell,
                |this, _w, cx| {
                    this.terminal.windows_shell = WindowsShell::PowerShell;
                    this.persist_terminal(cx);
                },
            ),
            Segment::new(
                "Command Prompt",
                t.windows_shell == WindowsShell::CommandPrompt,
                |this, _w, cx| {
                    this.terminal.windows_shell = WindowsShell::CommandPrompt;
                    this.persist_terminal(cx);
                },
            ),
            Segment::new(
                "Git Bash",
                t.windows_shell == WindowsShell::GitBash,
                |this, _w, cx| {
                    this.terminal.windows_shell = WindowsShell::GitBash;
                    this.persist_terminal(cx);
                },
            ),
            Segment::new("Auto", t.windows_shell == WindowsShell::Auto, |this, _w, cx| {
                this.terminal.windows_shell = WindowsShell::Auto;
                this.persist_terminal(cx);
            }),
        ],
        theme,
        density,
        typography,
        cx,
    );

    let windows_powershell = segmented(
        "term-winps",
        vec![
            Segment::new(
                "Auto",
                t.windows_powershell == WindowsPowerShell::Auto,
                |this, _w, cx| {
                    this.terminal.windows_powershell = WindowsPowerShell::Auto;
                    this.persist_terminal(cx);
                },
            ),
            Segment::new(
                "pwsh 7",
                t.windows_powershell == WindowsPowerShell::Pwsh,
                |this, _w, cx| {
                    this.terminal.windows_powershell = WindowsPowerShell::Pwsh;
                    this.persist_terminal(cx);
                },
            ),
            Segment::new(
                "Windows",
                t.windows_powershell == WindowsPowerShell::Windows,
                |this, _w, cx| {
                    this.terminal.windows_powershell = WindowsPowerShell::Windows;
                    this.persist_terminal(cx);
                },
            ),
        ],
        theme,
        density,
        typography,
        cx,
    );

    let history = segmented(
        "term-history",
        history_segments(t.per_terminal_history),
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

    let copy_on_select = toggle_switch(
        "term-copyonselect",
        t.copy_on_select,
        theme,
        |this, _w, cx| {
            this.terminal.copy_on_select = !this.terminal.copy_on_select;
            this.persist_terminal(cx);
        },
        cx,
    );

    let mut rows: Vec<SettingEntry> = Vec::new();

    // The Windows shell picker leads the pane, matching the "Windows Shell"
    // section other terminals surface. Only meaningful on Windows; off it the
    // shell is `$SHELL` / the POSIX default, so these rows are hidden.
    if cfg!(windows) {
        rows.push(entry(
            "Default shell",
            "Shell new terminal panes run on Windows. Auto prefers Git Bash when Git for Windows is installed.",
            windows_shell,
        ));
        rows.push(entry(
            "PowerShell edition",
            "Which PowerShell the PowerShell option uses: Auto (pwsh 7 when present), pwsh 7, or inbox Windows PowerShell.",
            windows_powershell,
        ));
    } else {
        // Keep the controls "used" on non-Windows without rendering them.
        let _ = (&windows_shell, &windows_powershell);
    }

    rows.extend([
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
            "Shell history",
            "Up-arrow recalls each terminal's own commands, and every command still reaches your history file. \
             Auto keeps history shared when your shell already shares it live (zsh share_history, on in oh-my-zsh); \
             Per terminal separates it anyway. Applies to new terminals.",
            history,
        ),
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
        entry(
            "Copy on select",
            "Copy text to the clipboard as soon as a selection is made.",
            copy_on_select,
        ),
    ]);

    rows
}

/// The Shell history picker: `auto` (per terminal unless the shell shares
/// history live), per terminal (`always`), or one shared history (`off`).
fn history_segments(mode: PerTerminalHistory) -> Vec<Segment> {
    [
        ("Auto", PerTerminalHistory::Auto),
        ("Per terminal", PerTerminalHistory::Always),
        ("Shared", PerTerminalHistory::Off),
    ]
    .into_iter()
    .map(|(label, value)| {
        Segment::new(label, mode == value, move |this, _w, cx| {
            this.terminal.per_terminal_history = value;
            this.persist_terminal(cx);
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_history_picker_writes_the_mode_to_terminal_toml(cx: &mut TestAppContext) {
        let (w, m) = super::super::env_editor_tests::modal(cx);
        let toml = crate::terminal_settings::app_data_dir().expect("test data dir").join("terminal.toml");
        let segments = history_segments(PerTerminalHistory::Auto);
        let labels: Vec<&str> = segments.iter().map(|s| s.label.as_ref()).collect();
        assert_eq!(labels, ["Auto", "Per terminal", "Shared"]);
        let selected: Vec<bool> = segments.iter().map(|s| s.selected).collect();
        assert_eq!(selected, [true, false, false]);
        for (idx, mode, text) in [(1, PerTerminalHistory::Always, "always"), (2, PerTerminalHistory::Off, "off")] {
            w.update(cx, |_, window, cx| m.update(cx, |m, cx| (segments[idx].on_select)(m, window, cx)))
                .unwrap();
            assert_eq!(m.read_with(cx, |m, _| m.terminal.per_terminal_history), mode);
            let written = std::fs::read_to_string(&toml).expect("terminal.toml");
            assert!(written.contains(&format!("per_terminal_history = \"{text}\"")), "{written}");
        }
        // Back to the default, so the shared test data dir keeps defaults.
        w.update(cx, |_, window, cx| m.update(cx, |m, cx| (segments[0].on_select)(m, window, cx))).unwrap();
        assert_eq!(m.read_with(cx, |m, _| m.terminal.per_terminal_history), PerTerminalHistory::Auto);
    }
}
