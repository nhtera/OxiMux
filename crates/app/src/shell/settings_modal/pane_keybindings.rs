//! Read-only keybindings reference. Mirrors the chords registered in
//! `keymap.rs`; kept here as a flat table because `gpui::KeyBinding`
//! does not expose a human action label for display. Update both when a
//! binding changes.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

/// (chord, action description). Order follows `keymap.rs`.
const BINDINGS: &[(&str, &str)] = &[
    ("⌘,", "Open settings"),
    ("⌘P", "Quick Open (files)"),
    ("⌘⇧P", "Command palette"),
    ("⌘O", "Open project"),
    ("⌘⇧N", "New workspace"),
    ("⌘N", "New window"),
    ("⌘T", "New tab"),
    ("⌘W", "Close tab / sub-pane"),
    ("⌘⇧W", "Close pane group"),
    ("⌘D", "Split sub-pane right"),
    ("⌘⇧D", "Split sub-pane down"),
    ("⌘]", "Focus next sub-pane"),
    ("⌘[", "Focus previous sub-pane"),
    ("⌘⇧]", "Focus next pane group"),
    ("⌘⇧[", "Focus previous pane group"),
    ("⌘⇧↩", "Zoom sub-pane"),
    ("⌘}", "Next tab"),
    ("⌘{", "Previous tab"),
    ("⌃Tab", "Most-recent tab"),
    ("⌃⇧Tab", "Least-recent tab"),
    ("⌘F", "Search scrollback"),
    ("⌘B", "Toggle left sidebar"),
    ("⌘L", "Toggle right sidebar"),
    ("⌘⇧E", "Explorer tab"),
    ("⌘⇧F", "Search tab"),
    ("⌘⇧G", "Source control tab"),
    ("⌘K", "Commit dialog"),
    ("⌘⇧A", "New agent"),
    ("⌘⇧I", "Send selection to agent"),
    ("⌘⇧O", "Send last output to agent"),
    ("⌘R", "Refresh source control"),
    ("⌘S", "Save file"),
    ("⌃⇧1", "Layout: stacked"),
    ("⌃⇧2", "Layout: side-by-side"),
    ("⌃⇧3", "Layout: bottom terminal"),
    ("Esc", "Dismiss overlay"),
];

pub(super) fn render(theme: Theme, _density: Density, typography: &Typography) -> AnyElement {
    let mut col = div().flex().flex_col();
    for (chord, action) in BINDINGS {
        col = col.child(row(chord, action, theme, typography));
    }
    col.into_any_element()
}

fn row(
    chord: &str,
    action: &str,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .py(px(5.0))
        .child(
            div()
                .w(px(96.0))
                .flex_none()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(chord.to_string())),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(action.to_string())),
        )
}
