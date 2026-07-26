//! Row rendering for the palette modal — command rows, file rows, group
//! labels, and keybinding chips. Pure composition fed resolved state by the
//! owning entity; click activation flows through the shared `on_activate`
//! callback so mouse and keyboard converge on one dispatch path.

use std::rc::Rc;

use gpui::{
    App, HighlightStyle, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    StyledText, Window, div, prelude::FluentBuilder, px,
};
use oximux_settings::{Theme, Typography};

use crate::shell::command_palette::entry::PaletteItem;
use crate::shell::command_palette::match_engine::match_ranges;

/// Shared row geometry so command rows, file rows, and group labels read as
/// one component family.
pub const ROW_HEIGHT: f32 = 34.0;
pub const GROUP_LABEL_HEIGHT: f32 = 24.0;
const ROW_RADIUS: f32 = 5.0;

/// Row activation callback: dispatch the row's action at the given
/// filtered-list index AND close the modal. Shared by click and keyboard so
/// both converge on one path.
pub type ActivateFn = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// A single command row. Click and keyboard Enter both route through
/// `on_activate`, which dispatches the action AND closes the modal, so a
/// mouse click can't leave the palette open.
#[allow(clippy::too_many_arguments)]
pub fn palette_row(
    item: &PaletteItem,
    selected: bool,
    row_idx: usize,
    query: &str,
    theme: Theme,
    typography: &Typography,
    on_activate: ActivateFn,
) -> impl IntoElement {
    let fg = if selected { theme.fg_base } else { theme.fg_muted };

    let mut row = row_shell(selected, theme).cursor_pointer().on_mouse_down(
        MouseButton::Left,
        move |_event, window, cx| on_activate(row_idx, window, cx),
    );

    row = row.child(
        div()
            .flex_1()
            .text_size(px(typography.t_body_sm))
            .text_color(fg)
            .child(highlighted_label(&item.name, query, theme, typography)),
    );

    if let Some(kb) = item.keybinding.as_deref() {
        row = row.child(keybinding_chip(kb, theme, typography));
    }
    row
}

/// A Quick Open file row. Clickable like command rows; `on_activate` resolves
/// the relative path and opens an editor tab.
#[allow(clippy::too_many_arguments)]
pub fn file_row(
    path: &str,
    selected: bool,
    row_idx: usize,
    query: &str,
    actionable: bool,
    theme: Theme,
    typography: &Typography,
    on_activate: ActivateFn,
) -> impl IntoElement {
    let fg = if selected { theme.fg_base } else { theme.fg_muted };
    let mut row = row_shell(selected, theme);
    if actionable {
        row = row.cursor_pointer().on_mouse_down(
            MouseButton::Left,
            move |_event, window, cx| on_activate(row_idx, window, cx),
        );
    }
    row.child(
        div()
            .flex_1()
            .text_size(px(typography.t_body_sm))
            .text_color(fg)
            .child(highlighted_label(path, query, theme, typography)),
    )
}

/// Thin uppercase separator label between built-in and custom commands.
pub fn group_label(label: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(GROUP_LABEL_HEIGHT))
        .flex_shrink_0()
        .px(px(10.))
        .text_size(px(typography.t_sub_label))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_subtle)
        .child(label.to_uppercase())
}

/// Shared row container: fixed height, rounded, selection fill + hover.
/// Selection uses the dedicated selection tint (clearly above the card bg);
/// unselected rows light up on hover so the pointer target is obvious.
fn row_shell(selected: bool, theme: Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .h(px(ROW_HEIGHT))
        // Hold full height inside the capped, scrollable list — without this
        // a long catalog shrinks every row to fit instead of scrolling.
        .flex_shrink_0()
        .px(px(10.))
        .rounded(px(ROW_RADIUS))
        .when(selected, |d| d.bg(theme.selection))
        .when(!selected, |d| d.hover(|s| s.bg(theme.hover_overlay)))
}

/// Keybinding rendered as a faint inset chip (one pill for the whole combo,
/// e.g. `⌘⇧D`) so the shortcut reads as a key affordance, not body text.
fn keybinding_chip(kb: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(4.))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(kb.to_string())
}

/// Label with the matched query characters emphasized (brighter + heavier)
/// over the base row color. Falls back to a plain string when nothing matches
/// (empty query or no-match), avoiding a needless styled run.
fn highlighted_label(
    text: &str,
    query: &str,
    theme: Theme,
    typography: &Typography,
) -> gpui::AnyElement {
    let ranges = match_ranges(query, text);
    if ranges.is_empty() {
        return div().child(text.to_string()).into_any_element();
    }
    let hl = HighlightStyle {
        color: Some(theme.fg_base),
        font_weight: Some(typography.w_semibold),
        ..Default::default()
    };
    StyledText::new(text.to_string())
        .with_highlights(ranges.into_iter().map(|r| (r, hl)))
        .into_any_element()
}
