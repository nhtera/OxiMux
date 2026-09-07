//! The chat's "Add issue or pull request" picker: a modal list of the repo's
//! open issues and pull requests, opened from the composer's attach menu, whose
//! chosen item lands as a `@issue` / `@pull-request` context chip.
//!
//! Split of concerns matches the rest of the chat's context plumbing: the
//! composer only *asks* (it emits [`ComposerEvent::OpenForgePicker`]), because
//! listing needs the chat cwd and the forge CLI, and only the owning
//! [`AgentChatView`] has those. Everything here is either pure (the filter) or a
//! render of state the view holds.
//!
//! One listing pass, filtered in memory: issues and pull requests are fetched
//! together when the picker opens and the query box narrows what was fetched
//! rather than re-querying per keystroke. That keeps typing off the forge CLI,
//! which is a network round-trip per invocation.
//!
//! [`ComposerEvent::OpenForgePicker`]: super::composer::ComposerEvent::OpenForgePicker

use std::path::PathBuf;

use gpui::{
    div, px, relative, AnyElement, AppContext as _, Context, Entity, Focusable as _,
    InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_core::ForgeRefKind;

use crate::shell::forge::{Forge, ForgeKind, ForgeListFilter, ForgeProvider as _, ItemDetail};

use super::composer::ComposerView;
use super::{context_providers, AgentChatView};

/// Cap on rows kept from a listing. The forge CLI already pages at 50 per kind;
/// this bounds the *combined* list so a busy repo can't build a 100-row element
/// tree behind a filter the user is about to type into anyway.
const MAX_ROWS: usize = 60;
/// Max characters of a title rendered in a row before eliding.
const TITLE_MAX: usize = 72;
/// Fixed width of the picker card.
const CARD_W: f32 = 520.0;
/// Cap on the scrolling list's height.
const LIST_MAX_H: f32 = 320.0;

/// One issue / pull request offered in the picker. Flattened from
/// [`crate::shell::forge::ForgeItem`] at fetch time so the render path holds
/// plain data and the two forge backends converge on one row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForgeRow {
    pub kind: ForgeRefKind,
    pub number: u64,
    pub title: String,
    /// Login of whoever opened it. Empty when the forge omitted it.
    pub author: String,
}

impl ForgeRow {
    /// `issue` / `pull request` (GitLab: `merge request`), for the row's trailing
    /// kind tag. Takes the forge so the wording follows the host, the same rule
    /// the attach menu's own label follows.
    fn kind_label(&self, forge: ForgeKind) -> &'static str {
        match (self.kind, forge) {
            (ForgeRefKind::Issue, _) => "issue",
            (ForgeRefKind::Pull, ForgeKind::Github) => "pull request",
            (ForgeRefKind::Pull, ForgeKind::Gitlab) => "merge request",
        }
    }
}

/// The picker's live state, held by [`AgentChatView`] while it is open.
pub(super) struct ForgePicker {
    /// The query box.
    pub query: Entity<InputState>,
    /// Keeps the query box's `Change` subscription alive for the picker's
    /// lifetime. Without it the field is inert: an `InputState` only repaints
    /// its typed characters when the view that OWNS it notifies, and this view
    /// also has to re-run the filter on each keystroke.
    pub _sub: gpui::Subscription,
    /// Everything fetched, unfiltered. Empty while `loading`.
    pub rows: Vec<ForgeRow>,
    /// A listing is in flight.
    pub loading: bool,
    /// Which forge backs the repo — drives the card's wording.
    pub forge: ForgeKind,
    /// Bumped on each open so a listing the user has already dismissed (or
    /// reopened past) is discarded when it lands.
    pub generation: u64,
}

/// Rank `rows` against `query`, returning indices in display order.
///
/// An empty query keeps fetch order (the forge lists newest-first). A query of
/// digits — with or without a leading `#` — matches the number, so typing `42`
/// finds #42 rather than every title containing "42"; anything else is a
/// case-insensitive title substring. Number matches sort ahead of title matches
/// so an exact `#42` is never buried under prose that happens to mention it.
pub(super) fn filter_rows(rows: &[ForgeRow], query: &str) -> Vec<usize> {
    let q = query.trim().trim_start_matches('#').to_lowercase();
    if q.is_empty() {
        return (0..rows.len()).collect();
    }
    let mut by_number = Vec::new();
    let mut by_title = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.number.to_string().starts_with(&q) {
            by_number.push(i);
        } else if row.title.to_lowercase().contains(&q) {
            by_title.push(i);
        }
    }
    by_number.extend(by_title);
    by_number
}

/// Flatten a forge listing into rows, capped at [`MAX_ROWS`]. Both listings are
/// merged here (rather than shown in two tabs) because the user picking one
/// knows its number, not which of the two lists it lives in.
pub(super) fn rows_from_listings(
    issues: Vec<crate::shell::forge::ForgeItem>,
    pulls: Vec<crate::shell::forge::ForgeItem>,
) -> Vec<ForgeRow> {
    let map = |kind: ForgeRefKind| {
        move |it: crate::shell::forge::ForgeItem| ForgeRow {
            kind,
            number: it.number,
            title: it.title,
            author: it.author.login,
        }
    };
    let mut rows: Vec<ForgeRow> = issues
        .into_iter()
        .map(map(ForgeRefKind::Issue))
        .chain(pulls.into_iter().map(map(ForgeRefKind::Pull)))
        .collect();
    rows.truncate(MAX_ROWS);
    rows
}

/// Elide `title` to [`TITLE_MAX`] characters. Char-wise, not byte-wise, so a
/// title with non-ASCII cannot be cut mid-character.
fn elide(title: &str) -> String {
    if title.chars().count() <= TITLE_MAX {
        return title.to_string();
    }
    let kept: String = title.chars().take(TITLE_MAX.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

impl AgentChatView {
    /// Detect which forge (if any) hosts this chat's repo and push the answer to
    /// the composer, which uses it to word and gate the attach menu's issue row.
    ///
    /// `Forge::detect` is a local `git remote get-url` — no network — but it is
    /// still a subprocess, so it runs off the tokio runtime rather than on the
    /// construction path. A chat outside a repo simply never gets an answer, and
    /// the row stays hidden.
    /// An associated fn rather than a method: it runs from `assemble`, before
    /// the view value exists, and it needs nothing from the view but the two
    /// things passed in.
    pub(super) fn spawn_forge_detect(
        cwd: PathBuf,
        composer: Entity<ComposerView>,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<ForgeKind>>();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let _ = tx.send(Forge::detect(&cwd).await.map(|f| f.kind()));
        });
        cx.spawn(async move |_this, cx| {
            let kind = rx.await.unwrap_or(None);
            composer.update(cx, |c, cx| c.set_forge_kind(kind, cx));
        })
        .detach();
    }

    /// Open the issue / pull-request picker and start its listing.
    ///
    /// Both listings are fetched in one pass; the query box then filters what
    /// came back rather than re-querying, because each forge-CLI invocation is a
    /// network round-trip and a keystroke is not a reason to make one.
    pub(super) fn open_forge_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Without a detected forge there is nothing to list. The menu row is
        // hidden in that case, so this is the defensive half of the same rule.
        let Some(forge_kind) = self.composer.read(cx).forge_kind() else {
            return;
        };
        self.forge_picker_gen = self.forge_picker_gen.wrapping_add(1);
        let generation = self.forge_picker_gen;
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search by number or title…")
        });
        // The filter runs against what was typed, so each keystroke has to
        // repaint THIS view — the input's own entity notifying itself would
        // redraw the field and leave the list showing the previous query.
        let _sub = cx.subscribe(&query, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        let handle = query.read(cx).focus_handle(cx);
        self.forge_picker = Some(ForgePicker {
            query,
            _sub,
            rows: Vec::new(),
            loading: true,
            forge: forge_kind,
            generation,
        });
        // Focused after the picker is staged, so the field the user types into
        // is the one that is actually on screen.
        handle.focus(window, cx);
        cx.notify();

        let cwd = self.cwd.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<ForgeRow>>();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No reactor: leave the picker open showing its empty-state line
            // rather than closing under the user mid-gesture.
            if let Some(p) = self.forge_picker.as_mut() {
                p.loading = false;
            }
            cx.notify();
            return;
        };
        handle.spawn(async move {
            let rows = match Forge::detect(&cwd).await {
                Some(forge) => {
                    let filter = ForgeListFilter::default();
                    // Sequential, not joined: both shell out to the same CLI, and
                    // two concurrent invocations buy nothing over one after the
                    // other while doubling the peak process count.
                    let issues = forge.list_issues(&cwd, filter.clone()).await;
                    let pulls = forge.list_prs(&cwd, filter).await;
                    rows_from_listings(issues, pulls)
                }
                None => Vec::new(),
            };
            let _ = tx.send(rows);
        });
        self._forge_task = Some(cx.spawn(async move |this, cx| {
            let rows = rx.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                // Discard a listing the user has already dismissed or superseded.
                let Some(p) = this.forge_picker.as_mut().filter(|p| p.generation == generation)
                else {
                    return;
                };
                p.rows = rows;
                p.loading = false;
                cx.notify();
            });
        }));
    }

    /// Close the picker, dropping any listing still in flight with it and
    /// handing focus back to the composer — the picker's query box held it, so
    /// without this the next keystroke after a dismiss goes nowhere.
    pub(super) fn close_forge_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.forge_picker.take().is_some() {
            self._forge_task = None;
            let handle = self.composer.read(cx).focus_handle(cx);
            handle.focus(window, cx);
            cx.notify();
        }
    }

    /// Stage the picked issue / pull request as a context chip: close the picker
    /// now, fetch its body, and hand the chip to the composer when it lands.
    ///
    /// Closing first (rather than holding the modal open through the fetch) keeps
    /// the gesture responsive; the chip appearing above the composer a moment
    /// later is the same arrival `@diff` already has.
    fn stage_forge_item(
        &mut self,
        row: ForgeRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_forge_picker(window, cx);
        let cwd = self.cwd.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<ItemDetail>>();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (kind, number) = (row.kind, row.number);
        handle.spawn(async move {
            let detail = match Forge::detect(&cwd).await {
                Some(forge) => {
                    crate::shell::forge::fetch_item_detail(forge, &cwd, kind, number).await
                }
                None => None,
            };
            let _ = tx.send(detail);
        });
        self._forge_task = Some(cx.spawn(async move |this, cx| {
            let detail = rx.await.unwrap_or(None);
            let _ = this.update(cx, |this, cx| {
                // A body that couldn't be fetched still yields a usable chip: the
                // number and title alone tell the agent what to go read, which is
                // strictly better than dropping the user's pick on the floor.
                let (body, author) = detail
                    .map(|d| (d.body, d.author.login))
                    .unwrap_or_else(|| (String::new(), row.author.clone()));
                let chip = context_providers::forge_chip(
                    row.kind,
                    row.number,
                    &row.title,
                    &author,
                    &body,
                );
                this.composer.update(cx, |c, cx| c.add_context_chip(chip, cx));
            });
        }));
    }

    /// The picker overlay: a dark backdrop plus a centred card. `None` when the
    /// picker is closed.
    ///
    /// The backdrop closes on click; the card swallows its own clicks so a stray
    /// press inside it doesn't dismiss — the same split the image lightbox uses.
    pub(super) fn render_forge_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.forge_picker.as_ref()?;
        let theme = self.theme;
        let typo = &self.typography;
        let density = self.density;
        let query = picker.query.read(cx).value().to_string();
        let visible = filter_rows(&picker.rows, &query);

        let heading = match picker.forge {
            ForgeKind::Github => "Add an issue or pull request",
            ForgeKind::Gitlab => "Add an issue or merge request",
        };

        // The one line under the search box that explains an empty list. Three
        // different empties (still loading, nothing open on the repo, nothing
        // matching the query) read identically without it.
        let status: Option<SharedString> = if picker.loading {
            Some("Loading…".into())
        } else if picker.rows.is_empty() {
            Some("Nothing open on this repository, or its CLI isn't signed in.".into())
        } else if visible.is_empty() {
            Some("No match.".into())
        } else {
            None
        };

        // `.id()` first: `overflow_y_scroll` lives on the stateful div.
        let mut list = div()
            .id("chat-forge-picker-list")
            .flex()
            .flex_col()
            .w_full()
            .max_h(px(LIST_MAX_H))
            .overflow_y_scroll();

        for i in visible {
            let row = &picker.rows[i];
            let number = format!("#{}", row.number);
            let title = elide(&row.title);
            let meta = if row.author.is_empty() {
                row.kind_label(picker.forge).to_string()
            } else {
                format!("{} · @{}", row.kind_label(picker.forge), row.author)
            };
            let picked = row.clone();
            list = list.child(
                div()
                    .id(("chat-forge-row", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .w_full()
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(density.r_xs))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_overlay))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.fg_muted)
                            .text_size(px(typo.t_body_sm))
                            .child(SharedString::from(number)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme.fg_base)
                            .text_size(px(typo.t_body_sm))
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.fg_muted)
                            .text_size(px(typo.t_label_xs))
                            .child(SharedString::from(meta)),
                    )
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.stage_forge_item(picked.clone(), window, cx)
                    })),
            );
        }

        let card = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w(px(CARD_W))
            .max_w(relative(0.9))
            .p(px(12.0))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border_input)
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
            .child(
                div()
                    .text_color(theme.fg_muted)
                    .text_size(px(typo.t_label_xs))
                    .child(SharedString::from(heading)),
            )
            .child(Input::new(&picker.query))
            .children(status.map(|text| {
                div()
                    .text_color(theme.fg_muted)
                    .text_size(px(typo.t_label_xs))
                    .px(px(10.0))
                    .py(px(6.0))
                    .child(text)
            }))
            .child(list);

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::Hsla { a: 0.55, ..theme.bg_panel })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, window, cx| this.close_forge_picker(window, cx)),
                )
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ForgeItem` has no `Default`, so build one field-by-field rather than
    /// adding a derive to a shared type for a test's convenience.
    fn forge_item(number: u64, title: &str) -> crate::shell::forge::ForgeItem {
        crate::shell::forge::ForgeItem {
            number,
            title: title.to_string(),
            state: "OPEN".into(),
            url: String::new(),
            labels: Vec::new(),
            assignees: Vec::new(),
            author: Default::default(),
            updated_at: String::new(),
        }
    }

    fn row(kind: ForgeRefKind, number: u64, title: &str) -> ForgeRow {
        ForgeRow { kind, number, title: title.to_string(), author: "nhtera".into() }
    }

    fn sample() -> Vec<ForgeRow> {
        vec![
            row(ForgeRefKind::Issue, 42, "Parser drops a token"),
            row(ForgeRefKind::Issue, 7, "Crash on open"),
            row(ForgeRefKind::Pull, 421, "Add the attach menu"),
        ]
    }

    #[test]
    fn an_empty_query_keeps_fetch_order() {
        assert_eq!(filter_rows(&sample(), ""), vec![0, 1, 2]);
        assert_eq!(filter_rows(&sample(), "   "), vec![0, 1, 2]);
    }

    #[test]
    fn a_title_query_matches_case_insensitively() {
        assert_eq!(filter_rows(&sample(), "PARSER"), vec![0]);
    }

    /// The number is what a user has in hand when they open this. A digit query
    /// must not be diluted by titles that happen to contain the same digits.
    #[test]
    fn a_number_query_matches_the_number() {
        assert_eq!(filter_rows(&sample(), "42"), vec![0, 2]);
        assert_eq!(filter_rows(&sample(), "#7"), vec![1]);
    }

    /// `#42` written out and `42` typed bare are the same ask.
    #[test]
    fn a_leading_hash_is_optional() {
        assert_eq!(filter_rows(&sample(), "#42"), filter_rows(&sample(), "42"));
    }

    #[test]
    fn number_matches_sort_ahead_of_title_matches() {
        let rows = vec![
            row(ForgeRefKind::Issue, 1, "fixes 42 things"),
            row(ForgeRefKind::Issue, 42, "unrelated"),
        ];
        assert_eq!(filter_rows(&rows, "42"), vec![1, 0]);
    }

    #[test]
    fn no_match_returns_empty() {
        assert!(filter_rows(&sample(), "zzzznope").is_empty());
    }

    #[test]
    fn listings_merge_with_their_kinds_preserved() {
        let item = |n: u64, t: &str| forge_item(n, t);
        let rows = rows_from_listings(vec![item(1, "i")], vec![item(2, "p")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, ForgeRefKind::Issue);
        assert_eq!(rows[1].kind, ForgeRefKind::Pull);
    }

    #[test]
    fn merged_listings_are_capped() {
        let item = |n: u64| forge_item(n, "t");
        let many: Vec<_> = (0..80).map(item).collect();
        assert_eq!(rows_from_listings(many, Vec::new()).len(), MAX_ROWS);
    }

    #[test]
    fn a_long_title_elides_without_splitting_a_char() {
        let title = "é".repeat(TITLE_MAX + 10);
        let out = elide(&title);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), TITLE_MAX);
    }

    #[test]
    fn a_short_title_is_left_alone() {
        assert_eq!(elide("short"), "short");
    }
}
