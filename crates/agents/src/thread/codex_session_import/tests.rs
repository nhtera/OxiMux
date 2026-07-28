//! Tests for the Codex rollout → transcript importer.

use super::*;
use crate::thread::tool_call::ToolCallStatus;
use serde_json::json;

/// Build one rollout line (`{timestamp, type, payload}`).
fn line(ty: &str, payload: Value) -> String {
    json!({ "timestamp": "2026-07-13T00:00:00Z", "type": ty, "payload": payload }).to_string()
}

#[test]
fn maps_user_assistant_reasoning_and_tool_roundtrip() {
    let raw = [
        line("session_meta", json!({ "cwd": "/Users/x/proj", "id": "abc" })),
        line("event_msg", json!({ "type": "task_started" })), // skipped
        line("response_item", json!({ "type": "message", "role": "developer",
            "content": [{ "type": "input_text", "text": "<permissions instructions>…" }] })), // skipped
        line("response_item", json!({ "type": "message", "role": "user",
            "content": [{ "type": "input_text", "text": "fix the bug" }] })),
        line("response_item", json!({ "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": "Consider the parser" }] })),
        line("response_item", json!({ "type": "function_call", "name": "shell",
            "arguments": "{\"command\":\"cargo test\"}", "call_id": "c1" })),
        line("response_item", json!({ "type": "function_call_output", "call_id": "c1",
            "output": "ok\n2 passed" })),
        line("response_item", json!({ "type": "message", "role": "assistant",
            "content": [{ "type": "output_text", "text": "Fixed it." }] })),
    ]
    .join("\n");

    let imported = transcript_from_codex_str(&raw);
    assert_eq!(imported.cwd, Some(PathBuf::from("/Users/x/proj")));

    // developer message + event_msg dropped; the rest map in order.
    let kinds: Vec<&str> = imported
        .entries
        .iter()
        .map(|e| match e {
            ThreadEntry::User { .. } => "user",
            ThreadEntry::Assistant(m) if !m.thinking.is_empty() => "thinking",
            ThreadEntry::Assistant(_) => "assistant",
            ThreadEntry::ToolCall(_) => "tool",
            ThreadEntry::ContextCompaction { .. } => "divider",
            ThreadEntry::TurnDiff { .. } => "turn-diff",
        })
        .collect();
    assert_eq!(kinds, ["user", "thinking", "tool", "assistant"]);

    // The user + assistant text.
    match &imported.entries[0] {
        ThreadEntry::User { text, .. } => assert_eq!(text, "fix the bug"),
        other => panic!("expected user, got {other:?}"),
    }
    // The tool call settled from its output, input parsed from the JSON string.
    match &imported.entries[2] {
        ThreadEntry::ToolCall(tc) => {
            assert_eq!(tc.name, "shell");
            assert_eq!(tc.input, json!({ "command": "cargo test" }));
            assert_eq!(tc.status, ToolCallStatus::Completed);
            assert_eq!(tc.result.as_deref(), Some("ok\n2 passed"));
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn custom_tool_call_output_array_flattens_and_raw_input_wraps() {
    let raw = [
        line("response_item", json!({ "type": "custom_tool_call", "name": "exec",
            "input": "const r = await tools.exec()", "call_id": "c9" })),
        line("response_item", json!({ "type": "custom_tool_call_output", "call_id": "c9",
            "output": [
                { "type": "input_text", "text": "part one\n" },
                { "type": "input_text", "text": "part two" }
            ] })),
    ]
    .join("\n");
    let imported = transcript_from_codex_str(&raw);
    match &imported.entries[0] {
        ThreadEntry::ToolCall(tc) => {
            // A non-JSON raw input is wrapped so the card still shows it.
            assert_eq!(tc.input, json!({ "input": "const r = await tools.exec()" }));
            assert_eq!(tc.result.as_deref(), Some("part one\npart two"));
            assert_eq!(tc.status, ToolCallStatus::Completed);
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn encrypted_only_reasoning_and_unknown_items_are_skipped() {
    let raw = [
        line("response_item", json!({ "type": "reasoning", "summary": [],
            "encrypted_content": "gAAAA…ciphertext" })),
        line("response_item", json!({ "type": "some_future_item", "id": "z" })),
        line("turn_context", json!({ "cwd": "/x" })),
        line("world_state", json!({})),
        "not json at all".to_string(),
    ]
    .join("\n");
    let imported = transcript_from_codex_str(&raw);
    assert!(imported.entries.is_empty(), "no plaintext → nothing rendered, no panic");
}

#[test]
fn plumbing_user_messages_are_skipped_but_real_prompts_kept() {
    // Codex records context injections with role "user" (observed tags across
    // real rollouts). They must not become bubbles or count as user turns —
    // the companion-terminal sync anchors on the user-prompt count.
    let plumbing = [
        "<user_instructions>always be brief</user_instructions>",
        "<environment_context>cwd: /x</environment_context>",
        "<turn_aborted>user interrupted</turn_aborted>",
        "<subagent_notification>child done</subagent_notification>",
        "<recommended_plugins>list…</recommended_plugins>",
        "<skill name=\"docs\">…</skill>",
        "  <environment_context>leading whitespace</environment_context>",
    ];
    let mut lines: Vec<String> = plumbing
        .iter()
        .map(|t| {
            line("response_item", json!({ "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": t }] }))
        })
        .collect();
    lines.push(line("response_item", json!({ "type": "message", "role": "user",
        "content": [{ "type": "input_text", "text": "a real prompt, even one mentioning <skill>" }] })));
    let imported = transcript_from_codex_str(&lines.join("\n"));
    assert_eq!(imported.entries.len(), 1, "only the real prompt survives");
    match &imported.entries[0] {
        ThreadEntry::User { text, .. } => {
            assert_eq!(text, "a real prompt, even one mentioning <skill>");
        }
        other => panic!("expected user, got {other:?}"),
    }
}

#[test]
fn unsettled_tool_at_eof_is_canceled_not_spinning() {
    let raw = line("response_item", json!({ "type": "function_call", "name": "wait",
        "arguments": "{}", "call_id": "open" }));
    let imported = transcript_from_codex_str(&raw);
    match &imported.entries[0] {
        ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Canceled),
        other => panic!("expected tool call, got {other:?}"),
    }
}

#[test]
fn out_of_order_output_settles_correct_call() {
    // Two tools; the second's output arrives before the first's — the call_id map
    // settles each correctly regardless of order.
    let raw = [
        line("response_item", json!({ "type": "function_call", "name": "a", "arguments": "{}", "call_id": "A" })),
        line("response_item", json!({ "type": "function_call", "name": "b", "arguments": "{}", "call_id": "B" })),
        line("response_item", json!({ "type": "function_call_output", "call_id": "B", "output": "b-done" })),
        line("response_item", json!({ "type": "function_call_output", "call_id": "A", "output": "a-done" })),
    ]
    .join("\n");
    let imported = transcript_from_codex_str(&raw);
    let results: Vec<Option<&str>> = imported
        .entries
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::ToolCall(tc) => Some(tc.result.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(results, [Some("a-done"), Some("b-done")]);
}

#[test]
fn empty_and_all_noise_yield_empty_transcript() {
    assert!(transcript_from_codex_str("").entries.is_empty());
    assert_eq!(transcript_from_codex_str("").cwd, None);
}
