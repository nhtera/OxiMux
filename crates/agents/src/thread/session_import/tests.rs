//! Import fixtures: text/thinking/tool pairs/images/compaction/parallel
//! out-of-order tools, plus corrupt/foreign line tolerance.

use super::*;
use serde_json::json;

/// One JSONL line for a real string-content user prompt.
fn user_line(text: &str) -> String {
    json!({"type":"user","isSidechain":false,
        "message":{"role":"user","content":text}})
    .to_string()
}

/// An assistant line carrying arbitrary content blocks.
fn assistant_line(content: Value) -> String {
    json!({"type":"assistant","message":{"id":"msg_1","role":"assistant","content":content}})
        .to_string()
}

/// A tool_result carrier (a `user` line whose content is a tool_result block).
fn tool_result_line(id: &str, content: &str, is_error: bool) -> String {
    json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":id,"is_error":is_error,"content":content}]}})
    .to_string()
}

fn join(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn simple_user_and_assistant() {
    let raw = join(&[
        user_line("hi there"),
        assistant_line(json!([{"type":"text","text":"Hello!"}])),
    ]);
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 2);
    assert_eq!(
        t[0],
        ThreadEntry::User { text: "hi there".into(), images: vec![], checkpoint: None }
    );
    match &t[1] {
        ThreadEntry::Assistant(m) => {
            assert_eq!(m.text, "Hello!");
            assert!(m.thinking.is_empty());
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn thinking_and_text_fold_into_one_assistant() {
    let raw = assistant_line(json!([
        {"type":"thinking","thinking":"let me think","signature":"sig"},
        {"type":"text","text":"Here."},
    ]));
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 1);
    match &t[0] {
        ThreadEntry::Assistant(m) => {
            assert_eq!(m.thinking, "let me think");
            assert_eq!(m.text, "Here.");
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn tool_use_then_result_completes() {
    let raw = join(&[
        user_line("read it"),
        assistant_line(json!([
            {"type":"text","text":"On it."},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.rs"}},
        ])),
        tool_result_line("toolu_1", "file body", false),
    ]);
    let t = transcript_from_str(&raw);
    // User, Assistant, ToolCall
    assert_eq!(t.len(), 3);
    match &t[2] {
        ThreadEntry::ToolCall(tc) => {
            assert_eq!(tc.name, "Read");
            assert_eq!(tc.status, ToolCallStatus::Completed);
            assert_eq!(tc.result.as_deref(), Some("file body"));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn tool_result_error_marks_failed() {
    let raw = join(&[
        assistant_line(json!([{"type":"tool_use","id":"t1","name":"Bash","input":{}}])),
        tool_result_line("t1", "command not found", true),
    ]);
    let t = transcript_from_str(&raw);
    match &t[0] {
        ThreadEntry::ToolCall(tc) => {
            assert_eq!(tc.status, ToolCallStatus::Failed("command not found".into()));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn parallel_tools_settle_out_of_order() {
    // Two tool_use in one assistant turn; results arrive in REVERSE order.
    let raw = join(&[
        assistant_line(json!([
            {"type":"tool_use","id":"a","name":"Read","input":{}},
            {"type":"tool_use","id":"b","name":"Grep","input":{}},
        ])),
        tool_result_line("b", "grep out", false),
        tool_result_line("a", "read out", false),
    ]);
    let t = transcript_from_str(&raw);
    let tools: Vec<(&str, &ToolCallStatus, Option<&str>)> = t
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::ToolCall(tc) => Some((tc.name.as_str(), &tc.status, tc.result.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0], ("Read", &ToolCallStatus::Completed, Some("read out")));
    assert_eq!(tools[1], ("Grep", &ToolCallStatus::Completed, Some("grep out")));
}

#[test]
fn dangling_tool_use_at_eof_is_canceled() {
    // A tool call with no result (session cut mid-tool) renders as Canceled,
    // not a perpetual InProgress spinner.
    let raw = assistant_line(json!([{"type":"tool_use","id":"t1","name":"Edit","input":{}}]));
    let t = transcript_from_str(&raw);
    match &t[0] {
        ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Canceled),
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn user_image_block_imports_inline() {
    let raw = json!({"type":"user","message":{"role":"user","content":[
        {"type":"text","text":"look"},
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}})
    .to_string();
    let t = transcript_from_str(&raw);
    match &t[0] {
        ThreadEntry::User { text, images, .. } => {
            assert_eq!(text, "look");
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].media_type, "image/png");
            assert_eq!(images[0].data, "AAAA");
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn tool_result_carrier_is_not_a_user_message() {
    // A `user` line that only carries a tool_result must NOT become a User
    // bubble — it settles the tool call instead.
    let raw = join(&[
        assistant_line(json!([{"type":"tool_use","id":"t1","name":"Read","input":{}}])),
        tool_result_line("t1", "body", false),
    ]);
    let t = transcript_from_str(&raw);
    assert!(
        !t.iter().any(|e| matches!(e, ThreadEntry::User { .. })),
        "tool_result carrier must not add a User entry"
    );
}

#[test]
fn compaction_summary_becomes_divider() {
    let raw = join(&[
        user_line("q1"),
        json!({"type":"summary","summary":"Earlier: set up the parser and tests","leafUuid":"x"})
            .to_string(),
        user_line("q2"),
    ]);
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 3);
    match &t[1] {
        ThreadEntry::ContextCompaction { summary } => {
            assert_eq!(summary, "Earlier: set up the parser and tests");
        }
        other => panic!("expected ContextCompaction, got {other:?}"),
    }
}

#[test]
fn compact_summary_user_line_becomes_divider() {
    let raw = json!({"type":"user","isCompactSummary":true,
        "message":{"role":"user","content":"This session is a continuation…"}})
    .to_string();
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 1);
    assert!(matches!(&t[0], ThreadEntry::ContextCompaction { .. }));
}

#[test]
fn sidechain_and_noise_lines_are_skipped() {
    let raw = join(&[
        json!({"type":"attachment","cwd":"/x","attachment":{}}).to_string(),
        json!({"type":"queue-operation","operation":"add"}).to_string(),
        json!({"type":"mode","mode":"default"}).to_string(),
        json!({"type":"last-prompt","lastPrompt":"hi"}).to_string(),
        json!({"type":"user","isSidechain":true,
            "message":{"role":"user","content":"subagent turn"}})
        .to_string(),
        json!({"type":"assistant","isSidechain":true,
            "message":{"content":[{"type":"text","text":"subagent reply"}]}})
        .to_string(),
        user_line("real prompt"),
    ]);
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 1, "only the real prompt survives");
    assert_eq!(
        t[0],
        ThreadEntry::User { text: "real prompt".into(), images: vec![], checkpoint: None }
    );
}

#[test]
fn corrupt_and_blank_lines_never_panic() {
    let raw = format!(
        "{}\n\nnot json at all\n{{ torn\n{}",
        user_line("before"),
        assistant_line(json!([{"type":"text","text":"after"}]))
    );
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 2);
    assert!(matches!(&t[0], ThreadEntry::User { .. }));
    assert!(matches!(&t[1], ThreadEntry::Assistant(_)));
}

#[test]
fn oversized_transcript_is_capped_with_divider() {
    // Build > MAX user lines; import keeps the last MAX + a leading divider.
    let mut lines = Vec::new();
    let total = MAX_IMPORT_ENTRIES + 25;
    for i in 0..total {
        lines.push(user_line(&format!("m{i}")));
    }
    let t = transcript_from_str(&join(&lines));
    assert_eq!(t.len(), MAX_IMPORT_ENTRIES + 1, "divider + last MAX entries");
    match &t[0] {
        ThreadEntry::ContextCompaction { summary } => {
            assert!(summary.contains("25 earlier messages not shown"), "got: {summary}");
        }
        other => panic!("expected leading divider, got {other:?}"),
    }
    // The newest message is preserved at the tail.
    match t.last() {
        Some(ThreadEntry::User { text, .. }) => assert_eq!(text, &format!("m{}", total - 1)),
        other => panic!("expected last user entry, got {other:?}"),
    }
}

#[test]
fn text_then_tool_then_text_makes_two_assistants() {
    let raw = join(&[
        assistant_line(json!([{"type":"text","text":"Let me check."},
            {"type":"tool_use","id":"t1","name":"Bash","input":{}}])),
        tool_result_line("t1", "ok", false),
        assistant_line(json!([{"type":"text","text":"Done."}])),
    ]);
    let t = transcript_from_str(&raw);
    let texts: Vec<&str> = t
        .iter()
        .filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Let me check.", "Done."]);
    // order: Assistant, ToolCall, Assistant
    assert!(matches!(t[0], ThreadEntry::Assistant(_)));
    assert!(matches!(t[1], ThreadEntry::ToolCall(_)));
    assert!(matches!(t[2], ThreadEntry::Assistant(_)));
}

/// A `user` line flagged `isMeta` (IDE context, skill expansions, reminders).
fn meta_user_line(text: &str) -> String {
    json!({"type":"user","isMeta":true,"message":{"role":"user","content":text}}).to_string()
}

#[test]
fn slash_command_envelope_is_unwrapped_not_rendered_raw() {
    // The real on-disk shape: the command turn is NOT meta; the skill expansion
    // that follows IS meta. Only the clean `/research …` bubble should survive.
    let raw = join(&[
        user_line(
            "<command-message>research</command-message>\n\
             <command-name>/research</command-name>\n\
             <command-args>find the bug</command-args>",
        ),
        meta_user_line("Base directory for this skill: /x\n# Research\n..."),
        assistant_line(json!([{"type":"text","text":"On it."}])),
    ]);
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 2, "meta skill-expansion turn must be dropped");
    assert_eq!(
        t[0],
        ThreadEntry::User { text: "/research find the bug".into(), images: vec![], checkpoint: None }
    );
    assert!(matches!(t[1], ThreadEntry::Assistant(_)));
}

#[test]
fn meta_turns_are_dropped() {
    let raw = join(&[
        meta_user_line("<ide-context/>"),
        user_line("real question"),
    ]);
    let t = transcript_from_str(&raw);
    assert_eq!(t.len(), 1);
    assert_eq!(
        t[0],
        ThreadEntry::User { text: "real question".into(), images: vec![], checkpoint: None }
    );
}

#[test]
fn system_reminder_scaffolding_stripped_but_message_kept() {
    let raw = user_line("Fix the parser.\n<system-reminder>injected</system-reminder>");
    let t = transcript_from_str(&raw);
    assert_eq!(
        t[0],
        ThreadEntry::User { text: "Fix the parser.".into(), images: vec![], checkpoint: None }
    );
}

#[test]
fn a_turn_that_is_only_scaffolding_is_dropped() {
    // After stripping, nothing real remains → no empty bubble.
    let raw = user_line("<system-reminder>just plumbing</system-reminder>");
    let t = transcript_from_str(&raw);
    assert!(t.is_empty());
}

#[test]
fn image_tool_result_becomes_placeholder_and_structured_is_captured() {
    let result_line = json!({
        "type": "user",
        "toolUseResult": {"isImage": true, "stdout": ""},
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}
        ]}
    })
    .to_string();
    let raw = join(&[
        assistant_line(json!([
            {"type": "tool_use", "id": "t1", "name": "mcp__computer-use__screenshot", "input": {}}
        ])),
        result_line,
    ]);
    let t = transcript_from_str(&raw);
    let tc = t
        .iter()
        .find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("a settled tool call");
    // An image result flattens to a visible placeholder, not a blank output.
    assert_eq!(tc.result.as_deref(), Some("[image]"));
    // The structured result is captured for richer rendering.
    assert!(tc.structured.is_some());
    assert_eq!(tc.status, ToolCallStatus::Completed);
}

// ---------------------------------------------------------------------------
// tail_beyond_known_turns — the companion-terminal sync anchor
// ---------------------------------------------------------------------------

/// A minimal folded transcript: N (user, assistant) turn pairs.
fn folded_turns(n: usize) -> Vec<ThreadEntry> {
    let mut out = Vec::new();
    for i in 1..=n {
        out.extend(transcript_from_str(&join(&[
            user_line(&format!("hi {i}")),
            assistant_line(json!([{"type": "text", "text": format!("reply {i}")}])),
        ])));
    }
    out
}

#[test]
fn tail_starts_at_the_first_unseen_user_turn() {
    // Chat knows 2 turns; the log grew to 4 in the terminal → the tail is
    // turns 3+4, starting AT the user prompt (its reply rides along).
    let tail = tail_beyond_known_turns(folded_turns(4), 2);
    assert_eq!(tail.len(), 4, "two user+assistant pairs");
    match &tail[0] {
        ThreadEntry::User { text, .. } => assert_eq!(text, "hi 3"),
        other => panic!("tail must start at the unseen user turn, got {other:?}"),
    }
}

#[test]
fn a_log_with_nothing_unseen_yields_no_tail() {
    // Equal counts → nothing to append; toggling back and forth never dupes.
    assert!(tail_beyond_known_turns(folded_turns(3), 3).is_empty());
}

#[test]
fn a_capped_fold_that_knows_less_than_the_chat_yields_no_tail() {
    // MAX_IMPORT_ENTRIES elides oldest history, so a very long session's fold
    // can carry FEWER user turns than the chat has. Misalignment must fail
    // safe (skip the sync), not append the whole capped fold again.
    assert!(tail_beyond_known_turns(folded_turns(2), 5).is_empty());
}

#[test]
fn a_chat_with_no_turns_receives_the_whole_fold() {
    let tail = tail_beyond_known_turns(folded_turns(2), 0);
    assert_eq!(tail.len(), 4);
}
