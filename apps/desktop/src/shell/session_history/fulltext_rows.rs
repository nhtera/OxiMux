//! Rendering for the picker's full-text mode: hit rows with a highlighted
//! snippet, the results bar (count + sort), the scope chips, the hit preview,
//! and the hint that points at the setting when the index is off.

use gpui::{
    AnyElement, Entity, HighlightStyle, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Styled, StyledText, div, prelude::FluentBuilder, px,
};
use gpui_component::Icon;
use oximux_agents::session_search::{Role, SearchHit, Sort, Span};
use oximux_settings::{Density, Theme, Typography};

use super::fulltext::{HistoryScope, hit_entry};
use super::{SessionHistoryEvent, SessionHistoryModal, picker};
use crate::shell::agent_ui::agent_presentation::adapter_icon_path;

/// Hit rows carry a snippet line, so they are taller than title rows.
pub const HIT_ROW_HEIGHT: f32 = 64.0;

pub struct Ctx<'a> {
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
    pub entity: Entity<SessionHistoryModal>,
    pub now_ms: i64,
    pub home: Option<&'a str>,
}

fn role_prefix(role: Role) -> &'static str {
    match role {
        Role::User => "You: ",
        Role::Assistant => "Agent: ",
        Role::Tool => "Tool: ",
    }
}

/// `prefix` + snippet spans as one string with the hit ranges highlighted.
fn snippet_text(prefix: &str, spans: &[Span], cx: &Ctx<'_>) -> AnyElement {
    let mut text = prefix.to_string();
    let mut ranges = Vec::new();
    for s in spans {
        let start = text.len();
        text.push_str(&s.text);
        if s.hit {
            ranges.push(start..text.len());
        }
    }
    let hl = HighlightStyle {
        color: Some(cx.theme.fg_base),
        font_weight: Some(cx.typography.w_semibold),
        background_color: Some(cx.theme.selection),
        ..Default::default()
    };
    StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, hl))).into_any_element()
}

/// One clipped line (flex-row wrapper + min-w-0 child: the single-line clip
/// that does not collapse).
fn line(child: impl IntoElement, size: f32, color: gpui::Hsla) -> impl IntoElement {
    div().flex().flex_row().w_full().child(
        div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(size))
            .text_color(color)
            .child(child),
    )
}

pub fn hit_row(i: usize, hit: &SearchHit, selected: bool, show_all: bool, stale: bool, cx: &Ctx<'_>) -> AnyElement {
    let entry = hit_entry(hit);
    let title = picker::session_row_title(&entry);
    let mut meta = picker::session_row_subtitle(&entry, cx.now_ms, show_all, cx.home);
    meta.push_str(&format!(" · {} msgs", hit.message_count));
    let icon = Icon::default()
        .path(adapter_icon_path(picker::entry_slug(&entry)))
        .size(px(15.))
        .flex_shrink_0()
        .text_color(cx.theme.fg_muted);
    let ent = cx.entity.clone();
    div()
        .id(("session-hit", i))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(9.))
        .h(px(HIT_ROW_HEIGHT))
        .w_full()
        .px(px(10.))
        .rounded(px(cx.density.r_xs))
        .cursor_pointer()
        .when(selected, |d| d.bg(cx.theme.selection))
        .when(!selected, |d| d.hover(|s| s.bg(cx.theme.hover_overlay)))
        // Rows answering the previous query wait for the new one.
        .when(stale, |d| d.opacity(0.45))
        .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
            ent.update(cx, |m, cx| m.import_selected(i, window, cx));
        })
        .child(icon)
        .child(
            div()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.))
                .flex_1()
                .min_w_0()
                .child(line(title, cx.typography.t_body_md, cx.theme.fg_base))
                .child(line(
                    snippet_text(role_prefix(hit.role), &hit.snippet, cx),
                    cx.typography.t_sub_label,
                    cx.theme.fg_muted,
                ))
                .child(line(meta, cx.typography.t_sub_label, cx.theme.fg_subtle)),
        )
        .into_any_element()
}

fn chip(id: &'static str, label: &'static str, active: bool, cx: &Ctx<'_>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(8.))
        .py(px(2.))
        .rounded(px(cx.density.r_xs))
        .cursor_pointer()
        .text_size(px(cx.typography.t_sub_label))
        .when(active, |d| d.bg(cx.theme.selection).text_color(cx.theme.fg_base))
        .when(!active, |d| d.text_color(cx.theme.fg_subtle).hover(|s| s.bg(cx.theme.hover_overlay)))
        .child(label)
}

/// `{n} results` + the sort chips, and the indexing caveat when it applies.
pub fn results_bar(m: &SessionHistoryModal, cx: &Ctx<'_>) -> AnyElement {
    let ft = &m.fulltext;
    let count = if ft.loading && ft.hits.is_empty() {
        "Searching…".to_string()
    } else {
        format!("{} result{}", ft.total, if ft.total == 1 { "" } else { "s" })
    };
    let sort_chip = |id: &'static str, label: &'static str, sort: Sort| {
        let ent = cx.entity.clone();
        chip(id, label, ft.sort == sort, cx).on_mouse_down(MouseButton::Left, move |_e, window, cx| {
            cx.stop_propagation();
            let handle = ent.update(cx, |m, cx| {
                m.set_fulltext_sort(sort, cx);
                m.query_focus_handle(cx)
            });
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        })
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .py(px(4.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(cx.typography.t_sub_label))
                        .text_color(cx.theme.fg_subtle)
                        .child(count),
                )
                .child(sort_chip("hits-sort-relevance", "Most relevant", Sort::Relevance))
                .child(sort_chip("hits-sort-newest", "Newest", Sort::Newest)),
        )
        .when_some(ft.error.clone().filter(|_| !ft.hits.is_empty()), |d, err| {
            d.child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(cx.typography.t_sub_label))
                    .text_color(cx.theme.status_error)
                    .child(format!("Search failed: {err}")),
            )
        })
        .when(ft.indexing, |d| {
            d.child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(cx.typography.t_sub_label))
                    .text_color(cx.theme.status_warning)
                    .child("Indexing… results may be incomplete"),
            )
        })
        .into_any_element()
}

/// Scope chips for the header (index on); ⌃A cycles the same set.
pub fn scope_chips(m: &SessionHistoryModal, cx: &Ctx<'_>) -> AnyElement {
    let mut row = div().flex().flex_row().items_center().gap(px(4.)).flex_shrink_0();
    for scope in m.offered_scopes() {
        let id = match scope {
            HistoryScope::Worktree => "scope-worktree",
            HistoryScope::Project => "scope-project",
            HistoryScope::All => "scope-all",
        };
        let ent = cx.entity.clone();
        row = row.child(chip(id, scope.label(), m.scope == scope, cx).on_mouse_down(
            MouseButton::Left,
            move |_e, window, cx| {
                cx.stop_propagation();
                let handle = ent.update(cx, |m, cx| {
                    m.set_scope(scope, cx);
                    m.query_focus_handle(cx)
                });
                window.defer(cx, move |window, cx| window.focus(&handle, cx));
            },
        ));
    }
    row.into_any_element()
}

/// "Load more" (or a loading line) after the last hit.
pub fn load_more_row(m: &SessionHistoryModal, cx: &Ctx<'_>) -> Option<AnyElement> {
    let ft = &m.fulltext;
    ft.next_cursor.as_ref()?;
    let ent = cx.entity.clone();
    let label = if ft.loading { "Loading…" } else { "Load more" };
    Some(
        div()
            .id("hits-load-more")
            .flex()
            .items_center()
            .justify_center()
            .h(px(32.))
            .w_full()
            .rounded(px(cx.density.r_xs))
            .cursor_pointer()
            .text_size(px(cx.typography.t_sub_label))
            .text_color(cx.theme.fg_muted)
            .hover(|s| s.bg(cx.theme.hover_overlay))
            .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
                cx.stop_propagation();
                // The click blurs the modal's focus root; hand focus back to
                // the search field so the keys keep working.
                let handle = ent.update(cx, |m, cx| {
                    m.load_more_fulltext(cx);
                    m.query_focus_handle(cx)
                });
                window.defer(cx, move |window, cx| window.focus(&handle, cx));
            })
            .child(label)
            .into_any_element(),
    )
}

/// Points at the setting when the index is off and the user is searching.
pub fn enable_hint_row(cx: &Ctx<'_>) -> AnyElement {
    let ent = cx.entity.clone();
    div()
        .id("history-enable-search")
        .flex()
        .items_center()
        .min_h(px(36.))
        .px(px(10.))
        .rounded(px(cx.density.r_xs))
        .cursor_pointer()
        .text_size(px(cx.typography.t_sub_label))
        .text_color(cx.theme.fg_subtle)
        .hover(|s| s.bg(cx.theme.hover_overlay).text_color(cx.theme.fg_muted))
        .on_mouse_down(MouseButton::Left, move |_e, _window, cx| {
            cx.stop_propagation();
            ent.update(cx, |m, cx| {
                m.close(cx);
                cx.emit(SessionHistoryEvent::OpenSessionSearchSettings);
            });
        })
        .child("Enable session search in Settings › Agents to search inside conversations")
        .into_any_element()
}

/// Preview pane for a hit: title, meta, the full snippet, and the jump chip
/// when an open tab already runs the session.
pub fn hit_preview(hit: &SearchHit, live: bool, cx: &Ctx<'_>) -> AnyElement {
    let entry = hit_entry(hit);
    let mut meta = picker::session_row_subtitle(&entry, cx.now_ms, true, cx.home);
    meta.push_str(&format!(" · {} messages", hit.message_count));
    let label = match hit.role {
        Role::User => "You",
        Role::Assistant => "Agent",
        Role::Tool => "Tool",
    };
    let ent = cx.entity.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .w_full()
                        .text_size(px(cx.typography.t_body_lg))
                        .text_color(cx.theme.fg_base)
                        .child(picker::session_row_title(&entry)),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(cx.typography.t_sub_label))
                        .text_color(cx.theme.fg_subtle)
                        .child(meta),
                ),
        )
        .child(div().w_full().h(px(1.)).bg(cx.theme.border_inactive))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .w_full()
                .child(div().text_size(px(cx.typography.t_sub_label)).text_color(cx.theme.fg_muted).child(label))
                .child(
                    div()
                        .w_full()
                        .text_size(px(cx.typography.t_body_md))
                        .text_color(cx.theme.fg_muted)
                        .child(snippet_text("", &hit.snippet, cx)),
                ),
        )
        .when(live, |d| {
            d.child(
                div()
                    .id("hit-jump")
                    .flex_none()
                    .self_start()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(cx.density.r_chip))
                    .border_1()
                    .border_color(cx.theme.border_inactive)
                    .cursor_pointer()
                    .text_size(px(cx.typography.t_body_sm))
                    .text_color(cx.theme.fg_base)
                    .hover(|s| s.border_color(cx.theme.border_active))
                    .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
                        cx.stop_propagation();
                        ent.update(cx, |m, cx| m.jump_to_live_tab(window, cx));
                    })
                    .child("↵ Jump to open tab"),
            )
        })
        .into_any_element()
}
