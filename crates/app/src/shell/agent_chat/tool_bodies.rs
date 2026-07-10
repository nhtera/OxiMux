//! Legible per-tool body renderers for the transcript, so common tools read
//! like a chat instead of a raw-JSON log dump. Only Edit/Write/MultiEdit have a
//! diff (see `diff_card`); this covers Bash/Read/Grep/Glob, Agent/Task, the web
//! tools (WebFetch/WebSearch), and any `mcp__*` tool. Everything else falls back
//! to the generic key:value input + result blocks in `tool_card`.
//!
//! Read-only and built on demand (the caller only invokes these when a card is
//! expanded), and every output is capped by chars then rows so a chatty tool
//! can't blow up layout.

use gpui::{AnyElement, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::{ToolCall, ToolDetail};
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

use super::bubble;

/// Rows shown before truncation, and a char cap applied first (UTF-8-safe via
/// `bubble::elide`, which counts/takes by `char`, never slicing mid-codepoint).
const MAX_ROWS: usize = 200;
const MAX_CHARS: usize = 8000;

/// Dispatch a tool call to its bespoke body, or `None` to fall back to the
/// generic key:value view ([`render_generic_input`]).
///
/// Dispatch runs through the provider-agnostic [`ToolDetail`] classifier so a
/// Claude `Bash`, a Codex command, and an ACP `Execute` all reach the same
/// shell body — turning ACP's former generic fallback into rich cards. Claude's
/// routing is unchanged (each Claude name classifies to the archetype that maps
/// back to its original body — see the dispatch test).
pub(super) fn render_tool_body(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> Option<AnyElement> {
    match ToolDetail::classify(&tc.name, tc.kind.as_deref(), &tc.input) {
        ToolDetail::Shell => Some(render_bash(tc, theme, density, typo)),
        ToolDetail::Read => Some(render_read(tc, theme, density, typo)),
        // Preserve Claude's `Glob` "file" noun; every other search reads "match".
        ToolDetail::Search => {
            let noun = if tc.name == "Glob" { "file" } else { "match" };
            Some(render_matches(tc, noun, theme, density, typo))
        }
        ToolDetail::Fetch => Some(render_webfetch(tc, theme, density, typo)),
        ToolDetail::WebSearch => Some(render_websearch(tc, theme, density, typo)),
        ToolDetail::SubAgent => Some(render_agent(tc, theme, density, typo)),
        // MCP tools are journaled as `mcp__<server>__<tool>`; the header already
        // humanizes the name, so the body stays a terse input+result view.
        ToolDetail::Mcp => Some(render_mcp(tc, theme, density, typo)),
        // Edit/Write render as the diff card (built in `tool_card` before this is
        // reached); Plan feeds the pinned plan panel; Plain/Unknown use the clean
        // generic key:value card.
        ToolDetail::Edit
        | ToolDetail::Write
        | ToolDetail::Plan
        | ToolDetail::Plain
        | ToolDetail::Unknown => None,
    }
}

/// Read a string field from the structured (`toolUseResult`) result, if present.
fn structured_str<'a>(tc: &'a ToolCall, key: &str) -> Option<&'a str> {
    tc.structured.as_ref()?.get(key)?.as_str()
}

/// A legible key:value view of a tool's input — one row per top-level field —
/// so a tool with no bespoke body reads better than a pretty-printed JSON blob.
/// Object values collapse to compact one-line JSON (char-capped by `code_block`);
/// a non-object input renders as a single summary row.
pub(super) fn render_generic_input(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let rows: Vec<String> = match &tc.input {
        Value::Object(map) if !map.is_empty() => {
            map.iter().map(|(k, v)| format!("{k}: {}", value_summary(v))).collect()
        }
        Value::Null => Vec::new(),
        other => vec![value_summary(other)],
    };
    let body = if rows.is_empty() { "(no input)".to_string() } else { rows.join("\n") };
    code_block(&body, theme.fg_muted, theme, density, typo)
}

/// Collapse a JSON value to a one-line summary for a key:value row. Strings keep
/// their text (newlines flattened to spaces); containers become compact JSON.
fn value_summary(v: &Value) -> String {
    match v {
        Value::String(s) => s.replace('\n', " "),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn input_str<'a>(tc: &'a ToolCall, key: &str) -> &'a str {
    tc.input.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Bash → a terminal-style card: the `$`-prefixed command, then its output.
/// When the structured result splits stdout/stderr, stderr renders in a warn
/// tone and an interrupted run is called out; otherwise the flattened result is
/// shown as one block.
fn render_bash(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let cmd = input_str(tc, "command");
    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();

    if !cmd.trim().is_empty() {
        let prompted = cmd
            .lines()
            .enumerate()
            .map(|(i, l)| if i == 0 { format!("$ {l}") } else { format!("  {l}") })
            .collect::<Vec<_>>()
            .join("\n");
        col = col.child(code_block(&prompted, theme.status_info, theme, density, typo));
    } else if matches!(&tc.input, Value::Object(m) if !m.is_empty()) {
        // A shell-classified tool that doesn't use Claude's `command` key (an ACP
        // `Execute` whose input is agent-specific) — show its raw input as
        // key:value so nothing is lost, then its output below.
        col = col.child(render_generic_input(tc, theme, density, typo));
    }

    let stdout = structured_str(tc, "stdout").filter(|s| !s.trim().is_empty());
    let stderr = structured_str(tc, "stderr").filter(|s| !s.trim().is_empty());
    if stdout.is_some() || stderr.is_some() {
        if let Some(o) = stdout {
            col = col.child(code_block(o, theme.fg_muted, theme, density, typo));
        }
        if let Some(e) = stderr {
            col = col.child(code_block(e, theme.status_warn, theme, density, typo));
        }
    } else if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }

    let interrupted = tc
        .structured
        .as_ref()
        .and_then(|s| s.get("interrupted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if interrupted {
        col = col.child(caption("interrupted".to_string(), theme, typo));
    }
    col.into_any_element()
}

/// Read → the returned file slice as a code block, with a line-count caption
/// from the structured result when present (the path is already in the header).
fn render_read(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let body = tc.result.as_deref().unwrap_or("");
    let lines = tc
        .structured
        .as_ref()
        .and_then(|s| s.get("file"))
        .and_then(|f| f.get("numLines"))
        .and_then(Value::as_u64);
    let mut col = div().flex().flex_col().gap(px(2.0)).w_full();
    if let Some(n) = lines {
        col = col.child(caption(format!("{n} line{}", if n == 1 { "" } else { "s" }), theme, typo));
    }
    col = col.child(code_block(body, theme.fg_muted, theme, density, typo));
    col.into_any_element()
}

/// Agent / Task → the subagent brief (from input) plus a summary line from the
/// structured result (type · status · tokens · duration) and, when present, the
/// subagent's final content. Falls back to the flattened result when unsettled.
fn render_agent(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();

    let prompt = input_str(tc, "prompt");
    let brief = if prompt.is_empty() { input_str(tc, "description") } else { prompt };
    if !brief.trim().is_empty() {
        col = col.child(code_block(brief, theme.fg_muted, theme, density, typo));
    }

    if let Some(s) = &tc.structured {
        let mut bits: Vec<String> = Vec::new();
        if let Some(t) = s.get("agentType").and_then(Value::as_str) {
            bits.push(t.to_string());
        }
        if let Some(st) = s.get("status").and_then(Value::as_str) {
            bits.push(st.to_string());
        }
        if let Some(tok) = s.get("totalTokens").and_then(Value::as_u64) {
            bits.push(format!("{tok} tokens"));
        }
        if let Some(ms) = s.get("totalDurationMs").and_then(Value::as_u64) {
            bits.push(format!("{}s", ms / 1000));
        }
        if !bits.is_empty() {
            col = col.child(caption(bits.join(" · "), theme, typo));
        }
        if let Some(content) = s.get("content").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        {
            col = col.child(code_block(content, theme.fg_muted, theme, density, typo));
        }
    } else if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }
    col.into_any_element()
}

/// WebFetch → the fetched URL (prominent) plus the extraction prompt and a
/// capped slice of the returned page text. The result carries the payload, so
/// the input is just a header here.
fn render_webfetch(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();
    let url = input_str(tc, "url");
    if !url.trim().is_empty() {
        col = col.child(code_block(url, theme.status_info, theme, density, typo));
    }
    let prompt = input_str(tc, "prompt");
    if !prompt.trim().is_empty() {
        col = col.child(caption(bubble::elide(prompt, 200), theme, typo));
    }
    if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }
    col.into_any_element()
}

/// WebSearch → the query (prominent) plus the returned result list. The result
/// list (titles/URLs) is the payload, shown as a capped block.
fn render_websearch(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();
    let query = input_str(tc, "query");
    if !query.trim().is_empty() {
        col = col.child(code_block(query, theme.status_info, theme, density, typo));
    }
    if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }
    col.into_any_element()
}

/// MCP (`mcp__<server>__<tool>`) → a single compact-JSON line of the arguments
/// (the header names the server/tool) followed by the capped result. Terser
/// than the generic per-field view for tools whose args are position/coordinate
/// heavy (browser/computer-use), where one line reads better than many.
fn render_mcp(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();
    let summary = match &tc.input {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if !summary.is_empty() {
        col = col.child(code_block(&summary, theme.fg_muted, theme, density, typo));
    }
    if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }
    col.into_any_element()
}

/// Grep/Glob → a count header + the match/file list (`noun` = "match"/"file").
fn render_matches(
    tc: &ToolCall,
    noun: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let result = tc.result.as_deref().unwrap_or("");
    let n = result.lines().filter(|l| !l.trim().is_empty()).count();
    let header = format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .w_full()
        .child(caption(header, theme, typo))
        .child(code_block(result, theme.fg_muted, theme, density, typo))
        .into_any_element()
}

/// A bordered output block. Each line is wrapped in its OWN flex-row — a bare
/// line of text placed directly in a flex-col renders blank (a known gpui trap)
/// — and the block is char-capped then row-capped with a "… N more" footer.
fn code_block(
    text: &str,
    color: Hsla,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let capped = bubble::elide(text, MAX_CHARS);
    let lines: Vec<&str> = capped.lines().collect();
    let total = lines.len();

    let mut block = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(density.r_xs))
        .bg(theme.bg_base)
        .px(px(density.pad_row))
        .py(px(4.0))
        .text_size(px(typo.t_body_sm))
        .text_color(color);
    for line in lines.iter().take(MAX_ROWS) {
        // Preserve blank lines with a space so the row keeps its height.
        let shown = if line.is_empty() { " ".to_string() } else { (*line).to_string() };
        block = block.child(div().flex().flex_row().w_full().child(SharedString::from(shown)));
    }
    if total > MAX_ROWS {
        block = block.child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .text_color(theme.fg_subtle)
                .child(SharedString::from(format!("… {} more lines", total - MAX_ROWS))),
        );
    }
    block.into_any_element()
}

fn caption(label: String, theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(label))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_covers_known_tools_only() {
        let mk = |name: &str| {
            let mut tc = ToolCall::new("t", name, json!({"command": "ls", "file_path": "a", "pattern": "x"}));
            tc.result = Some("line one\nline two".into());
            tc
        };
        for t in [
            "Bash", "Read", "Grep", "Glob", "Agent", "Task", "WebFetch", "WebSearch",
            "mcp__chrome-devtools-mcp__click",
        ] {
            assert!(
                render_tool_body(&mk(t), Theme::default(), Density::default(), &Typography::default()).is_some(),
                "{t} should render a bespoke body"
            );
        }
        // Edit/Write/MultiEdit render a diff in `tool_card`, not here; a truly
        // unknown tool falls back to the generic key:value card.
        for t in ["Edit", "Write", "MultiEdit", "SomethingNew"] {
            assert!(
                render_tool_body(&mk(t), Theme::default(), Density::default(), &Typography::default()).is_none(),
                "{t} should fall back to the generic card"
            );
        }
    }

    #[test]
    fn cross_provider_tools_route_through_the_classifier() {
        let (theme, density, typo) = (Theme::default(), Density::default(), Typography::default());

        // Codex names its web search `web_search` (not Claude's `WebSearch`); it
        // now reaches the rich WebSearch body via the classifier.
        let mut codex_search = ToolCall::new("t", "web_search", json!({"query": "gpui focus"}));
        codex_search.result = Some("1. Title — https://a".into());
        assert!(render_tool_body(&codex_search, theme, density, &typo).is_some());

        // An ACP `Execute` tool: freeform title, kind carried on `tc.kind`, and an
        // agent-specific input key (not Claude's `command`). It routes to the
        // shell body AND must not lose its raw input.
        let mut acp_exec = ToolCall::new("t", "Run the build", json!({"cmd": "cargo build"}));
        acp_exec.kind = Some("execute".into());
        acp_exec.result = Some("Compiling…".into());
        assert!(
            render_tool_body(&acp_exec, theme, density, &typo).is_some(),
            "ACP execute should render a shell card, not the generic fallback"
        );

        // An ACP `Read` tool routes to the read body from its kind alone.
        let mut acp_read = ToolCall::new("t", "Read src/main.rs", json!({"path": "src/main.rs"}));
        acp_read.kind = Some("read".into());
        acp_read.result = Some("fn main() {}".into());
        assert!(render_tool_body(&acp_read, theme, density, &typo).is_some());

        // An unclassified ACP tool (freeform title, no kind) still falls to the
        // generic card — conservative, never a wrong-body guess.
        let acp_unknown = ToolCall::new("t", "Do something novel", json!({"opt": true}));
        assert!(render_tool_body(&acp_unknown, theme, density, &typo).is_none());

        // An ACP tool whose kind is unrecognized (mode switch / future) → generic.
        let mut acp_switch = ToolCall::new("t", "Switch mode", json!({}));
        acp_switch.kind = Some("switch_mode".into());
        assert!(render_tool_body(&acp_switch, theme, density, &typo).is_none());
    }

    #[test]
    fn structured_bodies_render_without_panic() {
        let theme = Theme::default();
        let density = Density::default();
        let typo = Typography::default();
        // Bash with split stdout/stderr + interrupted.
        let mut bash = ToolCall::new("t", "Bash", json!({"command": "make"}));
        bash.structured = Some(json!({"stdout": "ok", "stderr": "warn: x", "interrupted": true}));
        let _ = render_tool_body(&bash, theme, density, &typo);
        // Read with a numLines caption.
        let mut read = ToolCall::new("t", "Read", json!({"file_path": "a.rs"}));
        read.structured = Some(json!({"file": {"numLines": 42}}));
        let _ = render_tool_body(&read, theme, density, &typo);
        // Agent with a subagent summary.
        let mut agent = ToolCall::new("t", "Agent", json!({"prompt": "do the thing"}));
        agent.structured = Some(json!({
            "agentType": "researcher", "status": "completed",
            "totalTokens": 12345u64, "totalDurationMs": 8200u64, "content": "found it"
        }));
        let _ = render_tool_body(&agent, theme, density, &typo);
        // Web tools + MCP: URL/query header + result, and a compact MCP input
        // line — all must build, and must degrade gracefully on missing fields.
        let mut fetch = ToolCall::new("t", "WebFetch", json!({"url": "https://ex.com", "prompt": "get x"}));
        fetch.result = Some("page text".into());
        let _ = render_tool_body(&fetch, theme, density, &typo);
        let _ = render_tool_body(&ToolCall::new("t", "WebFetch", json!({})), theme, density, &typo);
        let mut search = ToolCall::new("t", "WebSearch", json!({"query": "gpui focus"}));
        search.result = Some("1. Title — https://a\n2. Title — https://b".into());
        let _ = render_tool_body(&search, theme, density, &typo);
        let mut mcp = ToolCall::new("t", "mcp__chrome-devtools-mcp__click", json!({"uid": "42"}));
        mcp.result = Some("clicked".into());
        let _ = render_tool_body(&mcp, theme, density, &typo);
        let _ = render_tool_body(
            &ToolCall::new("t", "mcp__x__y", json!(null)),
            theme,
            density,
            &typo,
        );
    }

    #[test]
    fn generic_input_summarizes_values_not_raw_json() {
        // Scalars pass through; containers compact; newlines flatten.
        assert_eq!(value_summary(&json!("hi\nthere")), "hi there");
        assert_eq!(value_summary(&json!(42)), "42");
        assert_eq!(value_summary(&json!(true)), "true");
        assert_eq!(value_summary(&json!([1, 2])), "[1,2]");
        assert_eq!(value_summary(&json!({"a": 1})), "{\"a\":1}");
        // The generic body builds for any input shape without panicking.
        let theme = Theme::default();
        let density = Density::default();
        let typo = Typography::default();
        let tc = ToolCall::new("t", "mcp__x__y", json!({"coordinate": [1, 2], "text": "ok"}));
        let _ = render_generic_input(&tc, theme, density, &typo);
        let _ = render_generic_input(&ToolCall::new("t", "n", json!({})), theme, density, &typo);
        let _ = render_generic_input(&ToolCall::new("t", "n", json!(null)), theme, density, &typo);
    }

    #[test]
    fn renderers_survive_empty_and_oversized_inputs() {
        let theme = Theme::default();
        let density = Density::default();
        let typo = Typography::default();
        // Empty: no input fields, no result — must not panic.
        let empty = ToolCall::new("t", "Bash", json!({}));
        let _ = render_tool_body(&empty, theme, density, &typo);
        let _ = render_tool_body(&ToolCall::new("t", "Read", json!({})), theme, density, &typo);
        // Oversized: 10k lines must build (capped) without panic.
        let mut big = ToolCall::new("t", "Bash", json!({"command": "seq 10000"}));
        big.result = Some((0..10_000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n"));
        let _ = render_tool_body(&big, theme, density, &typo);
    }
}
