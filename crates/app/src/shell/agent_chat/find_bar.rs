//! In-transcript find: a small bar over the top of the chat scroll that
//! searches message + tool text and steps between matching entries.
//!
//! Distinct from the terminal scrollback search (`terminal_search_state`), which
//! searches the alacritty grid — the chat transcript is a `Vec<ThreadEntry>`, so
//! matches are ENTRY indices and jumping reuses the transcript's own
//! scroll-to-entry primitive (`scroll_to_entry`), the same machinery the jump
//! list / tick-rail use. v1 scrolls the matched *message* into view with the
//! shared row flash; intra-text highlight (which `TextView::markdown` doesn't
//! expose ranges for) is a later nicety.

use gpui::{
    div, px, AnyElement, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Subscription,
};
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_agents::thread::ThreadEntry;

use super::{bubble, AgentChatView};

/// Fixed width of the find bar.
const BAR_W: f32 = 300.0;

/// Open find bar state: the query input, the matching entry indices, and a
/// cursor into them.
pub(super) struct FindBar {
    pub input: Entity<InputState>,
    pub matches: Vec<usize>,
    pub cursor: usize,
    /// Held so the query-change subscription lives as long as the bar.
    _sub: Subscription,
}

/// Entry indices whose searchable text contains `query` (case-insensitive
/// substring). Empty query → no matches. Pure so the walk is unit-testable.
pub(super) fn recompute_matches(entries: &[ThreadEntry], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| entry_haystack(e).to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// The searchable text of one entry: user prompt, assistant reply + thinking,
/// tool name + target + result, or a compaction summary.
fn entry_haystack(entry: &ThreadEntry) -> String {
    match entry {
        ThreadEntry::User { text, .. } => text.clone(),
        ThreadEntry::Assistant(msg) => format!("{}\n{}", msg.text, msg.thinking),
        ThreadEntry::ToolCall(tc) => {
            let mut s = tc.name.clone();
            if let Some(t) = bubble::tool_target(tc) {
                s.push('\n');
                s.push_str(&t);
            }
            if let Some(r) = &tc.result {
                s.push('\n');
                s.push_str(r);
            }
            s
        }
        ThreadEntry::ContextCompaction { summary } => summary.clone(),
    }
}

impl AgentChatView {
    /// Open (or refocus) the find bar and focus its input. Reuses the same bar
    /// on a second Cmd+F rather than recreating it, so an in-progress query is
    /// kept.
    pub(super) fn open_find(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.find_bar.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in chat…"));
            let sub = cx.subscribe(&input, |this, _input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.recompute_find(cx);
                }
            });
            self.find_bar = Some(FindBar { input, matches: Vec::new(), cursor: 0, _sub: sub });
        }
        let handle = self.find_bar.as_ref().map(|f| f.input.read(cx).focus_handle(cx));
        if let Some(handle) = handle {
            handle.focus(window, cx);
        }
        cx.notify();
    }

    /// Close the find bar and return focus to the composer.
    pub(super) fn close_find(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if self.find_bar.take().is_some() {
            let handle = self.composer.read(cx).focus_handle(cx);
            handle.focus(window, cx);
            cx.notify();
        }
    }

    /// True while the find bar is open AND its input holds focus — so the root
    /// key handlers can route ↵/⇧↵/Esc to find rather than the composer.
    pub(super) fn find_bar_focused(&self, window: &gpui::Window, cx: &gpui::App) -> bool {
        self.find_bar
            .as_ref()
            .is_some_and(|f| f.input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Recompute matches for the current query, reset the cursor, and scroll the
    /// first match into view (find-as-you-type). Called from the input's change
    /// subscription.
    pub(super) fn recompute_find(&mut self, cx: &mut Context<Self>) {
        let query = match &self.find_bar {
            Some(f) => f.input.read(cx).value().to_string(),
            None => return,
        };
        // Keep only matches we can actually jump to. An entry hidden inside a
        // collapsed >8-tool-call run has no scroll child (absent from
        // `entry_child_ix`), so counting it would make `n/total` promise a jump
        // that silently no-ops. v1 scope: find within the visible transcript.
        let matches: Vec<usize> = {
            let child_ix = self.entry_child_ix.borrow();
            recompute_matches(&self.thread.entries, &query)
                .into_iter()
                .filter(|i| child_ix.contains_key(i))
                .collect()
        };
        let first = matches.first().copied();
        if let Some(f) = &mut self.find_bar {
            f.matches = matches;
            f.cursor = 0;
        }
        if let Some(first) = first {
            self.scroll_to_entry(first, cx);
        }
        cx.notify();
    }

    /// Step to the next match (`dir = 1`) or previous (`dir = -1`), wrapping, and
    /// scroll it into view.
    pub(super) fn step_find(&mut self, dir: isize, cx: &mut Context<Self>) {
        let target = match &mut self.find_bar {
            Some(f) if !f.matches.is_empty() => {
                let n = f.matches.len() as isize;
                f.cursor = (f.cursor as isize + dir).rem_euclid(n) as usize;
                f.matches[f.cursor]
            }
            _ => return,
        };
        self.scroll_to_entry(target, cx);
    }

    pub(super) fn find_next(&mut self, cx: &mut Context<Self>) {
        self.step_find(1, cx);
    }

    pub(super) fn find_prev(&mut self, cx: &mut Context<Self>) {
        self.step_find(-1, cx);
    }

    /// The find bar overlay: query field, `n/total` counter, prev/next/close —
    /// an absolute overlay at the top-right of the transcript (like the jump
    /// list), so it floats over messages without resizing the composer.
    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.find_bar.as_ref()?;
        let theme = self.theme;
        let typo = &self.typography;
        let empty_query = f.input.read(cx).value().trim().is_empty();
        let count: SharedString = if !f.matches.is_empty() {
            format!("{}/{}", f.cursor + 1, f.matches.len()).into()
        } else if empty_query {
            SharedString::default()
        } else {
            "No matches".into()
        };
        let bar = div()
            .id("chat-find-bar")
            .absolute()
            .top(px(8.0))
            .right(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .w(px(BAR_W))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .shadow_lg()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(typo.t_body_sm))
                    .child(Input::new(&f.input).appearance(false)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(typo.t_label_xs))
                    .text_color(theme.fg_subtle)
                    .child(count),
            )
            .child(self.find_glyph_button("chat-find-prev", "↑", "Previous match", cx.listener(|this, _e, _w, cx| this.find_prev(cx))))
            .child(self.find_glyph_button("chat-find-next", "↓", "Next match", cx.listener(|this, _e, _w, cx| this.find_next(cx))))
            .child(self.find_glyph_button("chat-find-close", "✕", "Close (Esc)", cx.listener(|this, _e, window, cx| this.close_find(window, cx))));
        Some(bar.into_any_element())
    }

    /// A small glyph button for the find bar (prev/next/close).
    fn find_glyph_button(
        &self,
        id: &'static str,
        glyph: &'static str,
        tooltip: &'static str,
        on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> impl IntoElement {
        let theme = self.theme;
        let tip = SharedString::from(tooltip);
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(20.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .text_color(theme.fg_subtle)
            .hover(|s| s.bg(theme.hover_overlay).text_color(theme.fg_base))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .child(SharedString::from(glyph))
            .on_mouse_down(MouseButton::Left, on_click)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::{AssistantMessage, ToolCall};
    use serde_json::json;

    fn user(text: &str) -> ThreadEntry {
        ThreadEntry::User { text: text.into(), images: vec![], checkpoint: None }
    }
    fn assistant(text: &str) -> ThreadEntry {
        ThreadEntry::Assistant(AssistantMessage { text: text.into(), thinking: String::new() })
    }

    #[test]
    fn matches_are_case_insensitive_and_span_user_assistant_tool_text() {
        let mut tool = ToolCall::new("t1", "Bash", json!({"command": "grep FOO src"}));
        tool.result = Some("no results".into());
        let entries = vec![
            user("Please Find the widget"),        // 0: user text
            assistant("Here is the WIDGET code"),  // 1: assistant text
            ThreadEntry::ToolCall(tool),           // 2: tool command carries "FOO"
            user("unrelated"),                     // 3
        ];
        // Case-insensitive user + assistant hit.
        assert_eq!(recompute_matches(&entries, "widget"), vec![0, 1]);
        // Tool command text is searchable (via tool_target).
        assert_eq!(recompute_matches(&entries, "foo"), vec![2]);
        // Empty / whitespace query → no matches (bar shows nothing).
        assert!(recompute_matches(&entries, "   ").is_empty());
        // No hit → empty.
        assert!(recompute_matches(&entries, "zzz").is_empty());
    }
}
