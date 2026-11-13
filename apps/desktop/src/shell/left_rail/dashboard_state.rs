//! The rail's own state for the Agents page: its two filters, and the one
//! inline Reply box the rows share.
//!
//! It lives on `LeftRail` because the page is rendered inside the rail's
//! element tree and has no entity of its own — but it is the *page's* state,
//! not the rail's, so it is kept apart from the workspace list in the rail's
//! own file.
//!
//! **One box, not one per row.** A reply is typed into a single `InputState`
//! that changes owner, so opening another row's box cannot leave a draft
//! behind in the first. Everything that decides *where* a reply goes is in
//! `oximux_agents::row_actions` and the host; what is here is the widget's
//! lifecycle — build it lazily (it needs a window), focus it when the person
//! asked for it, and empty it on every change of owner.

use gpui::{AppContext, Context, Entity, Focusable, Window};
use gpui_component::input::{InputEvent, InputState};

use crate::shell::agents_dashboard::filter::StatusFilter;
use crate::shell::left_rail::nav_section::NavItem;
use crate::shell::left_rail::LeftRail;

impl LeftRail {
    /// Set the Agents-page status filter to `choice` (picked from the header
    /// dropdown). Repaints only when it actually changes.
    pub(crate) fn set_dashboard_status_filter(
        &mut self,
        choice: StatusFilter,
        cx: &mut Context<Self>,
    ) {
        if self.dashboard_status_filter != choice {
            self.dashboard_status_filter = choice;
            cx.notify();
        }
    }

    /// Clear the Agents-page text filter (the Escape affordance on its input).
    pub(crate) fn clear_dashboard_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.dashboard_filter_input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        if !self.dashboard_filter.is_empty() {
            self.dashboard_filter.clear();
            cx.notify();
        }
    }

    /// Lazily build the Agents-page filter input (it needs a window) and wire
    /// its `Change → mirror value + repaint` subscription. Returns a clone for
    /// the header render.
    pub(super) fn ensure_dashboard_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.dashboard_filter_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter…"));
        let sub = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dashboard_filter = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        self.dashboard_filter_input = Some(input.clone());
        self._dashboard_filter_sub = Some(sub);
        input
    }

    /// Open an Agents-page row's inline Reply box, or close it if it is
    /// already the open one.
    ///
    /// Focus moves into the box here and nowhere else: the person clicked
    /// Reply, so taking focus is what they asked for — while a row that
    /// merely painted itself must never pull focus out of a composer someone
    /// is typing in (F9).
    pub(crate) fn toggle_row_reply(
        &mut self,
        target: &str,
        answer: oximux_agents::row_actions::InlineAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.row_reply.is_open_for(target) {
            self.close_row_reply(window, cx);
            return;
        }
        // Lazily built (it needs a window) and reused, so opening a second
        // row's box is a change of owner rather than a new widget.
        if self.row_reply.input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Reply…"));
            let sub = cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if !matches!(event, InputEvent::PressEnter { .. }) {
                        return;
                    }
                    // The target is read here, not captured: the box outlives
                    // any one row, so a stale capture would send this reply to
                    // whichever row opened it first.
                    let Some(target) = this.row_reply.open_for.clone() else { return };
                    let Some(answer) = this.row_reply.answer.clone() else { return };
                    let text = input.read(cx).value().to_string();
                    // The root reports back rather than closing the box: this
                    // callback is already inside `LeftRail`'s own update, and
                    // a close from over there would re-enter this entity and
                    // panic — after the answer had reached the agent.
                    let sent = this
                        .weak_root
                        .update(cx, |root, cx| root.send_row_reply(&target, &answer, &text, cx))
                        .unwrap_or(false);
                    if sent {
                        this.close_row_reply(window, cx);
                    }
                },
            );
            self.row_reply.input = Some(input);
            self._row_reply_sub = Some(sub);
        }
        if let Some(input) = self.row_reply.input.clone() {
            // Empty for the new owner: carrying the last row's draft into
            // this one is how a reply reaches the wrong agent.
            input.update(cx, |state, cx| state.set_value("", window, cx));
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.row_reply.open_for = Some(target.to_string());
        // What this box's text will answer, fixed when it opened — so the
        // send can refuse if that is no longer what the session is asking.
        self.row_reply.answer = Some(answer);
        cx.notify();
    }

    /// Close the inline Reply box, clearing its text.
    ///
    /// The Escape affordance and what a sent reply does. Focus is not pushed
    /// anywhere: the box simply stops being rendered, and gpui hands focus
    /// back to the window.
    pub(crate) fn close_row_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.row_reply.open_for.is_none() {
            return;
        }
        self.row_reply.open_for = None;
        self.row_reply.answer = None;
        if let Some(input) = self.row_reply.input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Show the Agents page on the sessions that need someone.
    ///
    /// Not `select_nav`, which *toggles*: this is a "show" and must not close
    /// the page when it happens to be open already. It also clears the text
    /// filter and narrows the status filter to NEEDS YOU, because a stale
    /// filter — a text query from an hour ago, or `Working` — would leave the
    /// page open on none of the rows the person came to see.
    pub(crate) fn show_needs_you(&mut self, cx: &mut Context<Self>) {
        self.active_nav = Some(NavItem::Agents);
        // The text filter's own state is cleared too, not just the string the
        // list reads: leaving the box showing a stale query over an unfiltered
        // list is worse than either.
        self.dashboard_filter.clear();
        self.dashboard_filter_input = None;
        self._dashboard_filter_sub = None;
        self.dashboard_status_filter = StatusFilter::Attention;
        cx.notify();
    }
}
