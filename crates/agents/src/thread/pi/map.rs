//! pi `AgentSessionEvent` → [`ThreadEvent`] mapping.
//!
//! Pure decode: no I/O, no gpui, so it is testable against captured wire bytes.
//! pi forwards its `AgentSessionEvent` union verbatim on stdout, so the union is
//! the wire contract; unknown event types degrade to "no events", never a panic
//! (pi is pre-1.0 and adds events on minor releases).
//!
//! ## Snapshot → delta
//!
//! `message_update` re-sends the WHOLE accumulated message on every tick, not a
//! thin delta — measured at 137.8× amplification (201 updates totalling 185KB
//! for a 1.3KB final payload). Nothing downstream can absorb that: the view's
//! coalescer only throttles repaints for events that are already `*Delta`, and
//! it drops nothing. So the diffing happens HERE, and the rest of the system
//! sees the same delta shape Claude produces.
//!
//! Two verified invariants drive the design:
//! - **Text is append-only**: `concat(deltas) == text_end.content`, so a
//!   suffix-diff is exact.
//! - **Thinking is NOT**: pi trims trailing whitespace at `thinking_end`, so the
//!   accumulated value can be LONGER than the authoritative one. That is why
//!   `*_end.content` is treated as authoritative rather than as a formality.
//!
//! ## contentIndex resets per message
//!
//! Turn 1's thinking is `contentIndex: 0`; turn 2's *text* is also
//! `contentIndex: 0`. State keyed on `contentIndex` alone collides across
//! messages, so every key here is `(message ordinal, contentIndex)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

use super::super::event::{ThreadEvent, TurnUsage};

#[cfg(test)]
use super::super::entry::ThreadEntry;
#[cfg(test)]
use super::super::state::ChatThread;

/// Mapper state across one session's event stream.
#[derive(Debug, Default)]
pub struct PiState {
    /// Bumped on each assistant `message_start`. Scopes `contentIndex`, which
    /// pi resets per message.
    msg_ordinal: u64,
    /// `(msg_ordinal, contentIndex)` → the text already emitted as deltas.
    emitted: HashMap<(u64, u64), String>,
    /// `toolCallId` → tool output already emitted. Keyed by the FULL compound id
    /// (`call_…|fc_…`), never by arrival order: pi executes tools in parallel by
    /// default, so completion order and source order differ.
    tool_out: HashMap<String, String>,
    /// The meter's denominator, seeded from the handshake (pi reports
    /// `contextWindow` per model, so it is known before any turn settles).
    ///
    /// Shared rather than owned because it is **not** fixed for the session: a
    /// live `set_model` changes it, and by a lot — gpt-5.5 is 272K while
    /// gpt-5.3-codex-spark is 128K. The connection switches models on the UI
    /// thread while this mapper runs on the reader thread, so the denominator
    /// has to cross that boundary. `0` means unknown.
    context_window: Arc<AtomicU64>,
    /// Usage from the most recent assistant message, surfaced at turn end.
    last_usage: Option<TurnUsage>,
}

impl PiState {
    pub fn with_context_window(context_window: Option<u64>) -> Self {
        Self {
            context_window: Arc::new(AtomicU64::new(context_window.unwrap_or(0))),
            ..Self::default()
        }
    }

    /// A handle the connection updates when the model changes, so the meter's
    /// denominator follows the model rather than staying at the connect-time one.
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

/// Map one pi event to zero-or-more `ThreadEvent`s.
pub fn map_event(v: &Value, st: &mut PiState) -> Vec<ThreadEvent> {
    let Some(ty) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match ty {
        "message_start" => {
            if role_of(v) == Some("assistant") {
                st.msg_ordinal += 1;
            }
            Vec::new()
        }
        "message_update" => map_message_update(v, st),
        "message_end" => {
            // Usage rides on the finalized assistant message. Emitting it live
            // keeps the composer's context meter moving during a turn.
            if role_of(v) == Some("assistant")
                && let Some(u) = usage_of(v.get("message"), st.context_window())
            {
                st.last_usage = Some(u.clone());
                return vec![ThreadEvent::LiveUsage(u)];
            }
            Vec::new()
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
                // The authoritative full output. Replaces whatever the streamed
                // chunks accumulated, so an interleaving can't corrupt it.
                content: content_text(result).unwrap_or_default(),
                is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
                structured: result.cloned(),
            }]
        }
        "agent_settled" => {
            // The reliable "turn is fully done" signal — it lands after
            // `agent_end`, and after the aborted-turn unwind.
            vec![ThreadEvent::TurnEnded {
                result: None,
                usage: st.last_usage.take(),
                is_error: false,
            }]
        }
        "compaction_start" => vec![ThreadEvent::CompactionStarted],
        "compaction_end" => {
            // `result` is ABSENT (not null) when compaction fails, so this must
            // stay an optional read. Only the failure path is verified; the
            // success shape has never been observed, so nothing is decoded from
            // it beyond its presence.
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
        // Turn/agent lifecycle carries nothing the transcript needs on its own —
        // `agent_settled` closes the turn, and content arrives via message_*.
        "agent_start" | "turn_start" | "turn_end" | "agent_end" => Vec::new(),
        _ => Vec::new(),
    }
}

fn map_message_update(v: &Value, st: &mut PiState) -> Vec<ThreadEvent> {
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
            // Diff the SNAPSHOT, not the `delta` field. Both are present and the
            // snapshot is authoritative; appending `delta` alongside it is how
            // text gets duplicated.
            let Some(full) = block_text(ae.get("partial"), ci) else { return Vec::new() };
            let seen = st.emitted.entry(key).or_default();
            let Some(suffix) = full.strip_prefix(seen.as_str()) else {
                // Not an extension of what we emitted — a rewrite. Text is
                // verified append-only, so this shouldn't happen; the `_end`
                // finalize below reconciles it either way.
                return Vec::new();
            };
            if suffix.is_empty() {
                return Vec::new();
            }
            let suffix = suffix.to_string();
            *seen = full;
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
                // The deltas already produced exactly this. Emitting a finalize
                // would REPLACE the streamed value — harmless for one block, but
                // it would clobber an earlier block in a multi-block message.
                return Vec::new();
            }
            // Authoritative, and it genuinely disagrees: pi trims trailing
            // whitespace at `thinking_end`, so the streamed value is too long.
            // The finalize replaces the streamed text (see `ChatThread::apply`),
            // which is exactly the retraction a delta cannot express.
            vec![if kind == "text_end" {
                ThreadEvent::AssistantText(content.to_string())
            } else {
                ThreadEvent::AssistantThinking(content.to_string())
            }]
        }
        // `toolcall_start`/`toolcall_delta` carry ONLY a contentIndex — the tool
        // call's id first exists at `toolcall_end`. A live args preview would
        // therefore need a synthetic id reconciled against the real one later.
        // `tool_execution_start` lands moments later with the authoritative id,
        // name and args, so the card opens there instead.
        _ => Vec::new(),
    }
}

fn map_tool_update(v: &Value, st: &mut PiState) -> Vec<ThreadEvent> {
    let Some(id) = str_at(v, "toolCallId") else { return Vec::new() };
    // `partialResult` is a structured snapshot (`{content:[…], details:{}}`), so
    // the JSON string is not prefix-stable — but the inner text is (verified
    // across 201 updates). Diff the text, never the JSON.
    let Some(full) = content_text(v.get("partialResult")) else { return Vec::new() };
    let seen = st.tool_out.entry(id.to_string()).or_default();
    let Some(chunk) = full.strip_prefix(seen.as_str()) else {
        return Vec::new();
    };
    if chunk.is_empty() {
        return Vec::new();
    }
    let chunk = chunk.to_string();
    *seen = full;
    vec![ThreadEvent::ToolOutputDelta { id: id.to_string(), chunk }]
}

/// The text of `content[ci]` in a partial message snapshot — pi names it `text`
/// on a text block and `thinking` on a thinking block.
fn block_text(partial: Option<&Value>, ci: u64) -> Option<String> {
    let block = partial?.get("content")?.as_array()?.get(ci as usize)?;
    let s = block
        .get("text")
        .or_else(|| block.get("thinking"))
        .and_then(Value::as_str)?;
    Some(s.to_string())
}

/// Flatten a pi result/partialResult's `content[]` to its text.
fn content_text(v: Option<&Value>) -> Option<String> {
    let arr = v?.get("content")?.as_array()?;
    Some(
        arr.iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    )
}

/// Usage from a finalized assistant message — everything the meter needs, on
/// every message, with no round-trip.
///
/// pi also has a `get_session_stats` command, deliberately not wired: probed
/// live, it answers `{sessionFile, sessionId, userMessages, assistantMessages,
/// toolCalls, toolResults, totalMessages, tokens, cost, contextUsage}` — and
/// every field the UI would render is already here or already known.
/// `contextUsage.contextWindow` is the handshake's window, `tokens`/`cost` are
/// these same numbers accumulated, and pi's `input` count is itself cumulative
/// (it is the whole conversation resent), so the meter is not missing a total.
/// The only genuinely new fields are message counts, which nothing displays.
fn usage_of(message: Option<&Value>, context_window: Option<u64>) -> Option<TurnUsage> {
    let u = message?.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(TurnUsage {
        input_tokens: n("input"),
        output_tokens: n("output"),
        cache_read_tokens: n("cacheRead"),
        cache_creation_tokens: n("cacheWrite"),
        context_window,
        // pi reports real dollars, unlike backends that only expose tokens.
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

    fn ev(v: Value, st: &mut PiState) -> Vec<ThreadEvent> {
        map_event(&v, st)
    }

    /// Build a `message_update` carrying a partial snapshot of one text block.
    fn text_update(kind: &str, ci: u64, full: &str, delta: &str) -> Value {
        json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": kind, "contentIndex": ci, "delta": delta,
                "partial": {"role": "assistant", "content": [{"type": "text", "text": full}]}
            }
        })
    }

    #[test]
    fn text_snapshots_become_suffix_deltas_without_duplication() {
        let mut st = PiState::default();
        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        assert!(ev(text_update("text_start", 0, "", ""), &mut st).is_empty());
        // Each snapshot re-sends the WHOLE accumulated text.
        let a = ev(text_update("text_delta", 0, "Hel", "Hel"), &mut st);
        let b = ev(text_update("text_delta", 0, "Hello", "lo"), &mut st);
        let c = ev(text_update("text_delta", 0, "Hello!", "!"), &mut st);
        assert_eq!(a, vec![ThreadEvent::AssistantTextDelta("Hel".into())]);
        assert_eq!(b, vec![ThreadEvent::AssistantTextDelta("lo".into())]);
        assert_eq!(c, vec![ThreadEvent::AssistantTextDelta("!".into())]);
        // Accumulating the deltas reproduces the text exactly once.
        let joined: String = [a, b, c]
            .concat()
            .iter()
            .map(|e| match e {
                ThreadEvent::AssistantTextDelta(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(joined, "Hello!");
    }

    #[test]
    fn emitted_text_events_are_deltas_so_the_existing_throttle_applies() {
        let mut st = PiState::default();
        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        let out = ev(text_update("text_delta", 0, "hi", "hi"), &mut st);
        assert!(out[0].is_delta(), "must ride the existing repaint throttle");
    }

    #[test]
    fn text_end_agreeing_with_the_deltas_emits_nothing() {
        // Emitting a finalize here would REPLACE the streamed text — fine for a
        // single block, but it would clobber an earlier block of a multi-block
        // message. Text is append-only, so agreement is the normal case.
        let mut st = PiState::default();
        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        ev(text_update("text_start", 0, "", ""), &mut st);
        ev(text_update("text_delta", 0, "Hello!", "Hello!"), &mut st);
        let out = ev(
            json!({"type":"message_update","assistantMessageEvent":{
                "type":"text_end","contentIndex":0,"content":"Hello!"}}),
            &mut st,
        );
        assert!(out.is_empty(), "no correction needed, got {out:?}");
    }

    #[test]
    fn thinking_end_retracts_what_the_deltas_over_reported() {
        // Verified live: pi's thinking deltas summed to 53 chars ending "\n\n"
        // while thinking_end.content was the 51-char trimmed value. A delta
        // cannot retract, so the finalize must — and `ChatThread::apply`
        // replaces the streamed value with it.
        let mut st = PiState::default();
        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        let streamed = "**Planning response structure and file inspection**\n\n";
        let trimmed = "**Planning response structure and file inspection**";
        ev(
            json!({"type":"message_update","assistantMessageEvent":{
                "type":"thinking_start","contentIndex":0,
                "partial":{"role":"assistant","content":[{"type":"thinking","thinking":""}]}}}),
            &mut st,
        );
        let d = ev(
            json!({"type":"message_update","assistantMessageEvent":{
                "type":"thinking_delta","contentIndex":0,"delta":streamed,
                "partial":{"role":"assistant","content":[{"type":"thinking","thinking":streamed}]}}}),
            &mut st,
        );
        assert_eq!(d, vec![ThreadEvent::ThinkingDelta(streamed.into())]);
        let out = ev(
            json!({"type":"message_update","assistantMessageEvent":{
                "type":"thinking_end","contentIndex":0,"content":trimmed}}),
            &mut st,
        );
        assert_eq!(
            out,
            vec![ThreadEvent::AssistantThinking(trimmed.into())],
            "the authoritative finalize must replace the over-reported stream"
        );
    }

    #[test]
    fn content_index_resets_per_message_and_must_not_collide() {
        // Turn 1's thinking is ci=0; turn 2's TEXT is also ci=0. Keying on
        // contentIndex alone makes message 2 look like a rewrite of message 1
        // and silently drops its first delta.
        let mut st = PiState::default();
        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        ev(text_update("text_start", 0, "", ""), &mut st);
        ev(text_update("text_delta", 0, "first message", "first message"), &mut st);

        ev(json!({"type":"message_start","message":{"role":"assistant"}}), &mut st);
        ev(text_update("text_start", 0, "", ""), &mut st);
        let out = ev(text_update("text_delta", 0, "second", "second"), &mut st);
        assert_eq!(
            out,
            vec![ThreadEvent::AssistantTextDelta("second".into())],
            "message 2's ci=0 must not be diffed against message 1's ci=0"
        );
    }

    #[test]
    fn parallel_tools_are_keyed_by_id_not_arrival_order() {
        // pi's default toolExecution is "parallel": both tools start before
        // either ends, and completion order need not match source order.
        let mut st = PiState::default();
        let a = "call_A|fc_A";
        let b = "call_B|fc_B";
        let s1 = ev(
            json!({"type":"tool_execution_start","toolCallId":a,"toolName":"read","args":{"path":"alpha.txt"}}),
            &mut st,
        );
        let s2 = ev(
            json!({"type":"tool_execution_start","toolCallId":b,"toolName":"read","args":{"path":"beta.txt"}}),
            &mut st,
        );
        assert_eq!(
            s1,
            vec![ThreadEvent::ToolCallStarted {
                id: a.into(),
                name: "read".into(),
                input: json!({"path": "alpha.txt"})
            }]
        );
        assert!(matches!(&s2[0], ThreadEvent::ToolCallStarted { id, .. } if id == b));
        // B finishes FIRST — the result must land on B's card.
        let e = ev(
            json!({"type":"tool_execution_end","toolCallId":b,"toolName":"read",
                   "isError":false,"result":{"content":[{"type":"text","text":"beta file contents"}]}}),
            &mut st,
        );
        assert_eq!(
            e,
            vec![ThreadEvent::ToolResult {
                tool_use_id: b.into(),
                content: "beta file contents".into(),
                is_error: false,
                structured: Some(json!({"content":[{"type":"text","text":"beta file contents"}]})),
            }]
        );
    }

    #[test]
    fn tool_output_snapshots_become_append_only_chunks() {
        // partialResult is a structured snapshot whose JSON is NOT prefix-stable
        // (the `details` key and the wrapper move), but whose inner text is.
        let mut st = PiState::default();
        let id = "call_x|fc_x";
        let up = |t: &str| {
            json!({"type":"tool_execution_update","toolCallId":id,"toolName":"bash",
                   "args":{},"partialResult":{"content":[{"type":"text","text":t}],"details":{}}})
        };
        assert_eq!(
            ev(up("line-1\n"), &mut st),
            vec![ThreadEvent::ToolOutputDelta { id: id.into(), chunk: "line-1\n".into() }]
        );
        assert_eq!(
            ev(up("line-1\nline-2\n"), &mut st),
            vec![ThreadEvent::ToolOutputDelta { id: id.into(), chunk: "line-2\n".into() }]
        );
        // A repeated snapshot with no growth emits nothing.
        assert!(ev(up("line-1\nline-2\n"), &mut st).is_empty());
        // `end.result` carries only `content` — no `details` — and is authoritative.
        let e = ev(
            json!({"type":"tool_execution_end","toolCallId":id,"toolName":"bash","isError":false,
                   "result":{"content":[{"type":"text","text":"line-1\nline-2\n"}]}}),
            &mut st,
        );
        assert!(matches!(&e[0], ThreadEvent::ToolResult { content, .. } if content == "line-1\nline-2\n"));
    }

    #[test]
    fn an_aborted_tool_reports_its_partial_output_as_an_error() {
        // Verified live: abort kills the tool and pi reports what it had, with
        // "Command aborted" appended and isError set.
        let mut st = PiState::default();
        let out = ev(
            json!({"type":"tool_execution_end","toolCallId":"call_z|fc_z","toolName":"bash","isError":true,
                   "result":{"content":[{"type":"text","text":"tick-1\ntick-2\n\n\nCommand aborted"}]}}),
            &mut st,
        );
        assert!(matches!(&out[0], ThreadEvent::ToolResult { is_error: true, content, .. }
            if content.ends_with("Command aborted")));
    }

    #[test]
    fn usage_carries_pis_real_dollar_cost_and_the_context_window() {
        let mut st = PiState::with_context_window(Some(272_000));
        let out = ev(
            json!({"type":"message_end","message":{"role":"assistant","content":[],
                   "usage":{"input":3180,"output":44,"cacheRead":0,"cacheWrite":0,"totalTokens":3224,
                            "cost":{"input":0.0159,"output":0.00132,"total":0.01722}}}}),
            &mut st,
        );
        let ThreadEvent::LiveUsage(u) = &out[0] else { panic!("expected LiveUsage, got {out:?}") };
        assert_eq!(u.input_tokens, 3180);
        assert_eq!(u.output_tokens, 44);
        assert_eq!(u.cost_usd, Some(0.01722), "pi reports real dollars");
        assert_eq!(u.context_window, Some(272_000));
        // …and it is surfaced again when the turn settles.
        let end = ev(json!({"type":"agent_settled"}), &mut st);
        assert!(matches!(&end[0], ThreadEvent::TurnEnded { usage: Some(_), .. }));
    }

    #[test]
    fn a_failed_compaction_surfaces_pis_message_and_does_not_claim_a_boundary() {
        // compaction_end OMITS `result` entirely on failure (absent, not null).
        let mut st = PiState::default();
        assert_eq!(ev(json!({"type":"compaction_start","reason":"manual"}), &mut st),
                   vec![ThreadEvent::CompactionStarted]);
        let out = ev(
            json!({"type":"compaction_end","reason":"manual","aborted":false,"willRetry":false,
                   "errorMessage":"Compaction failed: Nothing to compact (session too small)"}),
            &mut st,
        );
        assert!(matches!(&out[0], ThreadEvent::Error(m) if m.contains("Nothing to compact")));
    }

    #[test]
    fn unknown_and_withdrawn_events_are_silent_not_fatal() {
        let mut st = PiState::default();
        // pi ships new events on minor releases.
        assert!(ev(json!({"type":"some_future_event","x":1}), &mut st).is_empty());
        // Lifecycle events the transcript doesn't need on their own.
        assert!(ev(json!({"type":"agent_start"}), &mut st).is_empty());
        assert!(ev(json!({"type":"turn_start"}), &mut st).is_empty());
        // `entry_appended` fires only for extension-appended custom entries,
        // never for messages — wiring it to transcript growth would be wrong.
        assert!(ev(json!({"type":"entry_appended","entry":{"type":"message"}}), &mut st).is_empty());
        // Malformed input must not panic.
        assert!(ev(json!({"type":"message_update"}), &mut st).is_empty());
        assert!(ev(json!({}), &mut st).is_empty());
    }

    /// Replay a captured pi session, mapping every event exactly as the live
    /// worker does. Bytes are real `pi --mode rpc` stdout, not a hand-written
    /// mock — the whole point of probing the wire first.
    fn replay(fixture: &str) -> (Vec<ThreadEvent>, PiState) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/thread/testdata/");
        let raw = std::fs::read_to_string(format!("{path}{fixture}")).expect("fixture");
        let mut st = PiState::with_context_window(Some(272_000));
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect("captured line must parse");
            // Responses are correlated by the transport, never mapped.
            if v.get("type").and_then(Value::as_str) == Some("response") {
                continue;
            }
            out.extend(map_event(&v, &mut st));
        }
        (out, st)
    }

    /// Drive the mapped events through the REAL `ChatThread`, so assertions are
    /// about the transcript the user actually sees — not a re-implementation of
    /// the reconcile rules (a hand-rolled fold got this wrong by ignoring that
    /// a tool call closes the current assistant block).
    fn render(events: &[ThreadEvent]) -> ChatThread {
        let mut t = ChatThread::default();
        for e in events {
            t.apply(e);
        }
        t
    }

    /// The visible text of every assistant bubble, in order.
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
    fn replays_a_real_parallel_tool_turn_from_captured_bytes() {
        // Captured live: one assistant message carrying
        // [thinking + text + toolCall + toolCall] at contentIndex 0/1/2/3, both
        // reads executing concurrently, then a second message with the summary.
        let (events, _) = replay("pi-parallel-turn.jsonl");
        let thread = render(&events);

        // Two assistant messages — the preamble and, after the tools ran, the
        // summary — each rendered exactly once, with no duplication and no
        // run-together text.
        assert_eq!(
            assistant_texts(&thread),
            vec![
                "I’ll read both files in parallel and then summarize their contents in one sentence.",
                "alpha.txt contains “alpha file contents,” and beta.txt contains “beta file contents.”",
            ]
        );

        // Both tools opened, keyed by their full compound ids, and both resolved.
        let started: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::ToolCallStarted { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 2, "two parallel reads");
        assert!(started.iter().all(|(id, name)| name == "read" && id.contains('|')));
        let results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::ToolResult { tool_use_id, content, is_error, .. } => {
                    Some((tool_use_id.clone(), content.clone(), *is_error))
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, _, err)| !err));
        // Every result lands on a card that was actually opened.
        for (id, _, _) in &results {
            assert!(started.iter().any(|(sid, _)| sid == id), "orphan result for {id}");
        }
        let bodies: String = results.iter().map(|(_, c, _)| c.clone()).collect();
        assert!(bodies.contains("alpha file contents") && bodies.contains("beta file contents"));

        // The thinking block's trailing whitespace was retracted at _end, and the
        // retraction survives into the rendered transcript.
        let thinking: Vec<_> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::Assistant(m) if !m.thinking.is_empty() => Some(m.thinking.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking.len(), 1, "one thinking block in this capture");
        assert!(
            !thinking[0].ends_with('\n'),
            "pi's deltas over-report trailing whitespace; the finalize must retract it: {:?}",
            thinking[0]
        );

        // The turn closed exactly once, on agent_settled.
        assert_eq!(
            events.iter().filter(|e| matches!(e, ThreadEvent::TurnEnded { .. })).count(),
            1
        );
    }

    #[test]
    fn replays_201_captured_tool_snapshots_into_exact_append_only_output() {
        // The O(n^2) case: 37 tool_execution_update snapshots for one bash call.
        // Accumulating the emitted chunks must reproduce the final output
        // EXACTLY once — a suffix-diff bug shows up here as duplication.
        let (events, _) = replay("pi-chatty-bash-turn.jsonl");

        let mut streamed = String::new();
        for e in &events {
            if let ThreadEvent::ToolOutputDelta { chunk, .. } = e {
                streamed.push_str(chunk);
            }
        }
        let final_result = events
            .iter()
            .find_map(|e| match e {
                ThreadEvent::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("the bash tool produced a result");

        assert!(!streamed.is_empty(), "the snapshots must have produced deltas");
        assert_eq!(
            streamed, final_result,
            "accumulated stream must equal the authoritative result exactly — no duplication, no loss"
        );
        assert!(final_result.contains("line-1\n") && final_result.contains("line-60"));
        // Every emitted event is a delta, so the existing repaint throttle applies
        // to all 37 of them rather than forcing an immediate repaint each.
        assert!(events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::ToolOutputDelta { .. }))
            .all(|e| e.is_delta()));
    }

    #[test]
    fn an_extension_error_is_a_non_fatal_notice() {
        let mut st = PiState::default();
        let out = ev(
            json!({"type":"extension_error","extensionPath":"/x/ext.js","event":"onStart","error":"boom"}),
            &mut st,
        );
        assert!(matches!(&out[0], ThreadEvent::Error(m) if m.contains("/x/ext.js") && m.contains("boom")));
    }
}
