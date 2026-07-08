//! Maps ACP `SessionUpdate` notifications → the transport-agnostic
//! [`ThreadEvent`] vocabulary the `ChatThread`/UI consume.
//!
//! Covered: assistant message + thought chunks (streamed as deltas — the ACP
//! chunks are authoritative, so the `ChatThread` builds the message from them and
//! seals it on `TurnEnded`; no separate finalized block is emitted, which would
//! clobber text interleaved with tool cards), and tool calls (`ToolCall` → a tool
//! card; `ToolCallUpdate` at a terminal status → its result). ACP `ToolKind`/
//! `title` don't line up with Claude's tool names, so cards render via the
//! generic path rather than a Bash/Read/Edit-specific renderer — a deliberate
//! fallback, not a leak of raw JSON.
//!
//! Also mapped: `Plan` (→ the pinned plan checklist), `AvailableCommandsUpdate`
//! (→ the slash palette), `CurrentModeUpdate` (→ the mode picker), and
//! `SessionInfoUpdate.title` (→ the tab label). `UsageUpdate` is NOT mapped here —
//! the worker stashes it and folds it into the next `TurnEnded.usage` (ACP
//! delivers usage out-of-band, not per-turn).

use agent_client_protocol::schema::v1::{
    ContentBlock, Diff, Plan, PlanEntryPriority, PlanEntryStatus, SessionUpdate, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate,
};
use serde_json::{Value, json};

use crate::thread::event::{PlanEntryLite, ThreadEvent};

/// Flatten one `SessionUpdate` into zero or more `ThreadEvent`s.
pub(crate) fn map_session_update(update: SessionUpdate) -> Vec<ThreadEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => vec![ThreadEvent::AssistantTextDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => vec![ThreadEvent::ThinkingDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::ToolCall(tc) => map_tool_call(tc),
        SessionUpdate::ToolCallUpdate(tcu) => map_tool_call_update(tcu),
        SessionUpdate::Plan(plan) => vec![ThreadEvent::PlanUpdated { entries: plan_entries(&plan) }],
        SessionUpdate::AvailableCommandsUpdate(u) => vec![ThreadEvent::SlashCommandsUpdated {
            commands: u.available_commands.iter().map(|c| c.name.clone()).collect(),
        }],
        SessionUpdate::CurrentModeUpdate(u) => {
            vec![ThreadEvent::ModeChanged { mode_id: u.current_mode_id.0.to_string() }]
        }
        // Title is `MaybeUndefined`: emit only when the agent actually set a
        // non-null title (a `null` clears it upstream — we simply don't override).
        SessionUpdate::SessionInfoUpdate(u) => match u.title.value() {
            Some(title) => vec![ThreadEvent::TitleUpdated { title: title.clone() }],
            None => Vec::new(),
        },
        // `UsageUpdate` is stashed by the worker (folded into `TurnEnded.usage`),
        // not mapped here. `UserMessageChunk` is the client's own prompt echoed
        // back — never re-rendered. `ConfigOptionUpdate` has no UI consumer yet.
        _ => Vec::new(),
    }
}

/// Map ACP `PlanEntry`s into the gpui-free `PlanEntryLite` the plan panel renders.
/// Status/priority become the same wire strings the `TodoWrite` plan path uses, so
/// both feed one renderer.
fn plan_entries(plan: &Plan) -> Vec<PlanEntryLite> {
    plan.entries
        .iter()
        .map(|e| PlanEntryLite {
            content: e.content.clone(),
            status: plan_status_wire(&e.status).to_string(),
            priority: plan_priority_wire(&e.priority).to_string(),
        })
        .collect()
}

/// ACP `PlanEntryStatus` → the `TodoWrite`-compatible status wire string. Unknown
/// future variants degrade to `pending` (the renderer's own fallback).
fn plan_status_wire(status: &PlanEntryStatus) -> &'static str {
    match status {
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

/// ACP `PlanEntryPriority` → a wire string. Unknown future variants → `medium`.
fn plan_priority_wire(priority: &PlanEntryPriority) -> &'static str {
    match priority {
        PlanEntryPriority::High => "high",
        PlanEntryPriority::Low => "low",
        _ => "medium",
    }
}

/// A `ToolCall` opens a tool card; when it already carries a terminal status it
/// also emits the result in the same batch (some agents send one shot).
///
/// A file edit (`ToolCallContent::Diff`) is normalized into the rich diff card's
/// shape: the tool name becomes `Edit`/`Write` and the input carries a
/// `__acp_diff__` payload, so the shared Myers diff card fires instead of the
/// flattened-text fallback. Every other ACP tool keeps its agent-authored title
/// and raw input and renders via the generic key:value card (its agent-specific
/// input keys don't line up with the Bash/Read/etc. bodies, and the title is more
/// descriptive than a forced rename — deliberate fallback, not a raw-JSON leak).
fn map_tool_call(tc: ToolCall) -> Vec<ThreadEvent> {
    let id = tc.tool_call_id.0.to_string();
    let diff = first_diff(&tc.content);
    let (name, input) = match diff {
        Some(d) => (diff_tool_name(d).to_string(), diff_input(d)),
        None => (tc.title.clone(), tc.raw_input.clone().unwrap_or(Value::Null)),
    };
    let mut out = vec![ThreadEvent::ToolCallStarted { id: id.clone(), name, input }];
    if is_terminal(&tc.status) {
        out.push(ThreadEvent::ToolResult {
            tool_use_id: id,
            content: content_text(&tc.content),
            is_error: matches!(tc.status, ToolCallStatus::Failed),
            // A diff that arrives with the terminal shot also rides in the
            // structured slot so the card renders even for one-shot completions.
            structured: diff.map(diff_structured).or(tc.raw_output),
        });
    }
    out
}

/// A `ToolCallUpdate` at a terminal status closes the card with its result;
/// intermediate updates (still running) carry no `ThreadEvent`. A diff carried by
/// the update rides in the structured slot so the diff card fires even when the
/// edit's diff only arrives at completion (after the card was already opened).
fn map_tool_call_update(tcu: ToolCallUpdate) -> Vec<ThreadEvent> {
    match tcu.fields.status {
        Some(status) if is_terminal(&status) => {
            let structured = tcu
                .fields
                .content
                .as_deref()
                .and_then(first_diff)
                .map(diff_structured)
                .or(tcu.fields.raw_output);
            vec![ThreadEvent::ToolResult {
                tool_use_id: tcu.tool_call_id.0.to_string(),
                content: tcu.fields.content.as_deref().map(content_text).unwrap_or_default(),
                is_error: matches!(status, ToolCallStatus::Failed),
                structured,
            }]
        }
        _ => Vec::new(),
    }
}

/// The first file diff in a tool call's content, if any.
fn first_diff(content: &[ToolCallContent]) -> Option<&Diff> {
    content.iter().find_map(|c| match c {
        ToolCallContent::Diff(d) => Some(d),
        _ => None,
    })
}

/// A diff with no prior content is a whole-file write (all-additions); one with
/// prior content is an edit. The name drives the card's glyph + the "replaces the
/// file" hint.
fn diff_tool_name(d: &Diff) -> &'static str {
    if d.old_text.as_deref().unwrap_or("").is_empty() { "Write" } else { "Edit" }
}

/// The normalized diff payload the diff card reads (`{path, old_text, new_text}`),
/// with `file_path` also lifted to the top level so the card header shows the
/// target path.
fn diff_input(d: &Diff) -> Value {
    json!({ "file_path": d.path.display().to_string(), "__acp_diff__": diff_body(d) })
}

/// The same payload wrapped for the structured (result) slot.
fn diff_structured(d: &Diff) -> Value {
    json!({ "__acp_diff__": diff_body(d) })
}

fn diff_body(d: &Diff) -> Value {
    json!({ "path": d.path.display().to_string(), "old_text": d.old_text, "new_text": d.new_text })
}

/// `Completed`/`Failed` are terminal; `Pending`/`InProgress` are not.
fn is_terminal(status: &ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}

/// The plain text of a `ContentBlock`, when it carries any.
fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// Flatten a tool call's content items into a legible result body: text blocks
/// verbatim. File diffs are skipped here — they render as the rich diff card
/// (normalized into `__acp_diff__`), so flattening them into the result text
/// would duplicate them. Terminal output is skipped.
fn content_text(items: &[ToolCallContent]) -> String {
    let mut out = String::new();
    for item in items {
        if let ToolCallContent::Content(c) = item
            && let Some(t) = text_of(&c.content)
        {
            out.push_str(&t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AvailableCommand, AvailableCommandsUpdate, ContentChunk, CurrentModeUpdate, Diff,
        PlanEntry, SessionInfoUpdate, TextContent, ToolCallUpdateFields, UsageUpdate,
    };

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s.to_string())))
    }

    #[test]
    fn agent_message_chunk_streams_as_delta() {
        let evs = map_session_update(SessionUpdate::AgentMessageChunk(text_chunk("Hello")));
        assert_eq!(evs, vec![ThreadEvent::AssistantTextDelta("Hello".into())]);
    }

    #[test]
    fn agent_thought_chunk_maps_to_thinking() {
        let evs = map_session_update(SessionUpdate::AgentThoughtChunk(text_chunk("hm")));
        assert_eq!(evs, vec![ThreadEvent::ThinkingDelta("hm".into())]);
    }

    #[test]
    fn tool_call_opens_a_card() {
        let tc = ToolCall::new("call-1", "Read src/main.rs")
            .raw_input(serde_json::json!({"path": "src/main.rs"}));
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ThreadEvent::ToolCallStarted { id, name, input } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "Read src/main.rs");
                assert_eq!(input["path"], "src/main.rs");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_completed_emits_result() {
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
                "file contents".to_string(),
            )))]);
        let tcu = ToolCallUpdate::new("call-1", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ThreadEvent::ToolResult { tool_use_id, content, is_error, .. } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_in_progress_is_silent() {
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let tcu = ToolCallUpdate::new("call-1", fields);
        assert!(map_session_update(SessionUpdate::ToolCallUpdate(tcu)).is_empty());
    }

    #[test]
    fn tool_call_update_failed_marks_error() {
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let tcu = ToolCallUpdate::new("call-9", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        match &evs[0] {
            ThreadEvent::ToolResult { is_error, .. } => assert!(is_error),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn plan_maps_to_plan_updated_with_status_and_priority() {
        let plan = Plan::new(vec![
            PlanEntry::new("design", PlanEntryPriority::High, PlanEntryStatus::InProgress),
            PlanEntry::new("build", PlanEntryPriority::Medium, PlanEntryStatus::Pending),
            PlanEntry::new("ship", PlanEntryPriority::Low, PlanEntryStatus::Completed),
        ]);
        let evs = map_session_update(SessionUpdate::Plan(plan));
        match &evs[0] {
            ThreadEvent::PlanUpdated { entries } => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].content, "design");
                assert_eq!(entries[0].status, "in_progress");
                assert_eq!(entries[0].priority, "high");
                assert_eq!(entries[1].status, "pending");
                assert_eq!(entries[2].status, "completed");
                assert_eq!(entries[2].priority, "low");
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
    }

    #[test]
    fn available_commands_map_to_names_only() {
        let update = AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("create_plan", "Draft a plan"),
            AvailableCommand::new("research", "Research the codebase"),
        ]);
        let evs = map_session_update(SessionUpdate::AvailableCommandsUpdate(update));
        assert_eq!(
            evs,
            vec![ThreadEvent::SlashCommandsUpdated {
                commands: vec!["create_plan".into(), "research".into()],
            }]
        );
    }

    #[test]
    fn current_mode_maps_to_mode_changed() {
        let evs = map_session_update(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            "acceptEdits",
        )));
        assert_eq!(evs, vec![ThreadEvent::ModeChanged { mode_id: "acceptEdits".into() }]);
    }

    #[test]
    fn session_info_with_title_maps_to_title_updated() {
        let info = SessionInfoUpdate::new().title("Refactor auth".to_string());
        let evs = map_session_update(SessionUpdate::SessionInfoUpdate(info));
        assert_eq!(evs, vec![ThreadEvent::TitleUpdated { title: "Refactor auth".into() }]);
    }

    #[test]
    fn session_info_without_title_is_silent() {
        // A metadata update that doesn't set a title (e.g. only `updated_at`) must
        // not clobber the tab label — no event.
        let info = SessionInfoUpdate::new().updated_at("2026-07-08T00:00:00Z".to_string());
        assert!(map_session_update(SessionUpdate::SessionInfoUpdate(info)).is_empty());
    }

    #[test]
    fn usage_update_is_not_mapped_here() {
        // Usage is stashed by the worker + folded into `TurnEnded`, so the mapper
        // emits nothing for it (no standalone usage event).
        let evs = map_session_update(SessionUpdate::UsageUpdate(UsageUpdate::new(1000, 200_000)));
        assert!(evs.is_empty());
    }

    #[test]
    fn edit_diff_normalizes_to_the_diff_card_shape() {
        let diff = Diff::new("src/main.rs", "new line").old_text("old line".to_string());
        let tc = ToolCall::new("call-e", "Modify src/main.rs")
            .content(vec![ToolCallContent::Diff(diff)])
            .status(ToolCallStatus::Completed);
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        // Start: name → Edit (has prior content), input carries the normalized diff
        // + a top-level file_path for the card header.
        match &evs[0] {
            ThreadEvent::ToolCallStarted { name, input, .. } => {
                assert_eq!(name, "Edit");
                assert_eq!(input["file_path"], "src/main.rs");
                assert_eq!(input["__acp_diff__"]["old_text"], "old line");
                assert_eq!(input["__acp_diff__"]["new_text"], "new line");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
        // Terminal shot: the diff also rides the structured slot, and the result
        // TEXT does NOT duplicate the diff (it renders as a card).
        match &evs[1] {
            ThreadEvent::ToolResult { content, structured, .. } => {
                assert!(content.is_empty(), "diff is not flattened into result text");
                assert_eq!(structured.as_ref().unwrap()["__acp_diff__"]["new_text"], "new line");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn new_file_diff_normalizes_to_write() {
        // A diff with no prior content is a whole-file write (all additions).
        let tc = ToolCall::new("call-w", "Create notes.md")
            .content(vec![ToolCallContent::Diff(Diff::new("notes.md", "hello"))]);
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        match &evs[0] {
            ThreadEvent::ToolCallStarted { name, input, .. } => {
                assert_eq!(name, "Write");
                assert_eq!(input["__acp_diff__"]["new_text"], "hello");
                assert!(input["__acp_diff__"]["old_text"].is_null());
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn diff_arriving_in_an_update_rides_the_structured_slot() {
        // Some agents open the tool call first, then send the diff at completion —
        // it must still reach the diff card (via structured).
        let diff = Diff::new("a.rs", "b").old_text("a".to_string());
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Diff(diff)]);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("call-e", fields)));
        match &evs[0] {
            ThreadEvent::ToolResult { structured, .. } => {
                assert_eq!(structured.as_ref().unwrap()["__acp_diff__"]["new_text"], "b");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn non_edit_tool_keeps_title_and_raw_input() {
        // A non-diff tool stays on its agent-authored title + raw input (the
        // generic card renders it) — no forced rename.
        let tc = ToolCall::new("call-x", "Run tests")
            .raw_input(serde_json::json!({"cmd": "cargo test"}));
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        match &evs[0] {
            ThreadEvent::ToolCallStarted { name, input, .. } => {
                assert_eq!(name, "Run tests");
                assert_eq!(input["cmd"], "cargo test");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }
}
