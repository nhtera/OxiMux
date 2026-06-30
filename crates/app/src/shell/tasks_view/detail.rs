//! Issue/PR detail view — shown in the Tasks pane when a row is opened.
//!
//! Replaces the table for a single issue/PR. Top-to-bottom:
//!   1. Breadcrumb row — back link · `owner / repo · #number` · the `↗` /
//!      `+ Workspace` actions.
//!   2. Title         — the issue/PR title.
//!   3. Status line   — state chip (`● Open`), `@author opened this … · updated
//!      <ago>`, assignees, and label chips (wraps when narrow).
//!   4. Body          — the markdown-rendered description, framed in a card.
//!
//! The row's [`ForgeItem`] metadata renders immediately; the body + author
//! stream in via [`super::TasksView::open_detail`].
//!
//! Field access into `TasksView` is direct (this is a descendant module of
//! `tasks_view`); interaction goes through `pub(super)` methods/builders.
//!
//! Kept whole despite running over the file-size soft cap: it is a single
//! cohesive view — one composition (`render_detail`), the body card
//! (`render_body`), and two pure string helpers — and splitting one screen's
//! layout across files would add more import surface than it removes.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled, div,
    px,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};

use crate::shell::forge::ref_parse::parse_forge_ref;
use crate::shell::session_merge::relative_age_long;
use crate::shell::tasks_view::row::{create_action, open_action, state_color, workspace_name_for};
use crate::shell::tasks_view::{TaskKind, TasksView};

/// Render the detail view for `view.selected`. No-op element when nothing is
/// selected (the caller only invokes this when `selected.is_some()`).
pub(super) fn render_detail(view: &TasksView, cx: &mut Context<TasksView>) -> AnyElement {
    let Some(item) = view.selected.as_ref() else {
        return div().into_any_element();
    };
    let theme = view.theme;
    let density = view.density;
    let typo = &view.typography;
    // One clock read per render for the relative "updated <ago>" label.
    let now = chrono::Utc::now().to_rfc3339();

    // ----- Row 1: breadcrumb (back · owner/repo) + right-aligned actions -----
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

    // `owner / repo · #number` — the repo half is parsed from the item URL
    // (the same parser the prefill uses) and dropped for an unrecognized URL,
    // so the number always anchors the breadcrumb.
    let context = match repo_breadcrumb(&item.url) {
        Some(repo) => format!("{repo} \u{00b7} #{}", item.number),
        None => format!("#{}", item.number),
    };
    let mut breadcrumb = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .child(back)
        .child(
            div()
                .flex_none()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(context),
        );
    breadcrumb = breadcrumb
        .child(div().flex_1())
        .child(open_action(item.url.clone(), theme, density, typo));
    if let Some(project) = view.project.clone() {
        breadcrumb = breadcrumb.child(create_action(
            workspace_name_for(view.kind, item),
            format!("#{}", item.number),
            view.weak_root.clone(),
            project,
            theme,
            density,
            typo,
        ));
    }

    // ----- Row 2: title (the number lives in the breadcrumb above) -----
    let title = div()
        .w_full()
        .text_size(px(typo.t_body_lg))
        .font_weight(typo.w_semibold)
        .text_color(theme.fg_base)
        .child(item.title.clone());

    // ----- Row 3: status line (state · author/updated · assignees · labels) --
    let noun = match view.kind {
        TaskKind::Issues => "issue",
        TaskKind::Prs => "pull request",
    };
    let state_chip = div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_overlay)
        .text_size(px(typo.t_label_xs))
        .text_color(state_color(&item.state, theme))
        .child(format!("\u{25cf} {}", titlecase(&item.state)));

    let mut meta = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(state_chip);

    // `@author` (bold) followed by the muted "opened this … · updated <ago>"
    // phrase. The author streams in with the body, so it's omitted until known.
    if let Some(login) = view
        .detail
        .as_ref()
        .map(|d| d.author.login.clone())
        .filter(|l| !l.is_empty())
    {
        meta = meta.child(
            div()
                .flex_none()
                .font_weight(typo.w_semibold)
                .text_color(theme.fg_base)
                .child(format!("@{login}")),
        );
    }
    let updated = relative_age_long(&item.updated_at, &now);
    let opened = if updated.is_empty() {
        format!("opened this {noun}")
    } else {
        format!("opened this {noun} \u{00b7} updated {updated}")
    };
    meta = meta.child(div().flex_none().child(opened));

    // Assignees as plain `@login` text (distinct from the bg-filled labels).
    for assignee in item.assignees.iter().take(3) {
        meta = meta.child(
            div()
                .flex_none()
                .text_color(theme.fg_muted)
                .child(format!("@{}", assignee.login)),
        );
    }
    // Labels as filled chips.
    for label in item.labels.iter().take(6) {
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
        .child(breadcrumb)
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
/// GitHub-flavored-markdown body framed in a card. `id` discriminates the
/// renderer's keyed state per issue/PR so two opens never share it.
fn render_body(view: &TasksView, id: usize) -> AnyElement {
    let theme = view.theme;
    let density = view.density;
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
    // Frame the description in a card (a step up from `bg_panel`) so the body
    // reads as a distinct block beneath the header rather than floating loose.
    div()
        .id(("tasks-detail-body", id))
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .mx(px(density.pad_panel))
        .my(px(density.pad_panel))
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel_alt)
        .p(px(density.pad_panel))
        .child(
            TextView::markdown(("tasks-detail-md", id), body)
                .style(style)
                .scrollable(true),
        )
        .into_any_element()
}

/// `owner / repo` (or a GitLab group path) for the breadcrumb, parsed from the
/// item URL. `None` for an unrecognized URL, so the caller drops the segment.
fn repo_breadcrumb(url: &str) -> Option<String> {
    let repo = parse_forge_ref(url)?.repo?;
    Some(repo.replace('/', " / "))
}

/// Title-case a forge state word (`OPEN` / `open` → `Open`) for the state chip.
/// ASCII-only input (the forge states), so byte-wise capitalization is safe.
fn titlecase(state: &str) -> String {
    let mut chars = state.chars();
    match chars.next() {
        Some(first) => {
            let rest = chars.as_str().to_ascii_lowercase();
            format!("{}{rest}", first.to_ascii_uppercase())
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{repo_breadcrumb, titlecase};

    #[test]
    fn titlecase_normalizes_forge_states() {
        assert_eq!(titlecase("OPEN"), "Open");
        assert_eq!(titlecase("closed"), "Closed");
        assert_eq!(titlecase("Merged"), "Merged");
        assert_eq!(titlecase(""), "");
    }

    #[test]
    fn repo_breadcrumb_parses_owner_repo() {
        assert_eq!(
            repo_breadcrumb("https://github.com/safishamsi/graphify/issues/1556").as_deref(),
            Some("safishamsi / graphify")
        );
        assert_eq!(
            repo_breadcrumb("https://github.com/foo/bar/pull/7").as_deref(),
            Some("foo / bar")
        );
    }

    #[test]
    fn repo_breadcrumb_none_for_unrecognized_url() {
        assert_eq!(repo_breadcrumb("not a url"), None);
    }
}
