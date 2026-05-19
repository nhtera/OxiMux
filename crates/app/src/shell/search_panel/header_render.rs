//! Search panel header: query input with inline icon + toggle pills, plus
//! labeled glob filter inputs and a summary banner.
//!
//! All click handlers live in `mod.rs` — this file only assembles the
//! `AnyElement`s and wires button listeners through `cx.listener`.

use crate::shell::search_panel::SearchPanel;
use crate::shell::search_panel::rg_runner::SearchResults;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _,
    Styled, Transformation, div, ease_in_out, percentage, px,
};
use gpui_component::{
    Icon, Sizable as _,
    input::{Input, InputState},
    tooltip::Tooltip,
};
use std::time::Duration;

/// Which boolean toggle a button maps to.
#[derive(Clone, Copy)]
pub(in crate::shell) enum HeaderToggle {
    CaseSensitive,
    WholeWord,
    UseRegex,
}

/// Body text size used by the query/glob inputs. Matches the surrounding
/// summary/label rhythm so the panel reads as one type scale.
const INPUT_TEXT_PX: f32 = 11.0;
/// Inline icon-toggle square. Tight enough to keep all three pills inside
/// the input's right gutter without crowding the clear (×) button.
const TOGGLE_SIZE_PX: f32 = 18.0;

pub(super) fn render_header(panel: &SearchPanel, cx: &mut Context<SearchPanel>) -> AnyElement {
    let theme = panel.theme();
    let typography = panel.typography_ref();
    let opts = panel.options();

    // The first slot in the suffix strip flips between a spinner (search in
    // flight) and an inline clear (×) button. Both share the same square so
    // the layout doesn't shift between states.
    let has_text = !opts.query.is_empty();
    let leading: Option<AnyElement> = if panel.is_loading() {
        Some(spinner_slot(theme))
    } else if has_text {
        Some(clear_slot(theme, cx))
    } else {
        None
    };

    // Inline icon toggles. A custom div-toggle is used (instead of the
    // upstream Button widget) because Ghost's `selected_active` bg is too
    // close to the input fill to read — we need a stronger elevation.
    let toggle_strip = div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .children(leading)
        .child(toggle_pill(
            "sp-toggle-case",
            "icons/case-sensitive.svg",
            "Match Case",
            opts.case_sensitive,
            HeaderToggle::CaseSensitive,
            theme,
            cx,
        ))
        .child(toggle_pill(
            "sp-toggle-word",
            "icons/whole-word.svg",
            "Match Whole Word",
            opts.whole_word,
            HeaderToggle::WholeWord,
            theme,
            cx,
        ))
        .child(toggle_pill(
            "sp-toggle-regex",
            "icons/asterisk.svg",
            "Use Regular Expression",
            opts.use_regex,
            HeaderToggle::UseRegex,
            theme,
            cx,
        ));

    let query_row = div().px(px(8.0)).pt(px(8.0)).pb(px(4.0)).w_full().child(
        Input::new(panel.query_input_ref())
            .small()
            .shadow_none()
            .text_size(px(INPUT_TEXT_PX))
            .prefix(
                Icon::default()
                    .path("icons/search.svg")
                    .small()
                    .text_color(theme.fg_subtle),
            )
            .suffix(toggle_strip),
    );

    let include_row = filter_section(
        "FILES TO INCLUDE",
        panel.include_input_ref(),
        theme,
        typography,
    );
    let exclude_row = filter_section(
        "FILES TO EXCLUDE",
        panel.exclude_input_ref(),
        theme,
        typography,
    );

    let summary = match panel.summary_state() {
        SummaryState::Loading => summary_banner("Searching…", theme, typography),
        SummaryState::Idle => div().into_any_element(),
        SummaryState::RgMissing => summary_banner(
            "Install ripgrep to enable search (`brew install ripgrep`).",
            theme,
            typography,
        ),
        SummaryState::Error(msg) => {
            summary_banner(&format!("Search failed: {msg}"), theme, typography)
        }
        SummaryState::Results(r) => summary_banner(&format_summary(r), theme, typography),
    };

    div()
        .flex()
        .flex_col()
        .w_full()
        .bg(theme.bg_panel)
        .child(query_row)
        .child(include_row)
        .child(exclude_row)
        .child(summary)
        .into_any_element()
}

fn filter_section(
    label: &'static str,
    input: &gpui::Entity<InputState>,
    theme: oximux_settings::Theme,
    typography: &oximux_settings::Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(8.0))
        .pt(px(6.0))
        .w_full()
        .child(
            div()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(label)),
        )
        .child(
            Input::new(input)
                .small()
                .shadow_none()
                .text_size(px(INPUT_TEXT_PX)),
        )
        .into_any_element()
}

/// Single icon-toggle pill. Selected state uses `bg_overlay` + `border_active`
/// for clear visual separation from the input field's fill.
fn toggle_pill(
    id: &'static str,
    icon_path: &'static str,
    tooltip: &'static str,
    active: bool,
    which: HeaderToggle,
    theme: oximux_settings::Theme,
    cx: &mut Context<SearchPanel>,
) -> AnyElement {
    let (bg, border, fg) = if active {
        (theme.bg_overlay, theme.border_active, theme.fg_base)
    } else {
        (
            gpui::transparent_black(),
            gpui::transparent_black(),
            theme.fg_muted,
        )
    };

    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .items_center()
        .justify_center()
        .w(px(TOGGLE_SIZE_PX))
        .h(px(TOGGLE_SIZE_PX))
        .rounded(px(3.0))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .hover(|s| s.bg(theme.bg_panel_alt))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |me, _: &MouseDownEvent, window, cx| {
                me.toggle_flag(which, window, cx);
            }),
        )
        .child(Icon::default().path(icon_path).xsmall())
        .into_any_element()
}

/// Circular-arc loader rotated continuously. Sized to match `toggle_pill` so
/// the suffix slots don't jitter when the spinner appears or disappears.
fn spinner_slot(theme: oximux_settings::Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(TOGGLE_SIZE_PX))
        .h(px(TOGGLE_SIZE_PX))
        .text_color(theme.fg_muted)
        .child(
            Icon::default()
                .path("icons/loader-circle.svg")
                .xsmall()
                .with_animation(
                    "sp-spinner",
                    Animation::new(Duration::from_secs_f32(0.9))
                        .repeat()
                        .with_easing(ease_in_out),
                    |this, delta| this.transform(Transformation::rotate(percentage(delta))),
                ),
        )
        .into_any_element()
}

/// Inline clear (×) — click resets the query and re-focuses the input.
fn clear_slot(theme: oximux_settings::Theme, cx: &mut Context<SearchPanel>) -> AnyElement {
    div()
        .id(ElementId::Name("sp-clear-query".into()))
        .flex()
        .items_center()
        .justify_center()
        .w(px(TOGGLE_SIZE_PX))
        .h(px(TOGGLE_SIZE_PX))
        .rounded(px(3.0))
        .text_color(theme.fg_muted)
        .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
        .tooltip(move |window, cx| Tooltip::new("Clear").build(window, cx))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |me, _: &MouseDownEvent, window, cx| {
                me.clear_query(window, cx);
            }),
        )
        .child(Icon::default().path("icons/circle-x.svg").xsmall())
        .into_any_element()
}

fn summary_banner(
    text: &str,
    theme: oximux_settings::Theme,
    typography: &oximux_settings::Typography,
) -> AnyElement {
    // Compact status line between filters and result list. A subtle bottom
    // border separates it from the result rows without a heavy divider.
    div()
        .flex()
        .items_center()
        .pt(px(8.0))
        .pb(px(6.0))
        .px(px(8.0))
        .border_b_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(text.to_string())
        .into_any_element()
}

fn format_summary(r: &SearchResults) -> String {
    let files = r.files.len();
    let plural = |n: usize, word: &str| {
        if n == 1 {
            format!("{n} {word}")
        } else {
            format!("{n} {word}s")
        }
    };
    let suffix = if r.truncated { " (truncated)" } else { "" };
    format!(
        "{} in {}{}",
        plural(r.total_matches, "result"),
        plural(files, "file"),
        suffix
    )
}

/// What banner the header should display. Computed in `mod.rs` and read by
/// `render_header` so the function stays a pure projection of panel state.
#[derive(Debug, Clone)]
pub(in crate::shell) enum SummaryState<'a> {
    /// No query in flight and no results to display.
    Idle,
    Loading,
    RgMissing,
    Error(String),
    Results(&'a SearchResults),
}
