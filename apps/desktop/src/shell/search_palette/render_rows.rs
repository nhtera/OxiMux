//! Row rendering for the search palette: section headers, tab / worktree /
//! project / setting / action rows, "N more" hints and the create row.
//! Pure composition over a frozen [`Snapshot`]; clicks go through the shared
//! `on_activate` callback so mouse and keyboard converge on one path.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, HighlightStyle, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Styled, StyledText, Window, div, prelude::FluentBuilder, px, rgb, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::workspace_row::status_dot_color;
use crate::shell::search_palette::model::{ItemRef, Snapshot, TabItem, WorktreeItem};
use crate::shell::search_palette::recency::ms_to_rfc3339;
use crate::shell::search_palette::sections::Entry;
use crate::shell::workspace::session_merge::relative_age_compact;

pub const ROW_HEIGHT: f32 = 36.0;
const HEADER_HEIGHT: f32 = 26.0;
const ICON_SIZE: f32 = 14.0;
/// The location chip truncates past this so the title keeps the row.
const LOCATION_MAX_W: f32 = 260.0;

/// Activate the entry at this index (click). Shared with keyboard Enter.
pub type ActivateFn = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Everything a row needs besides its entry.
pub struct RowCtx<'a> {
    pub snapshot: &'a Snapshot,
    /// `snapshot.now_ms` as RFC-3339, for the shared age formatter.
    pub now: String,
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
    pub on_activate: ActivateFn,
}

pub fn render_entry(idx: usize, entry: &Entry, selected: bool, cx: &RowCtx<'_>) -> AnyElement {
    match entry {
        Entry::Header(section) => header(section.title(), cx).into_any_element(),
        Entry::Item { item, title_ranges, digit } => {
            let body = match *item {
                ItemRef::Tab(i) => tab_row(&cx.snapshot.tabs[i], title_ranges, *digit, cx),
                ItemRef::Worktree(i) => worktree_row(&cx.snapshot.worktrees[i], title_ranges, cx),
                ItemRef::Project(i) => {
                    let p = &cx.snapshot.projects[i];
                    let mut row = row_body()
                        .child(icon("icons/folder.svg", cx.theme.fg_subtle))
                        .child(title(&p.name, title_ranges, cx))
                        .child(muted(&p.root_path, cx));
                    if p.is_current {
                        row = row.child(badge("Current Project", cx));
                    }
                    row.into_any_element()
                }
                ItemRef::Setting(i) => {
                    let s = &cx.snapshot.settings[i];
                    row_body()
                        .child(icon(s.icon, cx.theme.fg_subtle))
                        .child(title(s.label, title_ranges, cx))
                        .child(badge("Settings", cx))
                        .into_any_element()
                }
                ItemRef::Action(i) => {
                    let a = &cx.snapshot.actions[i];
                    let mut row = row_body()
                        .child(icon("icons/play.svg", cx.theme.fg_subtle))
                        .child(title(&a.label, title_ranges, cx))
                        .child(badge("Action", cx));
                    if let Some(chord) = &a.chord {
                        row = row.child(div().ml_auto().flex_shrink_0().child(keycap(chord, cx)));
                    }
                    row.into_any_element()
                }
            };
            shell(idx, selected, body, cx)
        }
        Entry::More { hidden, .. } => {
            let body = row_body()
                .child(div().w(px(ICON_SIZE)).flex_shrink_0())
                .child(muted(&format!("{hidden} more"), cx))
                .child(pill("See more", cx))
                .into_any_element();
            shell(idx, selected, body, cx)
        }
        Entry::CreateWorktree(name) => {
            let body = row_body()
                .child(icon("icons/plus.svg", cx.theme.fg_subtle))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(cx.typography.t_body_sm))
                        .text_color(cx.theme.fg_muted)
                        .child(format!("Create worktree \u{201c}{name}\u{201d}")),
                )
                .into_any_element();
            shell(idx, selected, body, cx)
        }
    }
}

fn tab_row(t: &TabItem, ranges: &[Range<usize>], digit: Option<u8>, cx: &RowCtx<'_>) -> AnyElement {
    let mut lead = row_body().child(
        div()
            .relative()
            .flex_shrink_0()
            .child(icon(t.icon, cx.theme.fg_muted))
            .when(t.needs_attention, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(-2.))
                        .right(px(-2.))
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme.status_warn),
                )
            }),
    );
    lead = lead.child(title(&t.title, ranges, cx)).child(age(t.last_ms, cx));
    if t.is_current {
        lead = lead.child(badge("Current Tab", cx));
    } else if t.in_current_worktree {
        lead = lead.child(badge("Current Worktree", cx));
    }
    let location = if t.worktree_name.is_empty() || t.worktree_name == t.project_name {
        t.project_name.clone()
    } else {
        format!("{} \u{b7} {}", t.project_name, t.worktree_name)
    };
    let mut right = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .flex_shrink_0()
        .child(location_chip(&location, t.tint, cx));
    if let Some(d) = digit {
        right = right.child(keycap(&format!("\u{2318}{d}"), cx));
    }
    lead.child(div().flex_1().min_w(px(8.))).child(right).into_any_element()
}

fn worktree_row(w: &WorktreeItem, ranges: &[Range<usize>], cx: &RowCtx<'_>) -> AnyElement {
    let dot = status_dot_color(w.status.as_ref(), w.is_live, cx.theme);
    let mut lead = row_body()
        .child(
            div()
                .w(px(ICON_SIZE))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(div().size(px(7.)).rounded_full().bg(dot)),
        )
        .child(title(&w.name, ranges, cx))
        .child(age(w.last_visited_ms.unwrap_or(w.last_activity_ms).max(w.last_activity_ms), cx));
    if w.is_primary {
        lead = lead.child(badge("primary", cx));
    }
    if w.is_current {
        lead = lead.child(badge("Current Worktree", cx));
    }
    if !w.branch.is_empty() && w.branch != w.name {
        lead = lead.child(muted(&format!("\u{b7} {}", w.branch), cx));
    }
    lead.child(div().flex_1().min_w(px(8.)))
        .child(div().flex_shrink_0().child(location_chip(&w.project_name, w.tint, cx)))
        .into_any_element()
}

fn header(label: &str, cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_end()
        .h(px(HEADER_HEIGHT))
        .flex_shrink_0()
        .px(px(10.))
        .pb(px(4.))
        .text_size(px(cx.typography.t_sub_label))
        .font_weight(cx.typography.w_semibold)
        .text_color(cx.theme.fg_subtle)
        .child(label.to_string())
}

/// Selectable row container: fixed height, selection fill + 1px inset ring,
/// hover lift, click → activate.
fn shell(idx: usize, selected: bool, body: AnyElement, cx: &RowCtx<'_>) -> AnyElement {
    let on_activate = cx.on_activate.clone();
    div()
        .id(("search-palette-row", idx))
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .flex_shrink_0()
        .px(px(10.))
        .rounded(px(cx.density.r_xs))
        .border_1()
        .overflow_hidden()
        .cursor_pointer()
        .when(selected, |d| d.bg(cx.theme.selection).border_color(cx.theme.border_active))
        .when(!selected, |d| d.border_color(gpui::transparent_black()).hover(|s| s.bg(cx.theme.hover_overlay)))
        .on_mouse_down(MouseButton::Left, move |_e, window, app| on_activate(idx, window, app))
        .child(body)
        .into_any_element()
}

fn row_body() -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(8.)).w_full().min_w_0()
}

fn icon(path: &'static str, color: gpui::Hsla) -> impl IntoElement {
    svg().path(path).size(px(ICON_SIZE)).flex_shrink_0().text_color(color)
}

/// Title with matched ranges emphasised. Clips instead of truncating: a
/// `.truncate()` measured at its own width paints as a lone `…`.
fn title(text: &str, ranges: &[Range<usize>], cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(cx.typography.t_body_sm))
        .font_weight(cx.typography.w_medium)
        .text_color(cx.theme.fg_base)
        .child(highlighted_spans(text, ranges, cx.theme, cx.typography))
}

/// `text` with `ranges` (byte offsets) rendered heavier and brighter.
pub fn highlighted_spans(text: &str, ranges: &[Range<usize>], theme: Theme, typography: &Typography) -> AnyElement {
    let valid: Vec<Range<usize>> = ranges
        .iter()
        .filter(|r| r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
        .cloned()
        .collect();
    if valid.is_empty() {
        return div().child(text.to_string()).into_any_element();
    }
    let hl = HighlightStyle {
        color: Some(theme.fg_base),
        font_weight: Some(typography.w_semibold),
        ..Default::default()
    };
    StyledText::new(text.to_string())
        .with_highlights(valid.into_iter().map(|r| (r, hl)))
        .into_any_element()
}

fn age(ms: i64, cx: &RowCtx<'_>) -> AnyElement {
    if ms <= 0 {
        return div().into_any_element();
    }
    let label = relative_age_compact(&ms_to_rfc3339(ms), &cx.now);
    div()
        .flex_shrink_0()
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_subtle)
        .child(label)
        .into_any_element()
}

fn muted(text: &str, cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_subtle)
        .child(text.to_string())
}

fn badge(label: &str, cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(cx.density.r_chip))
        .bg(cx.theme.bg_panel_alt)
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_muted)
        .child(label.to_string())
}

fn pill(label: &str, cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px(px(8.))
        .py(px(1.))
        .rounded(px(cx.density.r_chip))
        .border_1()
        .border_color(cx.theme.border_inactive)
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_muted)
        .child(label.to_string())
}

fn keycap(label: &str, cx: &RowCtx<'_>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px(px(6.))
        .py(px(1.))
        .bg(cx.theme.bg_panel)
        .border_1()
        .border_color(cx.theme.border_inactive)
        .rounded(px(cx.density.r_xs))
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_subtle)
        .child(label.to_string())
}

/// `▪ project · worktree` — tint square (the worktree's tab colour, else a
/// neutral swatch) then the location, truncated past [`LOCATION_MAX_W`].
fn location_chip(label: &str, tint: Option<u32>, cx: &RowCtx<'_>) -> impl IntoElement {
    let swatch = match tint {
        Some(hex) => div().size(px(8.)).rounded(px(2.)).bg(rgb(hex)),
        None => div().size(px(8.)).rounded(px(2.)).bg(cx.theme.fg_subtle),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .max_w(px(LOCATION_MAX_W))
        .px(px(6.))
        .py(px(1.))
        .rounded(px(cx.density.r_chip))
        .bg(cx.theme.bg_panel_alt)
        .child(swatch.flex_shrink_0())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(cx.typography.t_sub_label))
                .text_color(cx.theme.fg_muted)
                .child(label.to_string()),
        )
}
