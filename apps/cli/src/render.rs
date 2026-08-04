//! ThreadEvent → compact terminal lines. The fold vocabulary is rendered ONCE,
//! here — one line per tool call and notice, raw text blocks for the
//! assistant's words, permission prompts highlighted — deliberately capped at
//! that: no markdown layout, no color themes, no reflow. Streaming deltas
//! render as nothing; the finalized event carries the authoritative text.

use oximux_agent_core::thread::ThreadEvent;
use serde_json::Value;

/// Cap for one-line summaries (tool inputs, first lines of errors).
const LINE_CAP: usize = 96;

/// First line of `s`, hard-capped at [`LINE_CAP`] chars (char-safe).
fn one_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() <= LINE_CAP {
        return first.to_string();
    }
    let cut: String = first.chars().take(LINE_CAP).collect();
    format!("{cut}…")
}

/// The most human field of a tool call's input: the command for a shell, the
/// path for a file tool, else the input object's compact head.
fn input_summary(input: &Value) -> String {
    for key in ["command", "file_path", "path", "url", "pattern", "prompt", "query"] {
        if let Some(v) = input.get(key).and_then(Value::as_str) {
            return one_line(v);
        }
    }
    match input {
        Value::Null => String::new(),
        other => one_line(&other.to_string()),
    }
}

/// Render one event as terminal text, or `None` for events with nothing to
/// show (streaming deltas, view-internal signals). Multi-line output is only
/// ever the agent's own prose.
pub fn render_event(event: &ThreadEvent) -> Option<String> {
    match event {
        ThreadEvent::UserMessage { text, .. } => Some(format!("❯ {}", one_line(text))),
        ThreadEvent::AssistantText(text) => Some(text.trim_end().to_string()),
        ThreadEvent::SessionInit { model, .. } => Some(format!("· session ready ({model})")),
        ThreadEvent::ToolCallStarted { name, input, .. } => {
            let summary = input_summary(input);
            if summary.is_empty() {
                Some(format!("→ {name}"))
            } else {
                Some(format!("→ {name} {summary}"))
            }
        }
        ThreadEvent::ToolResult { content, is_error: true, .. } => {
            Some(format!("✗ tool failed: {}", one_line(content)))
        }
        ThreadEvent::SubagentAction { line, .. } => Some(format!("  ↳ {}", one_line(line))),
        ThreadEvent::PermissionRequested { request_id, tool_name, description, .. } => {
            let what = if description.is_empty() { tool_name.clone() } else { description.clone() };
            Some(format!(
                "⚠ permission needed: {} (request {request_id}) — decide with `oximux permit`",
                one_line(&what)
            ))
        }
        ThreadEvent::QuestionAsked { request_id, questions, .. } => {
            let head = questions
                .first()
                .map(|q| one_line(&q.question))
                .unwrap_or_else(|| "question".into());
            Some(format!(
                "? question: {head} (request {request_id}) — answer with `oximux permit answer`"
            ))
        }
        ThreadEvent::TurnSummary { detail, .. } => Some(format!("· {}", one_line(detail))),
        ThreadEvent::CompactionStarted => Some("· compacting context…".into()),
        ThreadEvent::CompactBoundary { .. } => Some("— context compacted —".into()),
        ThreadEvent::TurnEnded { is_error, result, usage, .. } => {
            if *is_error {
                let detail = result.as_deref().map(one_line).unwrap_or_default();
                if detail.is_empty() {
                    Some("✗ turn ended with an error".into())
                } else {
                    Some(format!("✗ turn ended with an error: {detail}"))
                }
            } else {
                match usage {
                    Some(u) => Some(format!(
                        "✓ turn ended ({} in / {} out tokens)",
                        u.input_tokens, u.output_tokens
                    )),
                    None => Some("✓ turn ended".into()),
                }
            }
        }
        ThreadEvent::BackgroundTaskStarted { description, .. } => {
            Some(format!("· background task started: {}", one_line(description)))
        }
        ThreadEvent::BackgroundTaskFinished { failed, summary, .. } => {
            let head = summary.as_deref().map(one_line).unwrap_or_default();
            let verdict = if *failed { "failed" } else { "finished" };
            if head.is_empty() {
                Some(format!("· background task {verdict}"))
            } else {
                Some(format!("· background task {verdict}: {head}"))
            }
        }
        ThreadEvent::PlanUpdated { entries } => {
            Some(format!("· plan updated ({} steps)", entries.len()))
        }
        ThreadEvent::AuthRequired { .. } => {
            Some("✗ the agent requires authentication — sign in from the desktop app".into())
        }
        ThreadEvent::SessionResumeStale { .. } => {
            Some("· session no longer resumable — starting fresh".into())
        }
        ThreadEvent::Notice(text) => Some(format!("· {}", one_line(text))),
        ThreadEvent::Diagnostic(text) => Some(format!("! {}", one_line(text))),
        ThreadEvent::Error(text) => Some(format!("✗ {}", one_line(text))),
        ThreadEvent::Rewound { ordinal } => Some(format!("— rewound to turn {ordinal} —")),
        // Streaming deltas: the finalized event carries the authoritative text.
        ThreadEvent::AssistantTextDelta(_)
        | ThreadEvent::ThinkingDelta(_)
        | ThreadEvent::ToolInputDelta { .. }
        | ThreadEvent::ToolOutputDelta { .. }
        | ThreadEvent::LiveUsage(_)
        // Signals only a rich view can act on: pickers, embedded terminals,
        // thumbnails, thinking blocks, tool metadata, auth plumbing.
        | ThreadEvent::AssistantThinking(_)
        | ThreadEvent::ToolResult { is_error: false, .. }
        | ThreadEvent::ToolResultImages { .. }
        | ThreadEvent::ToolTerminal { .. }
        | ThreadEvent::ToolKind { .. }
        | ThreadEvent::BackgroundTaskProgress { .. }
        | ThreadEvent::SlashCommandsUpdated { .. }
        | ThreadEvent::AuthTerminal { .. }
        | ThreadEvent::AuthUrl { .. }
        | ThreadEvent::AuthOutcome { .. }
        | ThreadEvent::ModeChanged { .. }
        | ThreadEvent::ControlsUpdated
        | ThreadEvent::TitleUpdated { .. } => None,
    }
}

/// Render one **folded transcript entry** (a `ThreadEntry` as JSON) compactly.
///
/// Works on the JSON shape rather than the deserialized type on purpose: the
/// entry tree is deep and still evolving, and the transcript verb must render
/// what an older or newer host stored — unknown variants degrade to a marker
/// line instead of failing the whole fetch.
pub fn render_entry(entry: &Value) -> Option<String> {
    if let Some(user) = entry.get("User") {
        let text = user.get("text").and_then(Value::as_str).unwrap_or("");
        return Some(format!("❯ {}", one_line(text)));
    }
    if let Some(assistant) = entry.get("Assistant") {
        let text = assistant.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            return None;
        }
        return Some(text.trim_end().to_string());
    }
    if let Some(tool) = entry.get("ToolCall") {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
        let summary = tool.get("input").map(input_summary).unwrap_or_default();
        if summary.is_empty() {
            return Some(format!("→ {name}"));
        }
        return Some(format!("→ {name} {summary}"));
    }
    if let Some(compaction) = entry.get("ContextCompaction") {
        let summary = compaction.get("summary").and_then(Value::as_str).unwrap_or("");
        return Some(format!("— {} —", one_line(summary)));
    }
    if let Some(diff) = entry.get("TurnDiff") {
        let n = diff.get("files").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        return Some(format!("· {n} file(s) changed this turn"));
    }
    // An entry kind this build does not know: mark, never drop silently.
    Some("· (entry this CLI cannot render)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agent_core::thread::{AskQuestion, QuestionKind, TurnUsage};
    use serde_json::json;

    /// The golden lines: the exact compact strings each event class renders
    /// to. These are what scripts and eyes key on, so they are pinned.
    #[test]
    fn golden_event_lines() {
        let cases: Vec<(ThreadEvent, Option<&str>)> = vec![
            (
                ThreadEvent::UserMessage { text: "fix the bug\nplease".into(), images: vec![] },
                Some("❯ fix the bug"),
            ),
            (ThreadEvent::AssistantText("Done. It was a typo.\n".into()), Some("Done. It was a typo.")),
            (
                ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "cargo test", "timeout": 60}),
                },
                Some("→ Bash cargo test"),
            ),
            (
                ThreadEvent::ToolCallStarted {
                    id: "t2".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "/tmp/a.rs"}),
                },
                Some("→ Read /tmp/a.rs"),
            ),
            (
                ThreadEvent::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "boom\ndetail".into(),
                    is_error: true,
                    structured: None,
                },
                Some("✗ tool failed: boom"),
            ),
            (
                ThreadEvent::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                    structured: None,
                },
                None,
            ),
            (
                ThreadEvent::PermissionRequested {
                    request_id: "req-9".into(),
                    tool_use_id: None,
                    tool_name: "Bash".into(),
                    input: json!({}),
                    description: "Run cargo test".into(),
                    suggestions: vec![],
                    kind: oximux_agent_core::thread::PermissionKind::Tool,
                },
                Some("⚠ permission needed: Run cargo test (request req-9) — decide with `oximux permit`"),
            ),
            (
                ThreadEvent::QuestionAsked {
                    request_id: "req-q".into(),
                    tool_use_id: None,
                    questions: vec![AskQuestion {
                        id: "q-0".into(),
                        header: "Scope".into(),
                        question: "Which scope?".into(),
                        options: vec![],
                        kind: QuestionKind::SingleSelect,
                        other_allowed: true,
                        is_secret: false,
                    }],
                },
                Some("? question: Which scope? (request req-q) — answer with `oximux permit answer`"),
            ),
            (
                ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                Some("✓ turn ended"),
            ),
            (
                ThreadEvent::TurnEnded {
                    result: None,
                    usage: Some(TurnUsage { input_tokens: 12, output_tokens: 34, ..Default::default() }),
                    is_error: false,
                    turn_diff: None,
                },
                Some("✓ turn ended (12 in / 34 out tokens)"),
            ),
            (
                ThreadEvent::TurnEnded {
                    result: Some("model overloaded".into()),
                    usage: None,
                    is_error: true,
                    turn_diff: None,
                },
                Some("✗ turn ended with an error: model overloaded"),
            ),
            (ThreadEvent::AssistantTextDelta("par".into()), None),
            (ThreadEvent::Error("decode failed".into()), Some("✗ decode failed")),
            (ThreadEvent::Rewound { ordinal: 2 }, Some("— rewound to turn 2 —")),
        ];
        for (event, expected) in cases {
            assert_eq!(render_event(&event).as_deref(), expected, "event: {event:?}");
        }
    }

    /// Transcript entries render from their JSON shape, and an unknown entry
    /// kind degrades to a marker instead of failing or vanishing.
    #[test]
    fn golden_entry_lines() {
        assert_eq!(
            render_entry(&json!({"User": {"text": "hello", "images": []}})).as_deref(),
            Some("❯ hello")
        );
        assert_eq!(
            render_entry(&json!({"Assistant": {"text": "hi", "thinking": ""}})).as_deref(),
            Some("hi")
        );
        assert_eq!(
            render_entry(&json!({"ToolCall": {"name": "Bash", "input": {"command": "ls"}}}))
                .as_deref(),
            Some("→ Bash ls")
        );
        assert_eq!(
            render_entry(&json!({"SomeFutureEntry": {"x": 1}})).as_deref(),
            Some("· (entry this CLI cannot render)")
        );
    }

    /// Long summaries are capped and never panic on multibyte boundaries.
    #[test]
    fn one_line_caps_multibyte_safely() {
        let long = "é".repeat(200);
        let line = one_line(&long);
        assert!(line.chars().count() <= LINE_CAP + 1);
        assert!(line.ends_with('…'));
    }
}
