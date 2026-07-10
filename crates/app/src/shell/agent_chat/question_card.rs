//! The interactive AskUserQuestion card — a Claude-Desktop-style *pager*.
//!
//! Rendered inline in the transcript for a tool call awaiting answers. It is its
//! own gpui entity (not part of `AgentChatView`'s render) so its per-question
//! "Other" text inputs + the overall response box repaint on keystroke without
//! rebuilding the whole transcript — the same reason the composer is isolated.
//!
//! One question is shown at a time (`‹ i of N ›` nav appears only for >1
//! question). Options are native radio (single-select) / checkbox (multi). A
//! per-question "Other" field and one overall response box provide free text;
//! typing in either supersedes the option selections (the tool resolves `custom`
//! and the overall `response` ahead of the answer map). Submit is strictly gated:
//! every question must be answered, or the overall response filled. Skip (and
//! Esc) send a plain allow with no answers ("did not answer").

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_agents::thread::{QuestionAnswer, QuestionAnswers, QuestionRequest, ToolCall};
use oximux_settings::{Density, Theme, Typography};

use super::bubble;
use super::tool_card::pill_button;

/// True for the AskUserQuestion tool (its answered rows get the compact
/// [`render_settled`] summary instead of the generic tool card).
pub(super) fn is_question(tc: &ToolCall) -> bool {
    tc.name == "AskUserQuestion"
}

/// Wrap a text `InputState` in a bordered pill with explicit padding — the same
/// pattern the composer uses. A bare `Input(...).appearance(true)` does not
/// report a definite height to layout, which makes the card measure shorter than
/// it paints (its footer then falls outside the transcript's scrollable range);
/// the padded wrapper gives the field a measured height so the card's true
/// height is counted.
fn input_pill(
    state: &Entity<InputState>,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        // Explicit height so the field reports a definite size to layout (a bare
        // Input does not), keeping the card's measured height honest.
        .h(px(30.0))
        .rounded(px(density.r_xs))
        .border_1()
        .border_color(theme.border_input)
        .bg(theme.bg_base)
        .px(px(density.pad_row))
        .child(Input::new(state).appearance(false).text_size(px(typo.t_body_sm)))
}

/// One clickable option row: a radio (single-select) / checkbox (multi-select)
/// indicator, the option label, and its description muted on a second line. The
/// selected row is tinted; disabled (when the overall response supersedes the
/// options) it dims and stops responding.
#[allow(clippy::too_many_arguments)]
fn option_row(
    id: SharedString,
    label: String,
    desc: String,
    multi: bool,
    selected: bool,
    disabled: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let has_desc = !desc.is_empty() && desc != label;
    let mut indicator = div()
        .flex_none()
        .size(px(15.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(if multi { 4.0 } else { 8.0 }))
        .border_1()
        .border_color(if selected { theme.status_ok } else { theme.border_input });
    if selected {
        indicator = indicator.bg(theme.status_ok);
        if multi {
            indicator = indicator.child(
                div().text_size(px(10.0)).text_color(theme.bg_panel).child(SharedString::from("✓")),
            );
        }
    }
    let mut text_col = div().flex().flex_col().flex_1().min_w(px(0.0)).child(
        div()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_base)
            .child(SharedString::from(label)),
    );
    if has_desc {
        text_col = text_col.child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(desc)),
        );
    }
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_start()
        .gap(px(9.0))
        .w_full()
        .px(px(6.0))
        .py(px(5.0))
        .rounded(px(density.r_xs))
        .child(div().mt(px(1.0)).child(indicator))
        .child(text_col);
    if selected {
        row = row.bg(theme.bg_base);
    }
    if disabled {
        row = row.opacity(0.5);
    } else {
        row = row.cursor_pointer().hover(|s| s.bg(theme.bg_base)).on_click(on_click);
    }
    row.into_any_element()
}

/// The Submit action: a filled accent button when enabled, a muted outline when
/// the answers aren't complete yet.
fn submit_button(
    id: String,
    enabled: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut b = div()
        .id(SharedString::from(id))
        .px(px(14.0))
        .py(px(4.0))
        .rounded(px(density.r_chip))
        .text_size(px(typo.t_body_sm))
        .child(SharedString::from("Submit"));
    if enabled {
        b = b
            .bg(theme.status_ok)
            .text_color(theme.bg_panel)
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .on_click(on_click);
    } else {
        b = b.border_1().border_color(theme.border_inactive).text_color(theme.fg_subtle);
    }
    b.into_any_element()
}

/// The compact read-only summary shown once a question is answered/skipped — a
/// single line derived from the tool's result (the CLI echoes the chosen
/// answers), so the transcript keeps a trace without the interactive controls.
pub(super) fn render_settled(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let summary = tc.result.as_deref().map(normalize_result).unwrap_or_else(|| "Answered".into());
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .py(px(2.0))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
        .child(div().text_color(theme.status_ok).child(SharedString::from("✓")))
        .child(SharedString::from(bubble::elide(&summary, 140)))
        .into_any_element()
}

/// Turn the CLI's verbose tool-result into a short summary line.
fn normalize_result(result: &str) -> String {
    let r = result.trim();
    if r.contains("did not answer") {
        return "Skipped".to_string();
    }
    if let Some(rest) = r.strip_prefix("Your questions have been answered: ") {
        return rest
            .strip_suffix(" You can now continue with these answers in mind.")
            .unwrap_or(rest)
            .to_string();
    }
    r.to_string()
}

/// Emitted to the parent [`super::AgentChatView`] when the user resolves the card.
pub enum QuestionCardEvent {
    /// The user submitted their selections/answers.
    Submit { tool_id: String, answers: QuestionAnswers },
    /// The user skipped (plain allow, no answers).
    Skip { tool_id: String },
}

pub struct QuestionCard {
    /// The `ToolCall.id` this card answers (its join key in the transcript).
    tool_id: String,
    request: QuestionRequest,
    /// Active question index (the pager position).
    page: usize,
    /// Per-question selected option labels (parallel to `request.questions`).
    /// Free-text "Other"/overall answers are read live from the inputs.
    selected: Vec<Vec<String>>,
    /// One "Other" text field per question.
    others: Vec<Entity<InputState>>,
    /// One card-level overall response field.
    overall: Entity<InputState>,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<QuestionCardEvent> for QuestionCard {}

impl QuestionCard {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool_id: String,
        request: QuestionRequest,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let n = request.questions.len();
        let mut others = Vec::with_capacity(n);
        let mut subs = Vec::new();
        for i in 0..n {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Other…"));
            let sub = cx.subscribe(&input, move |this: &mut Self, _input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    // Typing an "Other" answer deselects that question's options
                    // (custom text supersedes the option map when resolved).
                    let has = !this.others[i].read(cx).value().trim().is_empty();
                    if has && let Some(sel) = this.selected.get_mut(i) {
                        sel.clear();
                    }
                }
                if matches!(ev, InputEvent::Change | InputEvent::Focus | InputEvent::Blur) {
                    cx.notify();
                }
            });
            others.push(input);
            subs.push(sub);
        }
        let overall =
            cx.new(|cx| InputState::new(window, cx).placeholder("Or reply in your own words…"));
        subs.push(cx.subscribe(&overall, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change | InputEvent::Focus | InputEvent::Blur) {
                cx.notify();
            }
        }));
        Self {
            tool_id,
            request,
            page: 0,
            selected: vec![Vec::new(); n],
            others,
            overall,
            theme,
            density,
            typography,
            focus_handle: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn overall_text(&self, cx: &Context<Self>) -> String {
        self.overall.read(cx).value().to_string()
    }

    fn question_answered(&self, i: usize, cx: &Context<Self>) -> bool {
        !self.selected[i].is_empty() || !self.others[i].read(cx).value().trim().is_empty()
    }

    /// Strict gating: every question resolved, OR the overall response is filled.
    fn submit_enabled(&self, cx: &Context<Self>) -> bool {
        if !self.overall_text(cx).trim().is_empty() {
            return true;
        }
        (0..self.request.questions.len()).all(|i| self.question_answered(i, cx))
    }

    /// Which action the footer's forward button offers on the current page:
    /// `true` = the (gated) Submit — shown on the final page, or as soon as the
    /// whole set is answerable from anywhere; `false` = a plain `Next →` that
    /// steps to the following question. Single-question cards are always final,
    /// so they only ever show Submit.
    fn forward_is_submit(&self, cx: &Context<Self>) -> bool {
        self.submit_enabled(cx) || self.page + 1 >= self.request.questions.len()
    }

    fn build_answers(&self, cx: &Context<Self>) -> QuestionAnswers {
        let mut by_question = std::collections::HashMap::new();
        for (i, q) in self.request.questions.iter().enumerate() {
            let raw = self.others[i].read(cx).value().to_string();
            let custom = (!raw.trim().is_empty()).then_some(raw);
            by_question
                .insert(q.id.clone(), QuestionAnswer { selected: self.selected[i].clone(), custom });
        }
        let overall = self.overall_text(cx);
        let response = (!overall.trim().is_empty()).then_some(overall);
        QuestionAnswers { by_question, response }
    }

    fn select_single(&mut self, i: usize, label: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected[i] = vec![label];
        // A picked option clears any "Other" text so the selection takes effect.
        self.others[i].update(cx, |s, cx| s.set_value("", window, cx));
        self.focus_self(window, cx);
        cx.notify();
    }

    fn toggle_multi(&mut self, i: usize, label: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pos) = self.selected[i].iter().position(|l| l == &label) {
            self.selected[i].remove(pos);
        } else {
            self.selected[i].push(label);
            self.others[i].update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.focus_self(window, cx);
        cx.notify();
    }

    fn goto(&mut self, page: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page.min(self.request.questions.len().saturating_sub(1));
        self.focus_self(window, cx);
        cx.notify();
    }

    /// Move focus onto the card container so ‹/› paging and Esc reach it after
    /// the user touches a non-text control (an option, a dot, an arrow). Text
    /// fields grab focus themselves when clicked, so this never fights the caret.
    /// Deferred: a synchronous focus inside a click handler is clobbered by the
    /// click's own post-dispatch focus pass, so the container would never end up
    /// focused — the deferred pass runs after that settles and sticks.
    fn focus_self(&self, window: &mut Window, cx: &mut App) {
        let handle = self.focus_handle.clone();
        window.defer(cx, move |window, cx| handle.focus(window, cx));
    }

    /// Whether a per-question "Other" or the overall response field holds focus —
    /// arrow paging stands down while typing so ‹/› move the caret, not the page.
    fn text_field_focused(&self, window: &Window, cx: &App) -> bool {
        self.others.iter().any(|s| s.read(cx).focus_handle(cx).is_focused(window))
            || self.overall.read(cx).focus_handle(cx).is_focused(window)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.submit_enabled(cx) {
            return;
        }
        let answers = self.build_answers(cx);
        cx.emit(QuestionCardEvent::Submit { tool_id: self.tool_id.clone(), answers });
    }

    fn skip(&mut self, cx: &mut Context<Self>) {
        cx.emit(QuestionCardEvent::Skip { tool_id: self.tool_id.clone() });
    }
}

impl Render for QuestionCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let n = self.request.questions.len();
        // Defensive: the state fold already drops empty question sets before a
        // card is created, but never index-panic the render thread on garbled
        // input — render nothing if there is somehow nothing to ask.
        if n == 0 {
            return div().into_any_element();
        }
        let page = self.page.min(n - 1);
        let q = &self.request.questions[page];

        let header = q.header.clone();
        let question = q.question.clone();
        let multi = q.multi_select();
        let is_secret = q.is_secret;
        let options: Vec<(usize, String, String)> = q
            .options
            .iter()
            .enumerate()
            .map(|(oi, o)| (oi, o.label.clone(), o.description.clone()))
            .collect();
        let selected = self.selected[page].clone();
        let overall_active = !self.overall_text(cx).trim().is_empty();
        let submit_ok = self.submit_enabled(cx);

        // Header: a short label chip, the full question, and (for multi-question
        // requests) the ‹ i of N › pager on the right.
        let mut title = div().flex().flex_col().gap(px(4.0)).flex_1().min_w(px(0.0));
        if !header.is_empty() {
            title = title.child(
                div().flex().flex_row().child(
                    div()
                        .flex_none()
                        .px(px(7.0))
                        .py(px(1.0))
                        .rounded(px(density.r_chip))
                        .bg(theme.bg_base)
                        .text_size(px(typo.t_label_xs))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(header)),
                ),
            );
        }
        title = title.child(
            div()
                .text_size(px(typo.t_body_md))
                .text_color(theme.fg_base)
                .child(SharedString::from(question)),
        );
        // Sensitive-answer hint (Codex `isSecret`): the answer still rides back to
        // the agent, so warn the user it's treated as a credential. Full input
        // masking + transcript no-persist is a follow-up.
        if is_secret {
            title = title.child(
                div()
                    .text_size(px(typo.t_label_xs))
                    .text_color(theme.status_warn)
                    .child(SharedString::from("🔒 Sensitive — your answer is sent to the agent")),
            );
        }
        if multi {
            title = title.child(
                div()
                    .text_size(px(typo.t_label_xs))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("Select all that apply")),
            );
        }
        let mut head = div()
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .gap(px(8.0))
            .w_full()
            .child(title);
        if n > 1 {
            head = head.child(self.pager(page, n, cx));
        }

        // Options as custom rows, then the per-question "Other" field (dimmed +
        // inert when the overall response supersedes the options).
        let mut opts = div().flex().flex_col().gap(px(3.0)).w_full();
        for (oi, label, desc) in options {
            let is_sel = selected.contains(&label);
            let id = SharedString::from(format!("q{page}-o{oi}"));
            let l = label.clone();
            let on_click = cx.listener(move |this, _e: &ClickEvent, w, cx| {
                if multi {
                    this.toggle_multi(page, l.clone(), w, cx);
                } else {
                    this.select_single(page, l.clone(), w, cx);
                }
            });
            opts = opts.child(option_row(
                id, label, desc, multi, is_sel, overall_active, theme, density, &typo, on_click,
            ));
        }
        opts = opts.child(
            div()
                .mt(px(3.0))
                .when(overall_active, |s| s.opacity(0.5))
                .child(input_pill(&self.others[page], theme, density, &typo)),
        );

        // The forward action. On any non-final page that isn't yet submittable, a
        // plain `Next →` steps to the following question (dots/arrows still allow
        // free jumping); on the final page — or once the whole set is answerable —
        // it becomes the gated `Submit`. Single-question cards are always "final",
        // so they only ever show Submit.
        let primary: AnyElement = if self.forward_is_submit(cx) {
            submit_button(
                format!("q-submit-{}", self.tool_id),
                submit_ok,
                theme,
                density,
                &typo,
                cx.listener(|this, _e: &ClickEvent, _w, cx| this.submit(cx)),
            )
        } else {
            let target = page + 1;
            pill_button(
                format!("q-next-{}", self.tool_id),
                "Next →",
                theme.fg_base,
                density,
                &typo,
                cx.listener(move |this, _e: &ClickEvent, window, cx| this.goto(target, window, cx)),
            )
        };

        // Footer: a divider, the overall free-text response (an alternative to
        // the options above), then Skip / the forward action.
        let footer = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .child(div().h(px(1.0)).w_full().bg(theme.border_inactive))
            .child(input_pill(&self.overall, theme, density, &typo))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(pill_button(
                        format!("q-skip-{}", self.tool_id),
                        "Skip",
                        theme.fg_muted,
                        density,
                        &typo,
                        cx.listener(|this, _e: &ClickEvent, _w, cx| this.skip(cx)),
                    ))
                    .child(primary),
            );

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if key == "escape" {
                    this.skip(cx);
                    cx.stop_propagation();
                    return;
                }
                // ‹/› page between questions — but only when the caret isn't in a
                // text field (there the arrows edit) and no modifier is held (so
                // ⌘←/⌥← stay as editor shortcuts).
                let m = &ev.keystroke.modifiers;
                if m.control || m.alt || m.platform || m.function || this.text_field_focused(window, cx)
                {
                    return;
                }
                let n = this.request.questions.len();
                match key {
                    "left" if this.page > 0 => {
                        this.goto(this.page - 1, window, cx);
                        cx.stop_propagation();
                    }
                    "right" if this.page + 1 < n => {
                        this.goto(this.page + 1, window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .w_full()
            .rounded(px(density.r_card))
            .border_1()
            .border_color(theme.border_input)
            .bg(theme.bg_panel_alt)
            .p(px(density.pad_panel))
            .child(head)
            .child(opts)
            .child(footer)
            .into_any_element()
    }
}

impl QuestionCard {
    /// The pager control (only rendered for multi-question cards): a ‹ stepper,
    /// one progress dot per question, then a › stepper. Each dot is filled once
    /// its question is answered, hollow while outstanding, and ringed at the
    /// current page — so at a glance the user sees how many remain and where they
    /// are. Dots and arrows are all clickable; the arrows dim + go inert at the
    /// range edge.
    fn pager(&self, page: usize, n: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typo = self.typography.clone();
        // Precompute answered-state so the dot builders don't re-borrow `self`
        // while `cx.listener` is live.
        let answered: Vec<bool> = (0..n).map(|i| self.question_answered(i, cx)).collect();

        let arrow = |glyph: &str, target: Option<usize>, cx: &mut Context<Self>| {
            let mut b = div()
                .id(SharedString::from(format!("q-nav-{glyph}-{page}")))
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(4.0))
                .text_size(px(typo.t_body_md))
                .text_color(if target.is_some() { theme.fg_muted } else { theme.fg_subtle })
                .child(SharedString::from(glyph.to_string()));
            if let Some(t) = target {
                b = b
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_base).text_color(theme.fg_base))
                    .on_click(cx.listener(move |this, _e, window, cx| this.goto(t, window, cx)));
            }
            b
        };

        let mut dots = div().flex().flex_row().items_center().gap(px(5.0)).flex_none();
        for (i, &filled) in answered.iter().enumerate() {
            let mut inner = div().size(px(7.0)).rounded_full();
            inner = if filled {
                inner.bg(theme.status_ok)
            } else {
                inner.border_1().border_color(theme.border_input)
            };
            // A fixed-size cell keeps every dot on the same baseline; only the
            // current one carries the accent ring.
            let mut cell = div()
                .id(SharedString::from(format!("q-dot-{i}")))
                .size(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .child(inner);
            if i == page {
                cell = cell.border_1().border_color(theme.border_active);
            } else {
                cell = cell.hover(|s| s.bg(theme.bg_base));
            }
            cell = cell.on_click(cx.listener(move |this, _e, window, cx| this.goto(i, window, cx)));
            dots = dots.child(cell);
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .flex_none()
            .child(arrow("‹", page.checked_sub(1), cx))
            .child(dots)
            .child(arrow("›", (page + 1 < n).then_some(page + 1), cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::{AskQuestion, QuestionKind, QuestionOption, QuestionRequest};

    fn q(id: &str) -> AskQuestion {
        AskQuestion {
            id: id.to_string(),
            header: "H".into(),
            question: format!("Question {id}?"),
            options: vec![
                QuestionOption { label: "A".into(), description: "opt a".into() },
                QuestionOption { label: "B".into(), description: "opt b".into() },
            ],
            kind: QuestionKind::SingleSelect,
            other_allowed: true,
            is_secret: false,
        }
    }

    fn card(n: usize, cx: &mut TestAppContext) -> gpui::WindowHandle<QuestionCard> {
        cx.update(gpui_component::init);
        let req = QuestionRequest {
            request_id: "req-1".into(),
            questions: (0..n).map(|i| q(&format!("q-{i}"))).collect(),
        };
        cx.add_window(|window, cx| {
            QuestionCard::new(
                "tool-1".into(),
                req,
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn pager_navigation_clamps_and_tracks_page(cx: &mut TestAppContext) {
        let w = card(3, cx);
        w.update(cx, |c, window, cx| {
            assert_eq!(c.page, 0, "starts on the first question");
            c.goto(1, window, cx);
            assert_eq!(c.page, 1);
            // Past the end clamps to the last question, never panics.
            c.goto(99, window, cx);
            assert_eq!(c.page, 2);
        })
        .unwrap();
    }

    #[gpui::test]
    fn forward_button_is_next_until_final_or_answerable(cx: &mut TestAppContext) {
        let w = card(3, cx);
        w.update(cx, |c, window, cx| {
            // Page 0 of 3, nothing answered → the forward action is Next, not Submit.
            assert!(!c.forward_is_submit(cx));
            // The final page always offers Submit (gated/disabled when unanswered).
            c.goto(2, window, cx);
            assert!(c.forward_is_submit(cx));
            // Answering every question makes Submit available from any page.
            c.goto(0, window, cx);
            c.select_single(0, "A".into(), window, cx);
            c.select_single(1, "A".into(), window, cx);
            c.select_single(2, "A".into(), window, cx);
            assert!(c.question_answered(0, cx));
            assert!(c.forward_is_submit(cx), "all answered → Submit even on page 0");
        })
        .unwrap();
    }

    #[gpui::test]
    fn single_question_card_only_ever_submits(cx: &mut TestAppContext) {
        let w = card(1, cx);
        w.update(cx, |c, _window, cx| {
            // n == 1 is always "final" → the forward action is Submit, never Next.
            assert!(c.forward_is_submit(cx));
        })
        .unwrap();
    }
}
