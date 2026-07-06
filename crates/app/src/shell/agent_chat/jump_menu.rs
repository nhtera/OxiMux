//! "Jump to message" dropdown — a compact trigger at the top of the transcript
//! that opens a list of the conversation's user turns (`#N` + one-line preview)
//! with a "Jump to bottom" footer. It pairs with the left tick-rail
//! ([`super::message_rail`]) to make up the conversation timeline: the rail is
//! the always-on scrubber, the dropdown is the labelled jump list.
//!
//! Clicking a row (or arrowing to it while the menu is open) scrolls that turn
//! into view and briefly flashes it via the shared `scroll_to_user_ordinal`
//! primitive. Both the trigger and the popover are rendered as absolute overlays
//! anchored to the top-left of the transcript, so they float over the messages
//! rather than pushing the composer around.

use gpui::prelude::FluentBuilder;
use gpui::{div, px, AnyElement, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement, Styled};
use oximux_agents::thread::ThreadEntry;

use super::{AgentChatView, RAIL_W};

/// Max characters in a list-row prompt line before eliding with `…`.
const PREVIEW_MAX: usize = 64;
/// Max characters in a row's assistant-reply second line.
const REPLY_MAX: usize = 80;
/// Cap the open list's height; past this it scrolls internally.
const LIST_MAX_H: f32 = 260.0;
/// Fixed width of the open list.
const MENU_W: f32 = 320.0;

/// Number of user turns in the thread — the tick/row count.
pub(super) fn user_turn_count(entries: &[ThreadEntry]) -> usize {
    entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count()
}

/// Character length of each user turn's text, in order — drives the rail's
/// varying tick lengths (longer message → longer tick), like a document minimap.
pub(super) fn user_turn_lengths(entries: &[ThreadEntry]) -> Vec<usize> {
    entries
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::User { text, .. } => Some(text.chars().count()),
            _ => None,
        })
        .collect()
}

/// A user turn's entry in the jump list: its 0-based ordinal, the one-line
/// prompt preview, and a one-line preview of the assistant reply that followed.
/// `reply` is empty when the turn has no visible reply yet (the live tail
/// mid-stream) or produced only tool calls.
pub(super) struct TurnPreview {
    pub ordinal: usize,
    pub prompt: String,
    pub reply: String,
}

/// Build a [`TurnPreview`] for every user turn, in transcript order. Pure so the
/// ordinal mapping is unit-testable. `ordinal` is 0-based among user turns —
/// exactly what `user_entry_index` inverts, so it feeds `scroll_to_user_ordinal`
/// directly. `prompt` is the first non-blank line (whitespace-collapsed, elided,
/// machine wrappers stripped); an image-only / empty message gets a synthetic
/// label instead. `reply` pairs the turn with its outcome, so a row previews
/// both the ask and the answer.
pub(super) fn user_turn_previews(entries: &[ThreadEntry]) -> Vec<TurnPreview> {
    let mut out = Vec::new();
    let mut ordinal = 0usize;
    for (i, e) in entries.iter().enumerate() {
        if let ThreadEntry::User { text, images, .. } = e {
            out.push(TurnPreview {
                ordinal,
                prompt: first_line(text, images.len(), PREVIEW_MAX),
                reply: reply_after(entries, i),
            });
            ordinal += 1;
        }
    }
    out
}

/// One-line preview of the first non-empty assistant reply following the user
/// turn at `user_idx`, stopping at the next user turn. Empty when the turn has
/// no visible reply yet (still streaming) or produced only tool calls.
fn reply_after(entries: &[ThreadEntry], user_idx: usize) -> String {
    for e in &entries[user_idx + 1..] {
        match e {
            ThreadEntry::User { .. } => break,
            ThreadEntry::Assistant(msg) if !msg.text.trim().is_empty() => {
                return elide(&collapse_first_line(&msg.text), REPLY_MAX);
            }
            _ => {}
        }
    }
    String::new()
}

/// First non-blank line of `text`, whitespace-collapsed and elided to `max`.
/// Falls back to an image count / empty marker so every row shows something.
fn first_line(text: &str, image_count: usize, max: usize) -> String {
    let collapsed = collapse_first_line(&readable_text(text));
    if collapsed.is_empty() {
        return if image_count > 0 {
            format!("({image_count} image{})", if image_count == 1 { "" } else { "s" })
        } else {
            "(empty message)".to_string()
        };
    }
    elide(&collapsed, max)
}

/// First non-blank line of `text` with inner whitespace runs collapsed to single
/// spaces. Empty when `text` has no non-blank line.
fn collapse_first_line(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    first.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a raw user-turn body into something readable for the jump list: a slash
/// command shows just its name (`/clear`), and machine wrappers the composer
/// prepends — `@`-context blocks and the local-command caveat/scaffolding — are
/// stripped so the row shows what the user actually said, not the plumbing.
fn readable_text(text: &str) -> String {
    if let Some(name) = between(text, "<command-name>", "</command-name>") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    // Drop whole noise blocks (inner content and all), then any stray tags.
    let mut s = text.to_string();
    for (open, close) in [
        ("<context", "</context>"),
        ("<local-command-caveat>", "</local-command-caveat>"),
        ("<command-message>", "</command-message>"),
        ("<command-args>", "</command-args>"),
    ] {
        s = remove_blocks(&s, open, close);
    }
    strip_angle_tags(&s)
}

/// The text between the first `open`…`close` pair, if both are present.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

/// Remove every `open`…`close` span (inclusive) from `text`. `open` may be a tag
/// prefix (e.g. `<context`) so attributes are covered.
fn remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            None => {
                // Unterminated — drop the remainder so half a wrapper never shows.
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Drop `<…>` tag substrings, keeping their inner text.
fn strip_angle_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Truncate `s` to at most `max` chars, appending `…` when it was cut. Counts
/// chars (not bytes) so multibyte text isn't split mid-codepoint.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl AgentChatView {
    /// The jump-to-message list — revealed by hovering the tick-rail (or the
    /// list itself), like a document minimap that expands to labelled rows. Each
    /// row previews a user turn — its prompt plus a muted line of the reply that
    /// followed; clicking one scrolls it into view (+ flash). A "Jump to bottom"
    /// footer returns to the live tail. `None`
    /// unless the pointer is over the rail/list AND there are ≥2 user turns.
    ///
    /// Positioned edge-to-edge with the rail (a hair of overlap, no gap) so the
    /// pointer crossing from rail to list never leaves both hover regions at
    /// once — that's what keeps the list open while you move onto it.
    pub(super) fn render_jump_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !(self.rail_hover || self.menu_hover) {
            return None;
        }
        let previews = user_turn_previews(&self.thread.entries);
        if previews.len() < 2 {
            return None;
        }
        let theme = self.theme;
        let typo = &self.typography;
        let active = self.current_user_ordinal().min(previews.len() - 1);

        // Scrollable region: the turn rows. `flex_1` + `min_h_0` lets it shrink
        // below its content height and scroll internally, so the footer below
        // stays pinned in view instead of scrolling away with the rows.
        let mut rows = div()
            .id("jump-rows")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(4.0));
        for TurnPreview { ordinal, prompt, reply } in previews {
            let selected = ordinal == active;
            let has_reply = !reply.is_empty();
            rows = rows.child(
                div()
                    .id(("jump-row", ordinal))
                    .flex()
                    .flex_col()
                    .w_full()
                    .px(px(9.0))
                    .py(px(5.0))
                    .gap(px(1.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .when(selected, |s| s.bg(theme.hover_overlay))
                    .hover(|s| s.bg(theme.hover_overlay))
                    // Prompt (the ask). Each line is wrapped in a flex-row: a
                    // truncated text node placed directly in a flex-col renders
                    // blank in gpui, so it needs a row parent to lay out width.
                    .child(
                        div().flex().flex_row().w_full().child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(typo.t_body_sm))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if selected { theme.fg_base } else { theme.fg_muted })
                                .child(SharedString::from(prompt)),
                        ),
                    )
                    // Reply (the outcome) — muted + smaller, mirroring how a
                    // hover card pairs each prompt with its answer. Omitted for
                    // turns without a visible reply (live tail, tool-only).
                    .when(has_reply, |row| {
                        row.child(
                            div().flex().flex_row().w_full().child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(typo.t_label_xs))
                                    .text_color(theme.fg_subtle)
                                    .child(SharedString::from(reply)),
                            ),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, window, cx| {
                            this.scroll_to_user_ordinal(ordinal, window, cx)
                        }),
                    ),
            );
        }
        // Pinned footer: a hairline + the live-tail action. `flex_none` keeps it
        // out of the scroll region, so "Jump to bottom" is always reachable no
        // matter how far the rows are scrolled.
        let footer = div()
            .flex_none()
            .flex()
            .flex_col()
            .px(px(4.0))
            .pb(px(4.0))
            .child(div().h(px(1.0)).w_full().mb(px(3.0)).bg(theme.border_inactive))
            .child(
                div()
                    .id("jump-list-bottom")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .w_full()
                    .px(px(9.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover_overlay))
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("↓"))
                    .child(SharedString::from("Jump to bottom"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| this.jump_list_to_bottom(cx)),
                    ),
            );

        let list = div()
            .id("jump-list")
            .absolute()
            .top(px(8.0))
            .left(px(RAIL_W - 2.0))
            .flex()
            .flex_col()
            .w(px(MENU_W))
            .max_h(px(LIST_MAX_H))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .shadow_lg()
            .on_hover(cx.listener(|this, hovered: &bool, _w, cx| {
                this.menu_hover = *hovered;
                cx.notify();
            }))
            .child(rows)
            .child(footer);
        Some(list.into_any_element())
    }

    /// Footer action: return to the live tail and re-arm auto-follow.
    pub(super) fn jump_list_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::ThreadEntry;

    fn user(text: &str) -> ThreadEntry {
        ThreadEntry::User { text: text.into(), images: vec![], checkpoint: None }
    }

    fn assistant(text: &str) -> ThreadEntry {
        ThreadEntry::Assistant(oximux_agents::thread::AssistantMessage {
            text: text.into(),
            thinking: String::new(),
        })
    }

    #[test]
    fn previews_number_user_turns_in_order_skipping_non_user() {
        let entries = vec![
            user("first question"),
            ThreadEntry::ContextCompaction { summary: "…".into() },
            user("second\nquestion with more"),
        ];
        let got = user_turn_previews(&entries);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[0].prompt, "first question");
        // Ordinal counts only user turns; the compaction between them is skipped.
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[1].prompt, "second"); // first non-blank line only
    }

    #[test]
    fn preview_pairs_user_turn_with_following_assistant_reply() {
        let entries = vec![
            user("what is 2+2?"),
            assistant("The answer is 4.\nExtra detail."),
            user("thanks"),
        ];
        let got = user_turn_previews(&entries);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].prompt, "what is 2+2?");
        assert_eq!(got[0].reply, "The answer is 4."); // first non-blank reply line
        // Last turn has no reply yet (live tail) → empty.
        assert_eq!(got[1].reply, "");
    }

    #[test]
    fn reply_skips_empty_assistant_text_and_stops_at_next_user() {
        let entries = vec![
            user("do the thing"),
            // Empty visible text (e.g. thinking-only) is not a reply preview.
            assistant(""),
            user("next"),
            assistant("done"),
        ];
        let got = user_turn_previews(&entries);
        assert_eq!(got[0].reply, ""); // empty assistant skipped, next user stops scan
        assert_eq!(got[1].reply, "done");
    }

    #[test]
    fn count_matches_user_turns_only() {
        let entries = vec![
            user("a"),
            ThreadEntry::ContextCompaction { summary: "…".into() },
            user("b"),
            user("c"),
        ];
        assert_eq!(user_turn_count(&entries), 3);
    }

    #[test]
    fn first_line_collapses_whitespace_and_takes_first_nonblank_line() {
        assert_eq!(first_line("\n\n   hello   world  \nmore", 0, PREVIEW_MAX), "hello world");
    }

    #[test]
    fn first_line_falls_back_for_empty_and_image_only() {
        assert_eq!(first_line("", 0, PREVIEW_MAX), "(empty message)");
        assert_eq!(first_line("   ", 2, PREVIEW_MAX), "(2 images)");
        assert_eq!(first_line("", 1, PREVIEW_MAX), "(1 image)");
    }

    #[test]
    fn elide_counts_chars_not_bytes() {
        let s = "é".repeat(100); // 2 bytes each, 100 chars
        let out = elide(&s, PREVIEW_MAX);
        assert_eq!(out.chars().count(), PREVIEW_MAX);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn elide_leaves_short_strings_untouched() {
        assert_eq!(elide("short", PREVIEW_MAX), "short");
    }

    #[test]
    fn readable_shows_slash_command_name() {
        let raw = "<command-name>/clear</command-name>\n\
                   <command-message>clear</command-message>\n<command-args></command-args>";
        assert_eq!(first_line(raw, 0, PREVIEW_MAX), "/clear");
    }

    #[test]
    fn readable_strips_context_block_keeps_user_text() {
        let raw = "<context name=\"terminal\">lots of\nscrollback here</context>what went wrong?";
        assert_eq!(first_line(raw, 0, PREVIEW_MAX), "what went wrong?");
    }

    #[test]
    fn readable_strips_caveat_and_stray_tags() {
        assert_eq!(
            first_line("<local-command-caveat>note</local-command-caveat>real ask", 0, PREVIEW_MAX),
            "real ask"
        );
        assert_eq!(strip_angle_tags("a <b>c</b> d"), "a c d");
    }

    #[test]
    fn remove_blocks_drops_unterminated_tail() {
        // A wrapper with no closing tag drops the rest rather than showing half.
        assert_eq!(remove_blocks("keep <context oops no close", "<context", "</context>"), "keep ");
    }
}
