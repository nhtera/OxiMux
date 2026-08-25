//! omp `AgentSessionEvent` → [`ThreadEvent`] mapping.
//!
//! Pure decode, mirrored from the Pi mapper — omp kept Pi's event taxonomy
//! verbatim (re-verified live on 18.0.4: `message_update` still re-sends full
//! snapshots alongside granular `assistantMessageEvent`s, `tool_execution_*`
//! still keys on the compound `toolCallId`). The snapshot→delta diffing rides
//! the shared [`snapshot_diff`] helpers so the two mappers cannot drift.
//!
//! Two deliberate divergences from the Pi mapper, both probe-verified:
//!
//! - **`agent_end` closes the turn, not `agent_settled`** — rpc-ui never
//!   emits `agent_settled` (measured: zero occurrences across every captured
//!   turn). And not every `agent_end` counts: omp can end an internal
//!   extension-notice cycle before the model turn for the same prompt starts,
//!   so an `agent_end` only closes the turn when its `messages` carry an
//!   assistant message or one streamed live this turn (the reference
//!   integration applies the same rule).
//! - **Error turns are well-formed, not silent** — a failed provider call
//!   still streams `message_start/end` with `stopReason:"error"` and an
//!   `errorMessage` (probed live via a 403), so the mapper surfaces that
//!   message and marks the turn's end as an error.
//!
//! [`snapshot_diff`]: super::super::snapshot_diff

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

use super::super::event::{ThreadEvent, TurnUsage};
use super::super::snapshot_diff::{block_text, content_text, suffix_delta};

#[cfg(test)]
use super::super::entry::ThreadEntry;
#[cfg(test)]
use super::super::state::ChatThread;

/// Mapper state across one session's event stream.
#[derive(Debug, Default)]
pub struct OmpState {
    /// Bumped on each assistant `message_start`. Scopes `contentIndex`, which
    /// resets per message (Pi-family behavior).
    msg_ordinal: u64,
    /// `(msg_ordinal, contentIndex)` → the text already emitted as deltas.
    emitted: HashMap<(u64, u64), String>,
    /// `toolCallId` → tool output already emitted. Keyed by the FULL compound
    /// id (`call_…|fc_…`) — tools run in parallel, so arrival order lies.
    tool_out: HashMap<String, String>,
    /// The meter's denominator, shared with the connection so a live
    /// `set_model` moves it.
    context_window: Arc<AtomicU64>,
    /// Usage from the most recent assistant message, surfaced at turn end.
    last_usage: Option<TurnUsage>,
    /// Whether an assistant message streamed since the last turn close —
    /// the flag that tells a real `agent_end` from an internal cycle's.
    saw_assistant: bool,
    /// Whether this turn carried a provider error (`stopReason:"error"`).
    turn_errored: bool,
}

impl OmpState {
    pub fn with_context_window(context_window: Option<u64>) -> Self {
        Self {
            context_window: Arc::new(AtomicU64::new(context_window.unwrap_or(0))),
            ..Self::default()
        }
    }

    /// A handle the connection updates when the model changes.
    pub fn context_window_handle(&self) -> Arc<AtomicU64> {
        self.context_window.clone()
    }

    fn context_window(&self) -> Option<u64> {
        match self.context_window.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }
}

/// Map one omp event to zero-or-more `ThreadEvent`s.
pub fn map_event(v: &Value, st: &mut OmpState) -> Vec<ThreadEvent> {
    let Some(ty) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match ty {
        "message_start" => {
            if role_of(v) == Some("assistant") {
                st.msg_ordinal += 1;
                st.saw_assistant = true;
            }
            Vec::new()
        }
        "message_update" => map_message_update(v, st),
        "message_end" => {
            if role_of(v) != Some("assistant") {
                return Vec::new();
            }
            let mut out = Vec::new();
            // A provider failure ends the message with `stopReason:"error"`
            // and a readable `errorMessage` — surface it, or the user sees an
            // empty bubble and a turn that just stops.
            if let Some(msg) = v
                .get("message")
                .and_then(|m| m.get("errorMessage"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                st.turn_errored = true;
                out.push(ThreadEvent::Error(msg.to_string()));
            }
            // Usage rides the finalized assistant message; emitting it live
            // keeps the context meter moving during a turn.
            if let Some(u) = usage_of(v.get("message"), st.context_window()) {
                st.last_usage = Some(u.clone());
                out.push(ThreadEvent::LiveUsage(u));
            }
            out
        }
        "tool_execution_start" => {
            let (Some(id), Some(name)) = (str_at(v, "toolCallId"), str_at(v, "toolName")) else {
                return Vec::new();
            };
            st.tool_out.remove(id);
            vec![ThreadEvent::ToolCallStarted {
                id: id.to_string(),
                name: name.to_string(),
                input: v.get("args").cloned().unwrap_or(Value::Null),
            }]
        }
        "tool_execution_update" => map_tool_update(v, st),
        "tool_execution_end" => {
            let Some(id) = str_at(v, "toolCallId") else { return Vec::new() };
            st.tool_out.remove(id);
            let result = v.get("result");
            vec![ThreadEvent::ToolResult {
                tool_use_id: id.to_string(),
                // The authoritative full output — replaces the streamed chunks.
                content: content_text(result).unwrap_or_default(),
                is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
                structured: result.cloned(),
            }]
        }
        "agent_end" => {
            // Only a cycle that produced (or streamed) an assistant message is
            // a turn; omp ends internal extension cycles through the same
            // event, and closing on those would flip the composer idle while
            // the real turn is still coming.
            let has_assistant = v
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|ms| {
                    ms.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
                });
            if !has_assistant && !st.saw_assistant {
                return Vec::new();
            }
            st.saw_assistant = false;
            let is_error = std::mem::take(&mut st.turn_errored);
            vec![ThreadEvent::TurnEnded {
                result: None,
                usage: st.last_usage.take(),
                is_error,
                turn_diff: None,
            }]
        }
        "compaction_start" | "auto_compaction_start" => vec![ThreadEvent::CompactionStarted],
        "compaction_end" | "auto_compaction_end" => {
            if let Some(msg) = str_at(v, "errorMessage") {
                return vec![ThreadEvent::Error(msg.to_string())];
            }
            vec![ThreadEvent::CompactBoundary { summary: String::new() }]
        }
        "extension_error" => {
            let path = str_at(v, "extensionPath").unwrap_or("extension");
            let err = str_at(v, "error").unwrap_or("unknown error");
            vec![ThreadEvent::Error(format!("{path}: {err}"))]
        }
        // An error-level notice is omp telling the user something went wrong
        // out-of-band; info/warn notices are extension mount chatter
        // (measured: MCP mount notices on every connect) — logged, not shown.
        "notice" => {
            let level = str_at(v, "level").unwrap_or("info");
            let message = str_at(v, "message").unwrap_or_default();
            if level == "error" && !message.is_empty() {
                return vec![ThreadEvent::Error(message.to_string())];
            }
            tracing::debug!(level, message, "omp notice");
            Vec::new()
        }
        // Lifecycle carries nothing the transcript needs on its own; content
        // arrives via message_* and the turn closes on agent_end above.
        "agent_start" | "turn_start" | "turn_end" => Vec::new(),
        // Subagent activity arrives EXCLUSIVELY as wrapped frames
        // (`subagent_lifecycle`/`subagent_progress`/`subagent_event`, inner
        // events inside `payload` — omp `rpc-subagents.ts:211/238/245`), so a
        // subagent can never masquerade as the top-level agent's
        // `agent_end`/`message_start` and corrupt turn state. Dropped here
        // until a later round renders them; the locking test below keeps the
        // no-contamination property honest.
        "subagent_lifecycle" | "subagent_progress" | "subagent_event" => Vec::new(),
        _ => Vec::new(),
    }
}

fn map_message_update(v: &Value, st: &mut OmpState) -> Vec<ThreadEvent> {
    let Some(ae) = v.get("assistantMessageEvent") else { return Vec::new() };
    let Some(kind) = ae.get("type").and_then(Value::as_str) else { return Vec::new() };
    let Some(ci) = ae.get("contentIndex").and_then(Value::as_u64) else { return Vec::new() };
    let key = (st.msg_ordinal, ci);

    match kind {
        "text_start" | "thinking_start" => {
            st.emitted.insert(key, String::new());
            Vec::new()
        }
        "text_delta" | "thinking_delta" => {
            // Diff the SNAPSHOT, not the `delta` field — the snapshot is
            // authoritative, and appending `delta` alongside it duplicates
            // text (Pi lesson, same wire shape).
            let Some(full) = block_text(ae.get("partial"), ci) else { return Vec::new() };
            let seen = st.emitted.entry(key).or_default();
            let Some(suffix) = suffix_delta(seen, full) else { return Vec::new() };
            vec![if kind == "text_delta" {
                ThreadEvent::AssistantTextDelta(suffix)
            } else {
                ThreadEvent::ThinkingDelta(suffix)
            }]
        }
        "text_end" | "thinking_end" => {
            let content = ae.get("content").and_then(Value::as_str).unwrap_or_default();
            let seen = st.emitted.remove(&key).unwrap_or_default();
            if seen == content {
                // The deltas already produced exactly this; a finalize would
                // clobber an earlier block of a multi-block message.
                return Vec::new();
            }
            vec![if kind == "text_end" {
                ThreadEvent::AssistantText(content.to_string())
            } else {
                ThreadEvent::AssistantThinking(content.to_string())
            }]
        }
        // toolcall_* carry only a contentIndex; `tool_execution_start` lands
        // moments later with the authoritative id/name/args (and on omp it
        // additionally carries `intent`), so the card opens there.
        _ => Vec::new(),
    }
}

fn map_tool_update(v: &Value, st: &mut OmpState) -> Vec<ThreadEvent> {
    let Some(id) = str_at(v, "toolCallId") else { return Vec::new() };
    // `partialResult` is a structured snapshot; only its inner text is
    // prefix-stable. Diff the text, never the JSON.
    let Some(full) = content_text(v.get("partialResult")) else { return Vec::new() };
    let seen = st.tool_out.entry(id.to_string()).or_default();
    let Some(chunk) = suffix_delta(seen, full) else { return Vec::new() };
    vec![ThreadEvent::ToolOutputDelta { id: id.to_string(), chunk }]
}

/// Usage from a finalized assistant message — same field names as Pi's
/// (verified live), including real dollars under `cost.total`.
fn usage_of(message: Option<&Value>, context_window: Option<u64>) -> Option<TurnUsage> {
    let u = message?.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(TurnUsage {
        input_tokens: n("input"),
        output_tokens: n("output"),
        cache_read_tokens: n("cacheRead"),
        cache_creation_tokens: n("cacheWrite"),
        context_window,
        cost_usd: u.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64),
    })
}

fn role_of(v: &Value) -> Option<&str> {
    v.get("message")?.get("role")?.as_str()
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Replay a captured wire fixture (event frames from a live omp 18.0.4
    /// session, probe 01; opaque signature blobs scrubbed, structure and text
    /// verbatim) through the mapper.
    fn replay(fixture: &str) -> (Vec<ThreadEvent>, OmpState) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/thread/testdata/");
        let raw = std::fs::read_to_string(format!("{path}{fixture}")).expect("fixture");
        let mut st = OmpState::with_context_window(Some(272_000));
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect("captured line must parse");
            out.extend(map_event(&v, &mut st));
        }
        (out, st)
    }

    /// Drive the mapped events through the REAL `ChatThread` (never a
    /// hand-rolled fold — Pi lesson).
    fn render(events: &[ThreadEvent]) -> ChatThread {
        let mut t = ChatThread::default();
        for e in events {
            t.apply(e);
        }
        t
    }

    fn assistant_texts(t: &ChatThread) -> Vec<String> {
        t.entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::Assistant(m) if !m.text.is_empty() => Some(m.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replays_a_real_simple_turn_from_captured_bytes() {
        let (events, _) = replay("omp-simple-turn.jsonl");
        let thread = render(&events);
        assert_eq!(assistant_texts(&thread), vec!["DONE"]);

        // The turn closed exactly once, on agent_end, carrying the usage the
        // assistant message reported (live numbers from the capture).
        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::TurnEnded { usage, is_error, .. } => Some((usage.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1);
        let (usage, is_error) = &ends[0];
        assert!(!is_error);
        let u = usage.as_ref().expect("usage reached the turn end");
        assert_eq!(u.input_tokens, 36164);
        assert_eq!(u.output_tokens, 5);
        assert!(u.cost_usd.unwrap() > 0.0, "omp reports real dollars");
    }

    #[test]
    fn replays_a_real_denied_approval_turn_from_captured_bytes() {
        // Captured live under `--approval-mode always-ask` with Deny answered:
        // thinking, a bash tool card, the denial, and the model's follow-up.
        let (events, _) = replay("omp-approval-deny-turn.jsonl");
        let thread = render(&events);

        // The tool card opened on the compound id and resolved as an error
        // carrying omp's denial text.
        let started: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::ToolCallStarted { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].1, "bash");
        assert!(started[0].0.contains('|'), "compound call_…|fc_… id, whole string");
        let denied = events.iter().any(|e| matches!(
            e,
            ThreadEvent::ToolResult { is_error: true, content, .. }
                if content.contains("denied by user")
        ));
        assert!(denied, "the denial must surface on the tool card");

        // The model's post-denial reply renders, and the turn closes cleanly
        // (a denied tool is not a provider error).
        let texts = assistant_texts(&thread);
        assert!(
            texts.iter().any(|t| t.contains("denied")),
            "the follow-up mentioning the denial must render, got {texts:?}"
        );
        let ends: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::TurnEnded { .. }))
            .collect();
        assert_eq!(ends.len(), 1, "one turn close, on the real agent_end");
        assert!(matches!(ends[0], ThreadEvent::TurnEnded { is_error: false, .. }));
    }

    #[test]
    fn an_internal_agent_end_without_an_assistant_message_closes_nothing() {
        // omp ends internal extension-notice cycles through agent_end too;
        // closing the turn there would flip the composer idle while the real
        // turn is still starting (reference-integration rule).
        let mut st = OmpState::default();
        let out = map_event(&json!({"type": "agent_end", "messages": [], "isTerminal": true}), &mut st);
        assert!(out.is_empty());
        // But an agent_end whose payload carries the assistant message closes,
        // even if the stream itself was missed.
        let out = map_event(
            &json!({"type": "agent_end", "messages": [{"role": "assistant", "content": []}]}),
            &mut st,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_provider_error_turn_surfaces_the_message_and_marks_the_end() {
        // The live 403 shape from probe 01: a well-formed assistant message
        // with stopReason error and a readable errorMessage.
        let mut st = OmpState::default();
        map_event(&json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        let out = map_event(
            &json!({"type":"message_end","message":{
                "role":"assistant","content":[],"stopReason":"error",
                "errorMessage":"Bedrock HTTP 403: The security token included in the request is invalid.",
                "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.0}}
            }}),
            &mut st,
        );
        assert!(
            out.iter().any(|e| matches!(e, ThreadEvent::Error(m) if m.contains("403"))),
            "the provider error must be visible"
        );
        let out = map_event(&json!({"type":"agent_end","messages":[{"role":"assistant"}]}), &mut st);
        assert!(matches!(out[0], ThreadEvent::TurnEnded { is_error: true, .. }));
    }

    #[test]
    fn error_notices_surface_and_info_notices_stay_quiet() {
        let mut st = OmpState::default();
        let quiet = map_event(
            &json!({"type":"notice","level":"info","message":"xd://: mounted mcp__x","source":"xdev"}),
            &mut st,
        );
        assert!(quiet.is_empty(), "mount chatter is not transcript content");
        let loud = map_event(
            &json!({"type":"notice","level":"error","message":"extension exploded"}),
            &mut st,
        );
        assert!(matches!(&loud[..], [ThreadEvent::Error(m)] if m == "extension exploded"));
    }

    #[test]
    fn subagent_frames_never_touch_the_parent_turns_state() {
        // Subagent activity rides wrapped frame types whose payloads embed the
        // inner events — if one ever mapped as a top-level event, a subagent's
        // agent_end could close the parent's turn or its message_start could
        // cross-contaminate the ordinal-keyed delta state.
        let mut st = OmpState::default();
        map_event(&json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        for ty in ["subagent_lifecycle", "subagent_progress", "subagent_event"] {
            let wrapped = json!({"type": ty, "payload": {
                // Worst case: the payload embeds a whole agent_end.
                "type": "agent_end", "messages": [{"role": "assistant"}]
            }});
            assert!(map_event(&wrapped, &mut st).is_empty(), "{ty} must map to nothing");
        }
        // The parent's turn still closes normally afterwards — state intact.
        let out = map_event(&json!({"type":"agent_end","messages":[{"role":"assistant"}]}), &mut st);
        assert!(matches!(&out[..], [ThreadEvent::TurnEnded { .. }]));
    }

    #[test]
    fn emitted_text_events_are_deltas_so_the_existing_throttle_applies() {
        let mut st = OmpState::default();
        map_event(&json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        let out = map_event(
            &json!({"type":"message_update","assistantMessageEvent":{
                "type":"text_delta","contentIndex":0,"delta":"hi",
                "partial":{"role":"assistant","content":[{"type":"text","text":"hi"}]}
            }}),
            &mut st,
        );
        assert!(out[0].is_delta(), "must ride the existing repaint throttle");
    }
}
