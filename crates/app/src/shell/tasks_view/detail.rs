//! Issue/PR detail view — shown in the Tasks pane when a row is opened.
//!
//! Replaces the table for a single issue/PR: a header (back link, `#number`,
//! state, title, author + label chips, the row's `↗` / `+ Workspace` actions)
//! over a markdown-rendered body. The row's [`ForgeItem`] metadata renders
//! immediately; the body streams in via [`super::TasksView::open_detail`].
//!
//! Field access into `TasksView` is direct (this is a descendant module of
//! `tasks_view`); interaction goes through `pub(super)` methods/builders.

use gpui::{
    AnyElement, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled, div, px,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use gpui::Context;

use crate::shell::tasks_view::TasksView;
use crate::shell::tasks_view::row::{create_action, open_action, state_color, workspace_name_for};

/// Render the detail view for `view.selected`. No-op element when nothing is
/// selected (the caller only invokes this when `selected.is_some()`).
pub(super) fn render_detail(view: &TasksView, cx: &mut Context<TasksView>) -> AnyElement {
    let Some(item) = view.selected.as_ref() else {
        return div().into_any_element();
    };
    let theme = view.theme;
    let density = view.density;
    let typo = &view.typography;

    // Header row: back · #number · state chip · spacer · actions.
    let back = div()
        .id("tasks-detail-back")
        .flex_none()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|tv, _ev, _window, cx| tv.close_detail(cx)),
        )
        .child("\u{2190} Back".to_string());

    let state_chip = div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_overlay)
        .text_size(px(typo.t_label_xs))
        .text_color(state_color(&item.state, theme))
        .child(item.state.to_ascii_lowercase());

    let mut top = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .child(back)
        .child(
            div()
                .flex_none()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(format!("#{}", item.number)),
        )
        .child(state_chip)
        .child(div().flex_1())
        .child(open_action(item.url.clone(), theme, density, typo));
    if let Some(project) = view.project.clone() {
        top = top.child(create_action(
            workspace_name_for(view.kind, item),
            format!("#{}", item.number),
            view.weak_root.clone(),
            project,
            theme,
            density,
            typo,
        ));
    }

    // Title (wraps naturally — no nowrap, so it's safe directly in a flex-col).
    let title = div()
        .w_full()
        .text_size(px(typo.t_body_lg))
        .font_weight(typo.w_semibold)
        .text_color(theme.fg_base)
        .child(item.title.clone());

    // Meta line: author (when known) + label chips.
    let author = view
        .detail
        .as_ref()
        .map(|d| d.author.login.clone())
        .filter(|l| !l.is_empty());
    let mut meta = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle);
    if let Some(login) = author {
        meta = meta.child(format!("opened by @{login}"));
    }
    for label in item.labels.iter().take(4) {
        meta = meta.child(
            div()
                .flex_none()
                .px(px(5.0))
                .rounded(px(density.r_chip))
                .bg(theme.bg_overlay)
                .text_color(theme.fg_muted)
                .child(label.name.clone()),
        );
    }

    let header = div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline))
        .w_full()
        .px(px(density.pad_panel))
        .py(px(8.0))
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(top)
        .child(title)
        .child(meta);

    let body = render_body(view, item.number as usize);

    div()
        .flex()
        .flex_col()
        .h_full()
        .w_full()
        .child(header)
        .child(body)
        .into_any_element()
}

/// The body region: a centered hint while loading or when empty, otherwise the
/// GitHub-flavored-markdown body in a scrollable region. `id` discriminates the
/// renderer's keyed state per issue/PR so two opens never share it.
fn render_body(view: &TasksView, id: usize) -> AnyElement {
    let theme = view.theme;
    let typo = &view.typography;
    let centered = |text: &str| -> AnyElement {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .w_full()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_subtle)
            .child(text.to_string())
            .into_any_element()
    };

    if view.detail_loading {
        return centered("Loading\u{2026}");
    }
    let body = view
        .detail
        .as_ref()
        .map(|d| d.body.clone())
        .unwrap_or_default();
    if body.trim().is_empty() {
        return centered("No description.");
    }

    // Dark-only app, so pin the renderer to the dark highlight theme (its own
    // default is a light code theme, which reads washed-out on `bg_panel`).
    let style = TextViewStyle {
        is_dark: true,
        highlight_theme: HighlightTheme::default_dark(),
        ..Default::default()
    };
    div()
        .id(("tasks-detail-body", id))
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .p(px(view.density.pad_panel))
        .child(
            TextView::markdown(("tasks-detail-md", id), body)
                .style(style)
                .scrollable(true),
        )
        .into_any_element()
}
