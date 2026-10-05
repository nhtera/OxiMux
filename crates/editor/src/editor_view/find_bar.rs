//! The find widget every editor tab shares, laid out the way a code
//! editor's find usually is: a compact box floating over the top-right of
//! the text, with a chevron that
//! opens a replace row, the query field with a match-case toggle inside it,
//! `n of total` (or "No results"), previous / next and close.
//!
//! One widget, two engines, picked per Cmd+F by [`FindTarget`]:
//!
//! - **Source** — the code editor's text (Source mode, every non-markdown
//!   file, the source half of Split while it has the caret). Driven through
//!   gpui-kit's custom-search API on `EditorState` (`set_search_query`,
//!   `next_search_match`, `replace_*`), which highlights matches in the editor
//!   without its built-in panel; that panel is switched off
//!   (`searchable(false)`), so the editor shows one find UI, not two.
//! - **Preview** — the rendered markdown ([`super::preview_find`]).
//!
//! Cmd+F reaches a view as the app-wide `Search` action through
//! [`EditorView::open_find`]: gpui ranks a binding with *no* key context as
//! the deepest match there is, so the app's context-less `Search` binding
//! always outranks the editor's own `cmd-f` (context `Input`), and this is the
//! only route in. Whole-word, regex and find-in-selection, common in find
//! widgets, are left out: gpui-kit's matcher has none of them, and
//! a toggle that does nothing would be worse than no toggle.

use std::ops::Range;

use gpui::{
    AnyElement, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, Styled as _, Subscription,
    Window, div, prelude::FluentBuilder as _, px, transparent_black,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{self, EditorState, Input, InputEvent, InputState},
    text::RenderedText,
};

use super::{EditorContent, EditorView, MarkdownViewMode, preview_find::MatchTones};

/// Width of the widget.
const BAR_W: f32 = 440.0;
/// Height of the query and replace fields.
const FIELD_H: f32 = 26.0;
/// Room for the counter, so "No results" and `12 of 345` do not shift the
/// buttons beside it as the count changes.
const COUNT_W: f32 = 76.0;
/// Width of the controls right of each field (the counter and three buttons
/// on the find row, two buttons on the replace row). Shared by both rows so
/// the two fields end at the same edge and the replace buttons sit under the
/// counter.
const TRAILING_W: f32 = 150.0;
/// Offset of the widget from the view's top edge: clear of the 28px
/// breadcrumb row, with the same 8px inset it keeps from the right edge.
const BAR_TOP: f32 = 28.0 + 8.0;

/// Which text the open find searches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FindTarget {
    /// The code editor's buffer.
    Source,
    /// The rendered markdown preview.
    Preview,
}

/// The open find widget.
pub(super) struct FindBar {
    pub(super) target: FindTarget,
    pub(super) query: Entity<InputState>,
    replace: Entity<InputState>,
    /// The replace row is showing (Source only — the preview is read-only).
    replace_open: bool,
    pub(super) match_case: bool,
    // Preview engine state; unused for `Source`, whose engine keeps its own.
    /// Byte ranges of the matches in `searched`, in document order.
    pub(super) matches: Vec<Range<usize>>,
    /// Index into `matches` of the "you are here" match.
    pub(super) current: usize,
    /// The rendered text `matches` index. A re-parse produces a different
    /// snapshot, which is the cue to search again; the notify that setting
    /// highlights sends leaves it equal, so highlighting never loops back
    /// into another search.
    pub(super) searched: Option<RenderedText>,
    /// The tones the preview highlights were last painted in, to repaint
    /// them when the theme changes.
    pub(super) tones: Option<MatchTones>,
    _subs: [Subscription; 2],
}

/// The counter beside the query: `n of total`, otherwise "No results" —
/// including before anything is typed, so the counter's slot is never an
/// empty gap between the field and the buttons.
pub(super) fn count_label(current: Option<usize>, total: usize) -> SharedString {
    match current {
        Some(ix) if total > 0 => format!("{} of {}", ix + 1, total).into(),
        _ => "No results".into(),
    }
}

impl EditorView {
    /// The code editor's state, for text content.
    fn source_state(&self) -> Option<Entity<EditorState>> {
        match &self.content {
            EditorContent::Text(t) => Some(t.state.clone()),
            _ => None,
        }
    }

    /// Whether `target`'s text is on screen.
    fn find_target_visible(&self, target: FindTarget) -> bool {
        match target {
            FindTarget::Preview => self.preview_visible(),
            FindTarget::Source => {
                self.source_state().is_some()
                    && (!self.is_markdown || self.md_mode != MarkdownViewMode::Preview)
            }
        }
    }

    /// Cmd+F on this view: open find over the text being read — the preview
    /// while it shows and the source does not have the caret, otherwise the
    /// source. A second Cmd+F on an open widget refocuses its query and
    /// selects it, so typing replaces it; a Cmd+F that lands on the other
    /// text moves the widget there, keeping the query.
    ///
    /// Returns `false`, doing nothing, for content with nothing to search
    /// (images, PDFs, binaries, a load in progress).
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(source) = self.source_state() else {
            return false;
        };
        let source_focused = source.read(cx).focus_handle(cx).is_focused(window);
        let target = if self.preview_visible() && !source_focused {
            FindTarget::Preview
        } else {
            FindTarget::Source
        };
        if self.find.as_ref().is_some_and(|find| find.target != target) {
            self.retarget_find(target, cx);
        }
        if self.find.is_none() {
            self.create_find(target, window, cx);
        }
        if let Some(find) = &self.find {
            let query = find.query.clone();
            query.update(cx, |input, cx| input.select_all(window, cx));
            query.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
        true
    }

    /// Build the widget for `target`, seeded with the target's single-line
    /// selection the way an editor's find is.
    fn create_find(&mut self, target: FindTarget, window: &mut Window, cx: &mut Context<Self>) {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let replace = cx.new(|cx| InputState::new(window, cx).placeholder("Replace"));
        let selection = match target {
            FindTarget::Preview => self
                .md_preview
                .as_ref()
                .map(|preview| preview.state.read(cx).selected_text()),
            FindTarget::Source => self.source_state().map(|state| state.read(cx).selected_text().to_string()),
        };
        let seed = selection
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty() && !text.contains('\n'));
        if let Some(seed) = &seed {
            query.update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
        }
        let query_sub = cx.subscribe_in(&query, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::Change => this.run_find_query(cx),
                InputEvent::PressEnter { shift, .. } => {
                    this.step_find(if *shift { -1 } else { 1 }, window, cx)
                }
                _ => {}
            }
        });
        let replace_sub =
            cx.subscribe_in(&replace, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary, .. } = event {
                    if *secondary {
                        this.replace_all(window, cx);
                    } else {
                        this.replace_one(window, cx);
                    }
                }
            });
        self.find = Some(FindBar {
            target,
            query,
            replace,
            replace_open: false,
            match_case: false,
            matches: Vec::new(),
            current: 0,
            searched: None,
            tones: None,
            _subs: [query_sub, replace_sub],
        });
        if seed.is_some() {
            self.run_find_query(cx);
        }
    }

    /// Point the open widget at the other text: clear what the old engine
    /// marked, then run the same query on the new one.
    fn retarget_find(&mut self, target: FindTarget, cx: &mut Context<Self>) {
        self.end_find_highlights(cx);
        if let Some(find) = &mut self.find {
            find.target = target;
            find.matches.clear();
            find.searched = None;
            find.tones = None;
            if target == FindTarget::Preview {
                find.replace_open = false;
            }
        }
        self.run_find_query(cx);
    }

    /// Clear the open target's match highlights.
    fn end_find_highlights(&mut self, cx: &mut Context<Self>) {
        match self.find.as_ref().map(|find| find.target) {
            Some(FindTarget::Preview) => self.clear_preview_highlights(cx),
            Some(FindTarget::Source) => {
                if let Some(state) = self.source_state() {
                    state.update(cx, |state, cx| state.close_search(cx));
                }
            }
            None => {}
        }
    }

    /// Re-focus the find query if the widget is open. For a caller that opened
    /// find from a context whose own focus change may land afterwards (the
    /// command palette refocuses the window root as it closes); it never opens
    /// or retargets anything.
    pub fn refocus_find(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = self.find.as_ref().filter(|f| self.find_target_visible(f.target)) {
            find.query.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// Close the widget, clear its highlights and give the caret back to the
    /// text it searched. Returns whether it was open.
    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.find.as_ref().map(|find| find.target) else {
            return false;
        };
        self.end_find_highlights(cx);
        self.find = None;
        match (target, self.source_state()) {
            (FindTarget::Source, Some(state)) => state.read(cx).focus_handle(cx).focus(window, cx),
            _ => self.focus_handle.focus(window, cx),
        }
        cx.notify();
        true
    }

    /// After the markdown view mode changed: close a widget whose text is no
    /// longer on screen. Landing in Source from the preview's widget puts the
    /// caret in the source, or typing would go nowhere until a click.
    pub(super) fn find_after_mode_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hidden = self.find.as_ref().is_some_and(|f| !self.find_target_visible(f.target));
        if hidden
            && self.close_find(window, cx)
            && self.md_mode == MarkdownViewMode::Source
            && let Some(state) = self.source_state()
        {
            state.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// Search again for the current query and match-case setting, starting
    /// over at the first match and revealing it.
    pub(super) fn run_find_query(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &self.find else {
            return;
        };
        match find.target {
            FindTarget::Preview => self.refresh_preview_find(true, cx),
            FindTarget::Source => {
                let query = find.query.read(cx).value().to_string();
                let case_insensitive = !find.match_case;
                if let Some(state) = self.source_state() {
                    state.update(cx, |state, cx| {
                        state.set_search_query(query, case_insensitive, cx);
                        // A new query leaves the engine on its first match
                        // without scrolling to it. Stepping back then forward
                        // lands on that same match and reveals it (with one
                        // match both steps wrap onto it).
                        if !state.search_session().matcher.is_empty() {
                            state.previous_search_match(cx);
                            state.next_search_match(cx);
                        }
                    });
                }
                cx.notify();
            }
        }
    }

    /// Move to the next (`dir = 1`) or previous (`dir = -1`) match, wrapping.
    pub(super) fn step_find(&mut self, dir: isize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.find.as_ref().map(|find| find.target) else {
            return;
        };
        match target {
            FindTarget::Preview => self.step_preview_find(dir, cx),
            FindTarget::Source => {
                if let Some(state) = self.source_state() {
                    state.update(cx, |state, cx| {
                        if dir < 0 {
                            state.previous_search_match(cx);
                        } else {
                            state.next_search_match(cx);
                        }
                    });
                }
                cx.notify();
            }
        }
    }

    /// The current match and the total, for the counter.
    fn find_position(&self, cx: &gpui::App) -> (Option<usize>, usize) {
        let Some(find) = &self.find else {
            return (None, 0);
        };
        match find.target {
            FindTarget::Preview => {
                let total = find.matches.len();
                ((total > 0).then_some(find.current), total)
            }
            FindTarget::Source => self.source_state().map_or((None, 0), |state| {
                let matcher = &state.read(cx).search_session().matcher;
                (matcher.current(), matcher.len())
            }),
        }
    }

    /// Whether the open widget can replace: a Source target the editor lets
    /// edit.
    fn find_can_replace(&self, cx: &gpui::App) -> bool {
        self.find.as_ref().is_some_and(|find| find.target == FindTarget::Source)
            && self.source_state().is_some_and(|state| state.read(cx).is_replaceable())
    }

    fn replacement(&self, cx: &gpui::App) -> Option<String> {
        self.find.as_ref().map(|find| find.replace.read(cx).value().to_string())
    }

    /// Replace the current match and move to the next one.
    fn replace_one(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_can_replace(cx) {
            return;
        }
        let (Some(text), Some(state)) = (self.replacement(cx), self.source_state()) else {
            return;
        };
        state.update(cx, |state, cx| {
            state.replace_current_search_match(&text, window, cx);
        });
        cx.notify();
    }

    /// Replace every match.
    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_can_replace(cx) {
            return;
        }
        let (Some(text), Some(state)) = (self.replacement(cx), self.source_state()) else {
            return;
        };
        state.update(cx, |state, cx| {
            state.replace_all_search_matches(&text, window, cx);
        });
        cx.notify();
    }

    fn toggle_match_case(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = &mut self.find {
            find.match_case = !find.match_case;
        }
        self.run_find_query(cx);
    }

    /// Show or hide the replace row, moving the caret into whichever field
    /// is the one being typed into.
    fn toggle_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_can_replace(cx) {
            return;
        }
        let Some(find) = &mut self.find else {
            return;
        };
        find.replace_open = !find.replace_open;
        let field = if find.replace_open { &find.replace } else { &find.query };
        field.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// The widget, when open and its text is on screen.
    pub(super) fn render_find(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.repaint_stale_preview_tones(cx);
        let find = self.find.as_ref().filter(|f| self.find_target_visible(f.target))?;
        let palette = oximux_settings::appearance::theme(cx);
        let typo = oximux_settings::appearance::typography(cx);
        let density = oximux_settings::appearance::density(cx);
        let (current, total) = self.find_position(cx);
        let count = count_label(current, total);
        let can_replace = self.find_can_replace(cx);
        let replace_open = can_replace && find.replace_open;
        let no_match = total == 0;
        let query_focused = find.query.read(cx).focus_handle(cx).is_focused(window);
        let replace_focused = find.replace.read(cx).focus_handle(cx).is_focused(window);

        // A flat, recessed field: darker than the widget, no outline until it
        // has the caret, then a thin focus ring — so the two fields read as
        // slots in one control rather than two separate text boxes.
        let field = |input: &Entity<InputState>, focused: bool| {
            div()
                .flex_1()
                .min_w_0()
                .h(px(FIELD_H))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .pl(px(6.0))
                .pr(px(2.0))
                .bg(palette.bg_base)
                .rounded(px(density.r_xs))
                .border_1()
                .border_color(if focused { palette.focus_ring } else { transparent_black() })
                .text_size(px(typo.t_body_sm))
                .child(div().flex_1().min_w_0().child(Input::new(input).appearance(false).xsmall()))
        };
        let icon_button = |id: &'static str, icon: Icon, tooltip: &'static str| {
            Button::new(id).ghost().xsmall().icon(icon).tooltip(tooltip)
        };

        let trailing = || div().flex_none().w(px(TRAILING_W)).flex().flex_row().items_center().gap(px(2.0));
        let find_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            // Match Case sits inside the query field, beside the text it
            // qualifies.
            .child(
                field(&find.query, query_focused).child(
                    icon_button("find-match-case", IconName::CaseSensitive.into(), "Match Case")
                        .selected(find.match_case)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_match_case(cx))),
                ),
            )
            .child(
                trailing()
                    .child(
                        div()
                            .flex_none()
                            .w(px(COUNT_W))
                            .px(px(6.0))
                            .text_size(px(typo.t_body_sm))
                            .text_color(palette.fg_muted)
                            .child(count),
                    )
                    .child(
                        icon_button("find-prev", IconName::ArrowUp.into(), "Previous Match (Shift+Enter)")
                            .disabled(no_match)
                            .on_click(cx.listener(|this, _, window, cx| this.step_find(-1, window, cx))),
                    )
                    .child(
                        icon_button("find-next", IconName::ArrowDown.into(), "Next Match (Enter)")
                            .disabled(no_match)
                            .on_click(cx.listener(|this, _, window, cx| this.step_find(1, window, cx))),
                    )
                    .child(
                        icon_button("find-close", IconName::Close.into(), "Close (Escape)").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.close_find(window, cx);
                            }),
                        ),
                    ),
            );
        let replace_row = replace_open.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .child(field(&find.replace, replace_focused))
                .child(
                    trailing()
                        .pl(px(4.0))
                        .child(
                            icon_button("find-replace-one", IconName::Replace.into(), "Replace (Enter)")
                                .disabled(no_match)
                                .on_click(cx.listener(|this, _, window, cx| this.replace_one(window, cx))),
                        )
                        .child(
                            icon_button(
                                "find-replace-all",
                                Icon::empty().path("icons/replace-all.svg"),
                                "Replace All (Cmd+Enter)",
                            )
                            .disabled(no_match)
                            .on_click(cx.listener(|this, _, window, cx| this.replace_all(window, cx))),
                        ),
                )
        });

        let widget = div()
            .id("editor-find")
            .absolute()
            .top(px(BAR_TOP))
            .right(px(16.0))
            .w(px(BAR_W))
            .flex()
            .flex_row()
            .bg(palette.bg_overlay)
            .border_1()
            .border_color(palette.border_inactive)
            .rounded(px(density.r_xs))
            .overflow_hidden()
            .shadow_lg()
            // A click anywhere on the widget — its padding, the counter — lands
            // here rather than on the text underneath, where it would start a
            // selection and take focus from the query mid-search.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(find) = &this.find {
                        find.query.read(cx).focus_handle(cx).focus(window, cx);
                    }
                }),
            )
            // With a field focused, Escape reaches the widget only as the
            // input's own `Escape` action — an ancestor never sees the raw
            // keystroke — so it has to be caught on the way down.
            .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                cx.stop_propagation();
                this.close_find(window, cx);
            }))
            // A thin accent along the leading edge marks the widget as one
            // floating control over the text.
            .child(div().flex_none().w(px(2.0)).bg(palette.fg_subtle.opacity(0.5)))
            // The replace chevron, centred across both rows; a blank column of
            // the same width when the text cannot be replaced, so the find row
            // never shifts.
            .child(
                div()
                    .flex_none()
                    .w(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(can_replace, |col| {
                        col.child(
                            icon_button(
                                "find-toggle-replace",
                                if replace_open { IconName::ChevronDown } else { IconName::ChevronRight }
                                    .into(),
                                "Toggle Replace",
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_replace(window, cx))),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .py(px(4.0))
                    .pr(px(4.0))
                    .child(find_row)
                    .children(replace_row),
            );
        Some(widget.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Put the caret in the source editor (needs an active window, or gpui
    /// never reports the handle as focused).
    fn focus_source(view: &EditorView, window: &mut Window, cx: &mut gpui::App) {
        window.activate_window();
        view.source_state().expect("text").read(cx).focus_handle(cx).focus(window, cx);
    }

    fn type_query(view: &mut EditorView, query: &str, window: &mut Window, cx: &mut Context<EditorView>) {
        let input = view.find.as_ref().expect("find open").query.clone();
        input.update(cx, |input, cx| input.set_value(query.to_string(), window, cx));
        view.run_find_query(cx);
    }

    /// Cmd+F on a code file opens the widget over the source and drives the
    /// editor's engine; the editor's own panel stays shut, so there is one UI.
    #[gpui::test]
    async fn find_on_a_code_file_searches_and_replaces_the_source(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "let crate_a = 1;\nlet crate_b = 2;\nlet Crate = 3;\n").expect("write");
        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(view.open_find(window, cx));
                assert_eq!(view.find.as_ref().unwrap().target, FindTarget::Source);

                type_query(view, "crate", window, cx);
                assert_eq!(view.find_position(cx), (Some(0), 3), "case-insensitive by default");
                let state = view.source_state().unwrap();
                assert!(!state.read(cx).search_session().open, "the built-in panel stays shut");
                assert!(state.read(cx).search_session().is_active(), "matches are highlighted");

                view.step_find(1, window, cx);
                assert_eq!(view.find_position(cx), (Some(1), 3));
                view.step_find(-1, window, cx);
                view.step_find(-1, window, cx);
                assert_eq!(view.find_position(cx), (Some(2), 3), "wraps backwards");

                view.toggle_match_case(cx);
                assert_eq!(view.find_position(cx).1, 2, "Match Case drops `Crate`");

                let replace = view.find.as_ref().unwrap().replace.clone();
                replace.update(cx, |input, cx| input.set_value("krate", window, cx));
                view.replace_all(window, cx);
                assert_eq!(
                    state.read(cx).value().to_string(),
                    "let krate_a = 1;\nlet krate_b = 2;\nlet Crate = 3;\n"
                );

                assert!(view.close_find(window, cx));
                assert!(!state.read(cx).search_session().is_active(), "closing clears the marks");
            })
            .expect("window alive");
    }

    /// In Split, Cmd+F follows the caret: with it in the source, the source is
    /// searched rather than the preview beside it — and the widget moves
    /// there, query and all, if it was open over the preview.
    #[gpui::test]
    async fn find_in_split_follows_the_caret_into_the_source(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.md");
        std::fs::write(&path, "# Plan\n\nSome **plan** text.\n").expect("write");
        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.md_mode = MarkdownViewMode::Split;
                assert!(view.open_find(window, cx));
                assert_eq!(view.find.as_ref().unwrap().target, FindTarget::Preview);
                type_query(view, "plan", window, cx);
                focus_source(view, window, cx);
            })
            .expect("window alive");
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(view.open_find(window, cx));
                assert_eq!(view.find.as_ref().unwrap().target, FindTarget::Source);
                // `**plan**` counts in the source (markers and all), not just
                // the rendered word.
                assert_eq!(view.find_position(cx), (Some(0), 2), "kept the query");
            })
            .expect("window alive");
    }

    #[test]
    fn count_label_reads_position_or_no_results() {
        assert_eq!(count_label(Some(0), 3).as_ref(), "1 of 3");
        assert_eq!(count_label(Some(2), 3).as_ref(), "3 of 3");
        assert_eq!(count_label(None, 0).as_ref(), "No results");
    }
}
