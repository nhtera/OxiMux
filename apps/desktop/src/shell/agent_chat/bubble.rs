//! Pure (cx-free) renderers for chat transcript pieces.
//!
//! Each function builds the *static* content of one bubble kind — user text,
//! the assistant's markdown body, a thinking block body, or the one-line
//! tool-call placeholder. The interactive shell (the thinking disclosure
//! toggle, the scroll container) lives in the view; keeping these pure means
//! the view file stays small and this file needs no `Context`.

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, clipboard::Clipboard, h_flex};
use oximux_agents::thread::{ToolCall, ToolCallStatus};
use oximux_settings::{Density, Theme, Typography};

use super::markdown_render::{FindMark, MarkdownStyle};
use super::markdown_state::{Markdown, MdKey, owned_renderer};

/// A small role caption above a message ("You" / "Claude").
pub(super) fn role_caption(label: &str, color: Hsla, typo: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typo.t_label_xs))
        .text_color(color)
        .child(SharedString::from(label.to_string()))
}

/// Max width of a user bubble — a prompt longer than this wraps into a column
/// rather than stretching the full measure, keeping the right-aligned shape.
const USER_BUBBLE_MAX_W: f32 = 520.0;

/// The user's prompt as a right-aligned, filled bubble (Claude-Desktop style):
/// visually distinct from the assistant's plain left-aligned text, so the thread
/// reads as an asymmetric back-and-forth. `bg_overlay` (the lightest surface)
/// reads as "my message" lifted off the panel. No `nowrap`/`truncate` (which
/// render blank in a flex column) — the width cap lets the text wrap naturally.
pub(super) fn user_body(
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .justify_end()
        .w_full()
        .child(
            div()
                .max_w(px(USER_BUBBLE_MAX_W))
                .rounded(px(density.r_card))
                .bg(theme.bg_overlay)
                .px(px(density.pad_panel))
                .py(px(6.0))
                .text_size(px(typo.t_body_md))
                .text_color(theme.fg_base)
                .child(SharedString::from(text.to_string())),
        )
        .into_any_element()
}

/// The max width for the user's attached-image thumbnail strip — matches the
/// text bubble so images and prose share the same right edge.
pub(super) const USER_IMAGES_MAX_W: f32 = USER_BUBBLE_MAX_W;

/// The assistant's visible reply, rendered as GitHub-flavored markdown.
///
/// The one place chat chooses a markdown renderer. `key` identifies the message
/// so the owned renderer can keep a parser for it — a streaming reply is
/// re-rendered on every batch of tokens, and re-reading it whole each time is
/// quadratic in its length.
pub(super) fn assistant_body(
    md: &Markdown,
    key: MdKey,
    body: &str,
    find: Option<FindMark>,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    if owned_renderer() {
        let style =
            MarkdownStyle::body(key, md.selection.clone(), theme, density, typo).with_find(find);
        return div()
            .w_full()
            .min_w_0()
            .child(md.render(key, body, &style))
            .into_any_element();
    }
    legacy_assistant_body(key.seed() as usize, body, theme, typo)
}

/// One top-level block of the assistant's reply, as its own transcript row.
///
/// The block-granularity path: a token appended to a long reply re-renders the
/// block it landed in, not the message. `None` when the block index has run past
/// the tree, which a streaming reply can do for a frame between the row list
/// being built and this being asked for it.
///
/// The legacy renderer has no blocks to speak of, so it reports one block and
/// draws its whole body here — the same single element it always drew.
// One more argument than [`assistant_body`], and deliberately the same shape
// otherwise: the two are read side by side, and collapsing this one's style
// arguments into a struct would make the pair harder to compare than the arity
// makes this one hard to call.
#[allow(clippy::too_many_arguments)]
pub(super) fn assistant_block(
    md: &Markdown,
    key: MdKey,
    body: &str,
    block_ix: usize,
    find: Option<FindMark>,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> Option<AnyElement> {
    if !owned_renderer() {
        return (block_ix == 0).then(|| legacy_assistant_body(key.seed() as usize, body, theme, typo));
    }
    let style = MarkdownStyle::body(key, md.selection.clone(), theme, density, typo).with_find(find);
    let block = md.render_block(key, body, block_ix, &style)?;
    Some(div().w_full().min_w_0().child(block).into_any_element())
}

/// The previous renderer, one setting away.
///
/// Chat markdown is the most-looked-at surface in the product; keeping the path
/// that shipped reachable for a release is worth more than the dead code costs.
/// See [`owned_renderer`].
fn legacy_assistant_body(key: usize, body: &str, theme: Theme, typo: &Typography) -> AnyElement {
    let style = crate::shell::markdown_style::gfm_style(&theme);
    // Copied out so the `'static` code-block closure below can carry it: the
    // closure outlives this borrow of `typo`, but an `f32` moves in freely.
    let body_sm = typo.t_body_sm;
    div()
        .w_full()
        // `min_w_0` is load-bearing: the markdown view reports its longest
        // *unwrapped* line as min-content width, and without this a flex ancestor
        // honors that and lets the text overflow the column (clipping at the pane
        // edge) instead of wrapping. Zeroing the min-width forces it to shrink to
        // the column and wrap.
        .min_w_0()
        .text_size(px(typo.t_body_md))
        .child(
            TextView::markdown(("chat-assistant-md", key), body.to_string())
                .style(style)
                // Fenced code gets a language tag + one-click copy on hover, the
                // way a polished chat surfaces a code answer (selection-copy is
                // still available; this is the affordance). The closure resolves
                // the active theme at render time, so this stays cx-free here.
                .code_block_actions(move |code_block, _window, cx| {
                    let code = code_block.code();
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when_some(code_block.lang(), |this, lang| {
                            this.child(
                                div()
                                    .text_size(px(body_sm))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(lang),
                            )
                        })
                        .child(Clipboard::new("chat-code-copy").value(code))
                })
                .selectable(true),
        )
        .into_any_element()
}

/// The extended-thinking body shown when its disclosure is expanded. Muted +
/// left-ruled so it reads as secondary to the reply, and rendered as markdown
/// (thinking traces routinely contain lists / code / emphasis that read as raw
/// source otherwise). `key` discriminates the renderer's per-block state so two
/// thinking blocks never share it.
pub(super) fn thinking_body(
    md: &Markdown,
    key: MdKey,
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    if owned_renderer() {
        let style = MarkdownStyle::thinking(key, md.selection.clone(), theme, density, typo);
        return thinking_frame(theme, density, typo).child(md.render(key, text, &style)).into_any_element();
    }
    legacy_thinking_body(key.seed() as usize, text, theme, density, typo)
}

/// The muted, left-ruled frame a thinking trace reads inside, shared by both
/// renderers so the escape hatch changes the text and nothing around it.
fn thinking_frame(theme: Theme, density: Density, typo: &Typography) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .border_l_2()
        .border_color(theme.border_inactive)
        .pl(px(density.pad_panel))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
}

fn legacy_thinking_body(
    key: usize,
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let style = crate::shell::markdown_style::gfm_style(&theme);
    // Same wrap trap as the assistant body: markdown reports its longest
    // unwrapped line as min-content, so the frame's `min_w_0` is what forces
    // wrapping rather than overflow.
    thinking_frame(theme, density, typo)
        .child(
            TextView::markdown(("chat-thinking-md", key), text.to_string())
                .style(style)
                .selectable(true),
        )
        .into_any_element()
}

/// Status → (glyph, color). `WaitingForConfirmation` reads as a pause because
/// the tool is gated on the user's Allow/Reject decision.
pub(super) fn status_glyph(status: &ToolCallStatus, theme: Theme) -> (&'static str, Hsla) {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => ("▸", theme.fg_subtle),
        ToolCallStatus::WaitingForConfirmation(_) => ("⏸", theme.status_warn),
        // Rendered by the dedicated question card, not this glyph — the arm just
        // keeps the match exhaustive.
        ToolCallStatus::AwaitingAnswer(_) => ("?", theme.status_warn),
        ToolCallStatus::Completed => ("✓", theme.status_ok),
        ToolCallStatus::Failed(_) => ("✗", theme.status_error),
        ToolCallStatus::Rejected | ToolCallStatus::Canceled => ("⊘", theme.fg_subtle),
    }
}

/// A friendly tool label for the header. MCP tools are journaled as
/// `mcp__<server>__<tool>`; show `<server> · <tool>` instead of the raw id so a
/// history full of `mcp__computer-use__left_click` reads like a chat, not a log.
pub(super) fn tool_display_name(name: &str) -> String {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", " · ", 1),
        None => name.to_string(),
    }
}

/// What this call is doing, in one line, for the collapsed header.
///
/// Both of these delegate to [`oximux_agents::thread`], which owns the rule
/// because the agents row's inline Allow and a phone's permission push have to
/// read a pending call the same way the card does — a surface that summarised
/// the arguments its own way would let a person approve one sentence while the
/// host hashed another.
pub(super) fn tool_target(tc: &ToolCall) -> Option<String> {
    oximux_agents::thread::tool_target(&tc.name, &tc.input)
}

/// The script the agent actually ran, minus any wrapper login shell.
pub(super) fn display_command(tc: &ToolCall) -> &str {
    oximux_agents::thread::display_command(&tc.input)
}


/// Cap a label to `max` chars, appending an ellipsis when cut. Keeps the tool
/// header one row regardless of a giant command/path.
pub(super) fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tc(name: &str, input: serde_json::Value, status: ToolCallStatus) -> ToolCall {
        let mut t = ToolCall::new("id", name, input);
        t.status = status;
        t
    }

    #[test]
    fn the_delegates_hand_over_this_call_s_own_name_and_input() {
        // The rule itself is tested in `agent-core`, beside `ToolCall`. What
        // can only break here is the hand-off: a wrapper reading the wrong
        // two fields would summarise some other call, and `apply_patch` is
        // the case that needs BOTH — the name decides whether `changes` is a
        // patch at all.
        let patch = tc("apply_patch", json!({"changes": [{"path": "a.rs"}]}), ToolCallStatus::InProgress);
        assert_eq!(tool_target(&patch), Some("a.rs".to_string()));
        let other = tc("mcp__store__patch_doc", json!({"changes": [{"path": "a.rs"}]}), ToolCallStatus::InProgress);
        assert_eq!(tool_target(&other), None, "the name is read, not guessed from the shape");
        let wrapped = tc(
            "Bash",
            json!({"command": "/bin/zsh -lc 'echo hi'",
                   "commandActions": [{"command": "echo hi"}]}),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&wrapped), "echo hi");
    }

    #[test]
    fn elide_caps_long_strings() {
        assert_eq!(elide("short", 80), "short");
        let long = "x".repeat(100);
        let out = elide(&long, 80);
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn display_name_humanizes_mcp_ids() {
        assert_eq!(tool_display_name("mcp__computer-use__left_click"), "computer-use · left_click");
        assert_eq!(tool_display_name("Bash"), "Bash");
    }
}
