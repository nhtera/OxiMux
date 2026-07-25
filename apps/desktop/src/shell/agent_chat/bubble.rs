//! Pure (cx-free) renderers for chat transcript pieces.
//!
//! Each function builds the *static* content of one bubble kind — user text,
//! the assistant's markdown body, a thinking block body, or the one-line
//! tool-call placeholder. The interactive shell (the thinking disclosure
//! toggle, the scroll container) lives in the view; keeping these pure means
//! the view file stays small and this file needs no `Context`.

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{ActiveTheme, clipboard::Clipboard, h_flex};
use oximux_agents::thread::{ToolCall, ToolCallStatus};
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

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

/// The assistant's visible reply, rendered as GitHub-flavored markdown. `key`
/// discriminates the renderer's per-bubble state so two bubbles never share it.
pub(super) fn assistant_body(key: usize, body: &str, typo: &Typography) -> AnyElement {
    // Dark-only app: pin the markdown renderer to the dark highlight theme (its
    // own default is a light code theme, which reads washed-out on the panel).
    let style = TextViewStyle {
        is_dark: true,
        highlight_theme: HighlightTheme::default_dark(),
        ..Default::default()
    };
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
                .code_block_actions(|code_block, _window, cx| {
                    let code = code_block.code();
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when_some(code_block.lang(), |this, lang| {
                            this.child(
                                div()
                                    .text_size(px(11.0))
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
    key: usize,
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let style = TextViewStyle {
        is_dark: true,
        highlight_theme: HighlightTheme::default_dark(),
        ..Default::default()
    };
    div()
        .w_full()
        // Same wrap trap as `assistant_body`: markdown reports its longest
        // unwrapped line as min-content, so zero the min-width to force wrapping.
        .min_w_0()
        .border_l_2()
        .border_color(theme.border_inactive)
        .pl(px(density.pad_panel))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
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

/// The shell command to present for `tc`: the script the agent actually ran,
/// rather than the login shell Codex wraps every exec in (`/bin/zsh -lc
/// '<script>'`), which is noise on every command card.
///
/// Codex reports the script itself in `commandActions`, so we read that instead
/// of parsing it back out of the wrapper — recovering it from the quoted string
/// would mean re-implementing shell quoting, and Codex switches between `'` and
/// `"` depending on the script's own quotes. Falls back to the raw command, so
/// a provider that already sends a bare command (Claude) is untouched. The raw
/// wrapper stays on the input for the copy and raw-JSON views.
pub(super) fn display_command(tc: &ToolCall) -> &str {
    let raw = tc.input.get("command").and_then(Value::as_str).unwrap_or_default();
    unwrapped_command(&tc.input).unwrap_or(raw)
}

/// The lone script `commandActions` reports for this exec, if it reports
/// exactly one. Several actions are left alone: re-joining them would mean
/// guessing the operator that ran between them, and `a && b` shown as two lines
/// reads as something the agent didn't do.
fn unwrapped_command(input: &Value) -> Option<&str> {
    let actions = input.get("commandActions")?.as_array()?;
    let [only] = actions.as_slice() else { return None };
    only.get("command").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// A short human target for the tool line — the file it touches, the command it
/// runs, the query it searches, a click coordinate, or a subagent/task brief.
/// `None` when the input carries none. Kept legible for the collapsed header,
/// which is what the user sees without expanding the card.
pub(super) fn tool_target(tc: &ToolCall) -> Option<String> {
    let input = &tc.input;
    // A file the tool touches.
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A Codex `apply_patch` names its files inside `changes[]` rather than at
    // the top level: one file reads as its path, several as a count (the diff
    // rows caption each path individually). Gated by name, not shape — a
    // server-defined MCP tool is free to take an unrelated `changes` array.
    if tc.name == "apply_patch"
        && let Some(changes) = input.get("changes").and_then(Value::as_array).filter(|a| !a.is_empty())
    {
        if let [only] = changes.as_slice()
            && let Some(path) = only.get("path").and_then(Value::as_str)
        {
            return Some(path.to_string());
        }
        return Some(format!("{} files", changes.len()));
    }
    // The command a shell tool runs, minus any wrapper shell.
    if input.get("command").and_then(Value::as_str).is_some() {
        return Some(display_command(tc).to_string());
    }
    // A pattern / url / query the tool acts on.
    for key in ["pattern", "url", "query"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A pointer target, e.g. computer-use clicks: `coordinate: [x, y]`.
    if let Some(coord) = input.get("coordinate").and_then(Value::as_array)
        && coord.len() >= 2
    {
        return Some(format!("({}, {})", round_num(&coord[0]), round_num(&coord[1])));
    }
    // A batch of UI actions → a count, not the whole array.
    if let Some(actions) = input.get("actions").and_then(Value::as_array) {
        let n = actions.len();
        return Some(format!("{n} action{}", if n == 1 { "" } else { "s" }));
    }
    // A Codex collab/sub-agent item: its action verb and who it targets, rather
    // than the `prompt` the generic fallback below would pick — the prompt
    // already reads in full on the card body, and "Spawn agent · 2 agents" is
    // what distinguishes two collab cards in a collapsed run. Gated on the item
    // `type` Codex carries on the input, so a Claude `Agent` call (no `type`
    // field) still falls through to its brief.
    if let Some(t) = collab_target(input) {
        return Some(t);
    }
    // Typed text / key chord, or a subagent/task brief → its first line.
    for key in ["text", "description", "prompt"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            let first = s.lines().next().unwrap_or(s);
            if !first.trim().is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// The header target for a Codex collab (`collabAgentToolCall`) or sub-agent
/// activity (`subAgentActivity`) card. `None` for any other input, including a
/// Claude subagent call.
///
/// A collab call reads as its action plus the number of agents it addresses
/// ("Spawn agent · 2 agents"); a single-agent call stays just the verb, since
/// "1 agent" is what every collab call that isn't a broadcast looks like. An
/// activity beat reads as the agent it belongs to.
fn collab_target(input: &Value) -> Option<String> {
    match input.get("type").and_then(Value::as_str)? {
        "collabAgentToolCall" => {
            let verb = input.get("tool").and_then(Value::as_str).map(collab_tool_label);
            let receivers = input
                .get("receiverThreadIds")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            match (verb, receivers) {
                (Some(v), n) if n > 1 => Some(format!("{v} · {n} agents")),
                (Some(v), _) => Some(v),
                (None, n) if n > 0 => Some(format!("{n} agent{}", if n == 1 { "" } else { "s" })),
                (None, _) => None,
            }
        }
        "subAgentActivity" => {
            let path = input.get("agentPath").and_then(Value::as_str).filter(|s| !s.is_empty());
            let kind = input.get("kind").and_then(Value::as_str).filter(|s| !s.is_empty());
            match (path, kind) {
                (Some(p), Some(k)) => Some(format!("{p} · {k}")),
                (Some(p), None) => Some(p.to_string()),
                (None, Some(k)) => Some(k.to_string()),
                (None, None) => None,
            }
        }
        _ => None,
    }
}

/// Humanize a `CollabAgentTool` enum value (`spawnAgent` → "Spawn agent"). An
/// unrecognized value renders verbatim rather than being guessed at — a future
/// collab action should read as itself, not as the wrong verb.
fn collab_tool_label(tool: &str) -> String {
    match tool {
        "spawnAgent" => "Spawn agent".to_string(),
        "sendInput" => "Send input".to_string(),
        "resumeAgent" => "Resume agent".to_string(),
        "wait" => "Wait".to_string(),
        "closeAgent" => "Close agent".to_string(),
        other => other.to_string(),
    }
}

/// A JSON number rendered as a whole integer (drops `.0` on coordinates).
fn round_num(v: &Value) -> String {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_f64().map(|f| (f.round() as i64).to_string()))
        .unwrap_or_default()
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
    fn tool_target_prefers_file_then_command_then_pattern() {
        assert_eq!(
            tool_target(&tc("Edit", json!({"file_path": "a.rs"}), ToolCallStatus::InProgress)),
            Some("a.rs".to_string())
        );
        assert_eq!(
            tool_target(&tc("Bash", json!({"command": "ls -la"}), ToolCallStatus::InProgress)),
            Some("ls -la".to_string())
        );
        assert_eq!(
            tool_target(&tc("Grep", json!({"pattern": "foo"}), ToolCallStatus::InProgress)),
            Some("foo".to_string())
        );
        assert_eq!(
            tool_target(&tc("Task", json!({"x": 1}), ToolCallStatus::InProgress)),
            None
        );
    }

    #[test]
    fn display_command_unwraps_the_codex_login_shell() {
        // Wire shapes captured from `codex app-server` 0.144.3. Note the quote
        // style flips with the script's own quotes — which is exactly why the
        // structured action is read instead of the wrapper being parsed.
        let single = tc(
            "Bash",
            json!({
                "command": "/bin/zsh -lc 'echo hello'",
                "commandActions": [{"type": "unknown", "command": "echo hello"}]
            }),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&single), "echo hello");

        let double = tc(
            "Bash",
            json!({
                "command": "/bin/zsh -lc \"sed -n '1,2p' sample.txt\"",
                "commandActions": [
                    {"type": "read", "command": "sed -n '1,2p' sample.txt", "path": "/x/sample.txt"}
                ]
            }),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&double), "sed -n '1,2p' sample.txt");

        // Operators live inside a single action — the card must show the whole
        // script, not a truncated first clause.
        let compound = tc(
            "Bash",
            json!({
                "command": "/bin/zsh -lc 'echo a && echo b || echo c'",
                "commandActions": [{"type": "unknown", "command": "echo a && echo b || echo c"}]
            }),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&compound), "echo a && echo b || echo c");
    }

    #[test]
    fn display_command_falls_back_to_the_raw_command() {
        // Claude sends a bare command with no actions — unchanged.
        let claude = tc("Bash", json!({"command": "ls -la"}), ToolCallStatus::InProgress);
        assert_eq!(display_command(&claude), "ls -la");
        assert_eq!(tool_target(&claude), Some("ls -la".to_string()));

        // Several actions are ambiguous to re-join, so the raw wrapper shows
        // rather than a reconstruction that might not be what ran.
        let multi = tc(
            "Bash",
            json!({
                "command": "/bin/zsh -lc 'a; b'",
                "commandActions": [{"command": "a"}, {"command": "b"}]
            }),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&multi), "/bin/zsh -lc 'a; b'");

        // A malformed/empty action never blanks the card.
        let empty = tc(
            "Bash",
            json!({"command": "/bin/zsh -lc 'x'", "commandActions": [{"command": "  "}]}),
            ToolCallStatus::InProgress,
        );
        assert_eq!(display_command(&empty), "/bin/zsh -lc 'x'");
        let no_cmd = tc("Read", json!({"file_path": "a.rs"}), ToolCallStatus::InProgress);
        assert_eq!(display_command(&no_cmd), "");
    }

    #[test]
    fn tool_target_reads_codex_apply_patch_paths() {
        // One change → its path, like an Edit's `file_path`.
        assert_eq!(
            tool_target(&tc(
                "apply_patch",
                json!({"changes": [{"path": "a.rs", "kind": {"type": "update"}}]}),
                ToolCallStatus::InProgress
            )),
            Some("a.rs".to_string())
        );
        // Several → a count; the per-file paths caption the diff rows instead.
        assert_eq!(
            tool_target(&tc(
                "apply_patch",
                json!({"changes": [{"path": "a.rs"}, {"path": "b.rs"}]}),
                ToolCallStatus::InProgress
            )),
            Some("2 files".to_string())
        );
        // An unrelated tool that happens to take a `changes` array is not a
        // patch — it must not be read as one.
        assert_eq!(
            tool_target(&tc(
                "mcp__store__patch_doc",
                json!({"changes": [{"path": "/a/b", "op": "replace"}]}),
                ToolCallStatus::InProgress
            )),
            None
        );
    }

    #[test]
    fn tool_target_reads_codex_collab_action_and_agent_count() {
        // The collab ACTION verb leads, humanized from the wire enum…
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"type": "collabAgentToolCall", "tool": "spawnAgent",
                       "receiverThreadIds": ["t1"]}),
                ToolCallStatus::InProgress
            )),
            Some("Spawn agent".to_string())
        );
        // …with the receiver count only when it addresses more than one agent.
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"type": "collabAgentToolCall", "tool": "sendInput",
                       "receiverThreadIds": ["t1", "t2"]}),
                ToolCallStatus::InProgress
            )),
            Some("Send input · 2 agents".to_string())
        );
        // The verb beats the `prompt` the generic fallback would otherwise pick —
        // the prompt reads on the card body instead.
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"type": "collabAgentToolCall", "tool": "spawnAgent",
                       "receiverThreadIds": [], "prompt": "Verify the parser"}),
                ToolCallStatus::InProgress
            )),
            Some("Spawn agent".to_string())
        );
        // An unknown future action renders verbatim, never as a guessed verb.
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"type": "collabAgentToolCall", "tool": "teleportAgent",
                       "receiverThreadIds": []}),
                ToolCallStatus::InProgress
            )),
            Some("teleportAgent".to_string())
        );
        // No `tool` on the wire → fall back to who it targets.
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"type": "collabAgentToolCall", "receiverThreadIds": ["t1", "t2"]}),
                ToolCallStatus::InProgress
            )),
            Some("2 agents".to_string())
        );
        // An activity beat identifies its agent.
        assert_eq!(
            tool_target(&tc(
                "Agent activity",
                json!({"type": "subAgentActivity", "agentPath": "reviewer",
                       "agentThreadId": "t7", "kind": "started"}),
                ToolCallStatus::InProgress
            )),
            Some("reviewer · started".to_string())
        );
        // A Claude subagent call carries no item `type` — it must keep reading as
        // its brief rather than being treated as a collab item.
        assert_eq!(
            tool_target(&tc(
                "Task",
                json!({"description": "Audit the parser", "prompt": "Check X"}),
                ToolCallStatus::InProgress
            )),
            Some("Audit the parser".to_string())
        );
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
    fn tool_target_covers_mcp_and_agent_shapes() {
        // computer-use click → coordinate tuple
        assert_eq!(
            tool_target(&tc(
                "mcp__computer-use__left_click",
                json!({"coordinate": [948, 810]}),
                ToolCallStatus::InProgress
            )),
            Some("(948, 810)".to_string())
        );
        // computer-use batch → action count
        assert_eq!(
            tool_target(&tc(
                "mcp__computer-use__computer_batch",
                json!({"actions": [{}, {}, {}]}),
                ToolCallStatus::InProgress
            )),
            Some("3 actions".to_string())
        );
        // web fetch → url; search → query
        assert_eq!(
            tool_target(&tc("WebFetch", json!({"url": "https://x.dev"}), ToolCallStatus::InProgress)),
            Some("https://x.dev".to_string())
        );
        assert_eq!(
            tool_target(&tc("WebSearch", json!({"query": "gpui focus"}), ToolCallStatus::InProgress)),
            Some("gpui focus".to_string())
        );
        // Agent → first line of the prompt
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"prompt": "Find the bug\nin detail"}),
                ToolCallStatus::InProgress
            )),
            Some("Find the bug".to_string())
        );
    }

    #[test]
    fn display_name_humanizes_mcp_ids() {
        assert_eq!(tool_display_name("mcp__computer-use__left_click"), "computer-use · left_click");
        assert_eq!(tool_display_name("Bash"), "Bash");
    }
}
