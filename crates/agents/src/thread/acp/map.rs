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
    AvailableCommand, AvailableCommandInput, ContentBlock, Diff, Plan, PlanEntryPriority,
    PlanEntryStatus, SessionUpdate, Terminal, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolKind,
};
use serde_json::{Value, json};

use crate::thread::entry::ChatImage;
use crate::thread::event::{PlanEntryLite, ThreadEvent};

/// Flatten one `SessionUpdate` into zero or more `ThreadEvent`s.
pub(crate) fn map_session_update(update: SessionUpdate) -> Vec<ThreadEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match message_chunk_text(&chunk.content) {
            Some(t) => vec![ThreadEvent::AssistantTextDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => match message_chunk_text(&chunk.content) {
            Some(t) => vec![ThreadEvent::ThinkingDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::ToolCall(tc) => map_tool_call(tc),
        SessionUpdate::ToolCallUpdate(tcu) => map_tool_call_update(tcu),
        SessionUpdate::Plan(plan) => vec![ThreadEvent::PlanUpdated { entries: plan_entries(&plan) }],
        SessionUpdate::AvailableCommandsUpdate(u) => vec![ThreadEvent::SlashCommandsUpdated {
            commands: u.available_commands.iter().map(|c| c.name.clone()).collect(),
            descriptions: u.available_commands.iter().map(|c| c.description.clone()).collect(),
            hints: u.available_commands.iter().map(command_hint).collect(),
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
        // The agent replaced its config options (models / reasoning + current
        // values). The worker has already swapped them into session state; emit a
        // signal so the composer re-pulls its pickers from the live connection.
        SessionUpdate::ConfigOptionUpdate(_) => vec![ThreadEvent::ControlsUpdated],
        // `UsageUpdate` is stashed by the worker (folded into `TurnEnded.usage`),
        // not mapped here. `UserMessageChunk` is the client's own prompt echoed
        // back — never re-rendered.
        _ => Vec::new(),
    }
}

/// The argument hint an ACP command advertises (`AvailableCommand.input`) as a
/// parallel string for the palette — the placeholder shown after the command name
/// (e.g. `<what to plan>`). Empty when the command takes no argument / no input
/// spec, or a future input variant we don't render.
fn command_hint(cmd: &AvailableCommand) -> String {
    match &cmd.input {
        Some(AvailableCommandInput::Unstructured(u)) => u.hint.clone(),
        _ => String::new(),
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
    // Carry the ACP tool kind (when the agent classified the call) as a follow-up
    // so the renderer can route this tool — whose `name` is a freeform human
    // title — to a rich body instead of the generic card.
    if let Some(kind) = acp_kind_wire(tc.kind) {
        out.push(ThreadEvent::ToolKind { tool_call_id: id.clone(), kind: kind.to_string() });
    }
    // An embedded terminal binds to the just-opened card (the app mounts an
    // inline `TerminalView`). Emitted regardless of status — the terminal is
    // usually live while the tool call is still in progress.
    if let Some(term) = first_terminal(&tc.content) {
        out.push(ThreadEvent::ToolTerminal {
            tool_call_id: id.clone(),
            terminal_id: term.terminal_id.0.to_string(),
        });
    }
    if is_terminal(&tc.status) {
        let images = content_images(&tc.content);
        out.push(ThreadEvent::ToolResult {
            tool_use_id: id.clone(),
            content: content_text(&tc.content),
            is_error: matches!(tc.status, ToolCallStatus::Failed),
            // A diff that arrives with the terminal shot also rides in the
            // structured slot so the card renders even for one-shot completions.
            structured: diff.map(diff_structured).or(tc.raw_output),
        });
        if !images.is_empty() {
            out.push(ThreadEvent::ToolResultImages { tool_use_id: id, images });
        }
    }
    out
}

/// A `ToolCallUpdate` at a terminal status closes the card with its result;
/// intermediate updates (still running) carry no `ThreadEvent`. A diff carried by
/// the update rides in the structured slot so the diff card fires even when the
/// edit's diff only arrives at completion (after the card was already opened).
fn map_tool_call_update(tcu: ToolCallUpdate) -> Vec<ThreadEvent> {
    let id = tcu.tool_call_id.0.to_string();
    let mut out = Vec::new();
    // A kind can arrive on an update (the agent opened the card first, then
    // classified it) — surface it so the card upgrades from the generic body.
    if let Some(kind) = tcu.fields.kind.and_then(acp_kind_wire) {
        out.push(ThreadEvent::ToolKind { tool_call_id: id.clone(), kind: kind.to_string() });
    }
    // An embedded terminal can arrive on the update that attaches it (the agent
    // opened the card first, then bound the terminal) — surface it regardless of
    // status so the app mounts the inline `TerminalView`.
    if let Some(term) = tcu.fields.content.as_deref().and_then(first_terminal) {
        out.push(ThreadEvent::ToolTerminal {
            tool_call_id: id.clone(),
            terminal_id: term.terminal_id.0.to_string(),
        });
    }
    if let Some(status) = tcu.fields.status
        && is_terminal(&status)
    {
        let structured = tcu
            .fields
            .content
            .as_deref()
            .and_then(first_diff)
            .map(diff_structured)
            .or(tcu.fields.raw_output);
        let images = tcu.fields.content.as_deref().map(content_images).unwrap_or_default();
        out.push(ThreadEvent::ToolResult {
            tool_use_id: id.clone(),
            content: tcu.fields.content.as_deref().map(content_text).unwrap_or_default(),
            is_error: matches!(status, ToolCallStatus::Failed),
            structured,
        });
        if !images.is_empty() {
            out.push(ThreadEvent::ToolResultImages { tool_use_id: id, images });
        }
    }
    out
}

/// The first file diff in a tool call's content, if any.
fn first_diff(content: &[ToolCallContent]) -> Option<&Diff> {
    content.iter().find_map(|c| match c {
        ToolCallContent::Diff(d) => Some(d),
        _ => None,
    })
}

/// The first embedded terminal in a tool call's content, if any — the agent
/// referencing a terminal it created via `terminal/create`.
fn first_terminal(content: &[ToolCallContent]) -> Option<&Terminal> {
    content.iter().find_map(|c| match c {
        ToolCallContent::Terminal(t) => Some(t),
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

/// The ACP `ToolKind` as its snake_case wire string for the tool-detail
/// classifier — or `None` for the default `Other`, `SwitchMode`, and any future
/// kind, which carry no useful body classification and would only add event
/// noise (a missing kind classifies the same as `Other`: the generic card).
fn acp_kind_wire(kind: ToolKind) -> Option<&'static str> {
    match kind {
        ToolKind::Execute => Some("execute"),
        ToolKind::Read => Some("read"),
        ToolKind::Edit => Some("edit"),
        ToolKind::Delete => Some("delete"),
        ToolKind::Move => Some("move"),
        ToolKind::Search => Some("search"),
        ToolKind::Fetch => Some("fetch"),
        ToolKind::Think => Some("think"),
        _ => None,
    }
}

/// The plain text of a `ContentBlock`, when it carries any.
fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// A displayable text rendering of a message `ContentBlock` — the richer mapping
/// used for `AgentMessageChunk`/`AgentThoughtChunk`, so an agent that streams a
/// resource link or an image isn't silently dropped. `Text` passes through;
/// `ResourceLink` becomes a Markdown link (`[name](uri)`) the chat renderer makes
/// clickable; `Image`/`Audio` become a muted placeholder (true inline image in a
/// message is a follow-up — the common image path is a tool result, handled by
/// `content_images`). Returns `None` only for a block that carries nothing worth
/// showing.
fn message_chunk_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        ContentBlock::ResourceLink(r) => {
            let uri = r.uri.clone();
            let name = if r.name.is_empty() { uri.clone() } else { r.name.clone() };
            Some(format!("[{name}]({uri})"))
        }
        ContentBlock::Image(_) => Some("[image]".to_string()),
        ContentBlock::Audio(_) => Some("[audio]".to_string()),
        // Embedded resource: surface its text when it carries any, else a link
        // placeholder. Kept defensive against the variant's inner shape.
        ContentBlock::Resource(_) => embedded_resource_text(block),
        _ => None,
    }
}

/// Best-effort text for an embedded `Resource` block via its JSON shape (the
/// inner union differs across schema revisions): a `text` field if present, else
/// a `[resource: uri]` placeholder, else nothing. Never panics on an unexpected
/// shape.
fn embedded_resource_text(block: &ContentBlock) -> Option<String> {
    let v = serde_json::to_value(block).ok()?;
    let res = v.get("resource")?;
    if let Some(t) = res.get("text").and_then(Value::as_str) {
        return Some(t.to_string());
    }
    res.get("uri").and_then(Value::as_str).map(|u| format!("[resource: {u}]"))
}

/// Extract inline base64 `image` blocks from a tool call's content as
/// [`ChatImage`]s — the ACP counterpart of the Claude tool-result image path, so
/// an ACP tool that returns an image (a screenshot tool) renders a thumbnail
/// instead of dropping the pixels. Non-image content yields an empty vec.
fn content_images(items: &[ToolCallContent]) -> Vec<ChatImage> {
    items
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Image(img) => Some(ChatImage {
                    media_type: img.mime_type.clone(),
                    data: img.data.clone(),
                }),
                _ => None,
            },
            _ => None,
        })
        .collect()
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
        AvailableCommand, AvailableCommandsUpdate, ConfigOptionUpdate, ContentChunk,
        CurrentModeUpdate, Diff, ImageContent, PlanEntry, ResourceLink, SessionConfigOption,
        SessionConfigOptionCategory, SessionConfigSelectOption, SessionInfoUpdate, TextContent,
        ToolCallUpdateFields, ToolKind, UsageUpdate,
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
    fn available_commands_map_names_with_descriptions_and_hints() {
        use agent_client_protocol::schema::v1::{AvailableCommandInput, UnstructuredCommandInput};
        let update = AvailableCommandsUpdate::new(vec![
            // A command with an argument hint (via its input spec).
            AvailableCommand::new("create_plan", "Draft a plan").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("what to plan")),
            ),
            // A command with no argument → empty hint.
            AvailableCommand::new("research", "Research the codebase"),
        ]);
        let evs = map_session_update(SessionUpdate::AvailableCommandsUpdate(update));
        assert_eq!(
            evs,
            vec![ThreadEvent::SlashCommandsUpdated {
                commands: vec!["create_plan".into(), "research".into()],
                descriptions: vec!["Draft a plan".into(), "Research the codebase".into()],
                hints: vec!["what to plan".into(), String::new()],
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
    fn config_option_update_signals_controls_refresh() {
        // An agent that repopulates its model list mid-session (e.g. after auth)
        // pushes the full option set; the worker absorbs it into state and the
        // mapper emits the single refresh signal so the composer re-pulls pickers.
        let model_select = SessionConfigOption::select(
            "model",
            "Model",
            "opus",
            vec![
                SessionConfigSelectOption::new("opus", "Claude Opus"),
                SessionConfigSelectOption::new("sonnet", "Claude Sonnet"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let update = ConfigOptionUpdate::new(vec![model_select]);
        let evs = map_session_update(SessionUpdate::ConfigOptionUpdate(update));
        assert_eq!(evs, vec![ThreadEvent::ControlsUpdated]);
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
    fn tool_result_image_emits_images_event() {
        use crate::thread::ChatImage;
        // An ACP tool that returns an image (a screenshot tool) → the tool result
        // PLUS a follow-up ToolResultImages carrying the pixels for a thumbnail.
        let tc = ToolCall::new("call-i", "Screenshot")
            .content(vec![ToolCallContent::from(ContentBlock::Image(ImageContent::new(
                "QUJD", "image/png",
            )))])
            .status(ToolCallStatus::Completed);
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert!(matches!(evs[0], ThreadEvent::ToolCallStarted { .. }));
        assert!(matches!(evs[1], ThreadEvent::ToolResult { .. }));
        match &evs[2] {
            ThreadEvent::ToolResultImages { tool_use_id, images } => {
                assert_eq!(tool_use_id, "call-i");
                assert_eq!(images, &vec![ChatImage {
                    media_type: "image/png".into(), data: "QUJD".into() }]);
            }
            other => panic!("expected ToolResultImages, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_with_embedded_terminal_emits_tool_terminal() {
        // A tool call that embeds a terminal (created via `terminal/create`)
        // opens its card AND emits `ToolTerminal` so the app mounts an inline
        // `TerminalView` bound to the client-minted id.
        let tc = ToolCall::new("call-t", "Run build")
            .content(vec![ToolCallContent::Terminal(Terminal::new("term-42"))]);
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert!(matches!(evs[0], ThreadEvent::ToolCallStarted { .. }));
        match &evs[1] {
            ThreadEvent::ToolTerminal { tool_call_id, terminal_id } => {
                assert_eq!(tool_call_id, "call-t");
                assert_eq!(terminal_id, "term-42");
            }
            other => panic!("expected ToolTerminal, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_attaching_terminal_emits_tool_terminal() {
        // Some agents open the tool call first, then attach the terminal via an
        // update while it's still running — the embed must still reach the app
        // even though the update carries no terminal status.
        let fields = ToolCallUpdateFields::new()
            .content(vec![ToolCallContent::Terminal(Terminal::new("term-9"))]);
        let tcu = ToolCallUpdate::new("call-t", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        assert_eq!(evs.len(), 1, "in-progress terminal update emits only the terminal bind");
        match &evs[0] {
            ThreadEvent::ToolTerminal { tool_call_id, terminal_id } => {
                assert_eq!(tool_call_id, "call-t");
                assert_eq!(terminal_id, "term-9");
            }
            other => panic!("expected ToolTerminal, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_with_kind_emits_tool_kind_after_start() {
        // An ACP tool with a classified kind emits `ToolKind` right after
        // `ToolCallStarted`, so the renderer can route its freeform-titled card
        // to a rich body.
        let tc = ToolCall::new("call-k", "Run the build")
            .kind(ToolKind::Execute)
            .raw_input(serde_json::json!({"command": "make"}));
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert!(matches!(evs[0], ThreadEvent::ToolCallStarted { .. }));
        match &evs[1] {
            ThreadEvent::ToolKind { tool_call_id, kind } => {
                assert_eq!(tool_call_id, "call-k");
                assert_eq!(kind, "execute");
            }
            other => panic!("expected ToolKind, got {other:?}"),
        }
    }

    #[test]
    fn default_other_kind_emits_no_tool_kind() {
        // The default `Other` kind carries no useful classification — no event
        // (keeps the generic-card behavior for unclassified ACP tools).
        let tc = ToolCall::new("call-o", "Do a thing")
            .raw_input(serde_json::json!({"x": 1}));
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert_eq!(evs.len(), 1, "only ToolCallStarted for an Other-kind tool");
        assert!(matches!(evs[0], ThreadEvent::ToolCallStarted { .. }));
    }

    #[test]
    fn tool_call_update_with_kind_emits_tool_kind() {
        // A kind that arrives on an update (agent classified the call after
        // opening it) still reaches the card.
        let fields = ToolCallUpdateFields::new().kind(ToolKind::Read);
        let tcu = ToolCallUpdate::new("call-k", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        assert_eq!(evs.len(), 1, "in-progress kind update emits only the kind");
        match &evs[0] {
            ThreadEvent::ToolKind { tool_call_id, kind } => {
                assert_eq!(tool_call_id, "call-k");
                assert_eq!(kind, "read");
            }
            other => panic!("expected ToolKind, got {other:?}"),
        }
    }

    #[test]
    fn message_chunk_resource_link_becomes_markdown_link() {
        let chunk = ContentChunk::new(ContentBlock::ResourceLink(ResourceLink::new(
            "spec.md", "file:///docs/spec.md",
        )));
        let evs = map_session_update(SessionUpdate::AgentMessageChunk(chunk));
        assert_eq!(
            evs,
            vec![ThreadEvent::AssistantTextDelta("[spec.md](file:///docs/spec.md)".into())]
        );
    }

    #[test]
    fn message_chunk_image_becomes_placeholder_not_dropped() {
        // An image streamed in a message isn't rendered inline yet, but it must
        // not silently vanish — a muted placeholder marks it.
        let chunk = ContentChunk::new(ContentBlock::Image(ImageContent::new("QUJD", "image/png")));
        let evs = map_session_update(SessionUpdate::AgentMessageChunk(chunk));
        assert_eq!(evs, vec![ThreadEvent::AssistantTextDelta("[image]".into())]);
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
