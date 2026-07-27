//! How a computer-use call reads once it is over.
//!
//! "Computer use" is the user-facing name throughout — the settings pane, these
//! cards, the run summary. Internal identifiers still say `screen_*`, which is
//! the older spelling and is left alone deliberately: renaming `ScreenControl`
//! and friends would touch every phase's code for no reader's benefit, and the
//! crate underneath has always been `oximux-computer-use` anyway.
//!
//! By default these render through the generic MCP path:
//! `oximux-computer-use · type_text` with a line of raw JSON under it, thirty
//! times in a row. That is a log, not a record — and this is the one tool family
//! where the transcript *is* the audit trail, because the actions happened in
//! windows the user was not looking at while the agent drove them.
//!
//! Everything here is presentation, and deliberately so: by the time any of it
//! runs the policy has already decided, the gate has already refused what it was
//! going to refuse, and nothing below can change what happened. It is all
//! strings, which is also what makes it testable without a window.
//!
//! # Why the app is named from a memo rather than resolved here
//!
//! Turning a pid into an app name is cheap, and it is only *correct* while that
//! process is alive. A transcript reloaded the next day holds pids that have
//! since been recycled, and resolving one then would confidently name the wrong
//! app — in an audit trail that is worse than naming none. So the name is
//! whatever this chat resolved when the call was *decided*, held by
//! [`ScreenControl`](super::computer_use::ScreenControl) and handed here through
//! [`ScreenContext`](super::screen_consent::ScreenContext). A call with nothing
//! remembered says `process 4321` rather than guessing, which is also what a
//! restored transcript shows — the memo is deliberately not persisted.

use oximux_agents::thread::{ToolCall, ToolCallStatus};
use serde_json::Value;

use super::bubble::elide;

/// Is this one of the screen-control server's tools?
///
/// Delegates rather than matching a prefix of its own, and that is the whole
/// reason it exists as a function. The same predicate decides whether a call
/// renders as a screen action here and whether its images are stripped before
/// they can reach a paired phone ([`scrub_transcript`]). Two copies would let a
/// rename make the transcript keep labelling a call as screen control while the
/// redactor quietly stopped recognising it — which would look like a cosmetic
/// regression and be an egress.
///
/// [`scrub_transcript`]: oximux_computer_use::scrub_transcript
pub(super) fn is_screen_call(name: &str) -> bool {
    oximux_computer_use::is_computer_use_tool(name)
}

/// How long a typed string may run in the collapsed header before it elides.
const HEADER_ARG_CHARS: usize = 40;

/// Tools whose humanized name would read as something milder than what they do.
///
/// Everything else is humanized from the bare tool name, on purpose: a driver
/// release that adds a tool then reads as itself rather than as a verb we
/// invented for behaviour we have not seen.
const VERBS: &[(&str, &str)] = &[
    ("type_text", "Type"),
    ("hotkey", "Key chord"),
    // Whole-display, and "get state" does not say that a picture was taken.
    ("get_desktop_state", "Capture desktop"),
    // Metadata only — app names, window bounds, z-order. No pixels, so this one
    // must NOT read as a capture.
    ("get_accessibility_tree", "Read elements"),
];

/// The header label for a computer-use call — `Computer use · Type`. `None` for
/// every other tool, which keeps its own label untouched.
///
/// Keeps the `<server> · <tool>` shape every other MCP card uses, swapping the
/// raw server id (`oximux-computer-use ·`) — which names OxiMux's own plumbing —
/// for the term the settings pane and the wider industry both use. Deliberately
/// not "Screen ·": OxiMux's Remote feature genuinely is controlling this screen
/// from elsewhere, and one vocabulary for two unrelated things is how a user
/// ends up believing their phone is driving these clicks.
pub(super) fn display_name(tc: &ToolCall) -> Option<String> {
    let bare = oximux_computer_use::bare_tool_name(&tc.name)?;
    Some(format!("Computer use · {}", verb(bare, &tc.input)))
}

fn verb(bare: &str, input: &Value) -> String {
    // `get_window_state` returns the element tree *and* a screenshot, and
    // `include_screenshot` defaults to true — so it is a capture unless the call
    // says otherwise. Read from the input rather than the table because both
    // spellings are real, and a label that claimed a picture was taken when none
    // was is the wrong kind of wrong for a record of what an agent did.
    if bare == "get_window_state" {
        return match input.get("include_screenshot") {
            Some(Value::Bool(false)) => "Read window".to_string(),
            _ => "Capture window".to_string(),
        };
    }
    if let Some((_, verb)) = VERBS.iter().find(|(tool, _)| *tool == bare) {
        return (*verb).to_string();
    }
    let mut words = bare.replace('_', " ");
    if let Some(first) = words.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    words
}

/// What the header says after the verb: the argument that was delivered and the
/// app it went to, as `"hello" → Figma`. `None` when the call carries neither,
/// which is the shape of the metadata reads (`list_apps`, `get_screen_size`).
///
/// `app` is what this chat already knew about the target pid; see the module
/// note on why it is not resolved here.
pub(super) fn target(tc: &ToolCall, app: Option<&str>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(arg) = argument(&tc.input) {
        parts.push(arg);
    }
    if let Some(addressee) = addressee(&tc.input, app) {
        parts.push(addressee);
    }
    (!parts.is_empty()).then(|| parts.join(" → "))
}

/// The payload a call delivers, in the shapes the driver's input tools use.
///
/// Unrecognized shapes return `None` rather than a guess — the full input is
/// rendered as key:value in the card body either way, so nothing is lost by
/// this being conservative.
fn argument(input: &Value) -> Option<String> {
    if let Some(text) = input.get("text").and_then(Value::as_str) {
        return Some(format!("{:?}", elide(text, HEADER_ARG_CHARS)));
    }
    if let Some(value) = input.get("value").and_then(Value::as_str) {
        return Some(format!("{:?}", elide(value, HEADER_ARG_CHARS)));
    }
    // A chord arrives as either a list of keys or one already-joined string.
    match input.get("keys") {
        Some(Value::Array(keys)) => {
            let chord: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
            if !chord.is_empty() {
                return Some(chord.join("+"));
            }
        }
        Some(Value::String(chord)) => return Some(chord.clone()),
        _ => {}
    }
    for key in ["key", "bundle_id", "app"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

/// Who the call went to: the app when this chat knows its name, the bare pid
/// when it does not, and nothing at all when the call names no process.
///
/// The pid comes from the policy's own accessor rather than a second read of the
/// field, so the header can never name a target the policy did not decide about.
fn addressee(input: &Value, app: Option<&str>) -> Option<String> {
    let pid = oximux_computer_use::policy::addressed_pid(input)?;
    Some(match app {
        Some(app) => app.to_string(),
        None => format!("process {pid}"),
    })
}

/// The one-line verdict for a settled call, from the driver's own reply.
///
/// Reads three fields measured against the shipping driver and passes over
/// anything else, which the raw result below it still shows. `verified` is the
/// one that has to be spelled out: a `press_key` reports `verified: false`
/// having delivered the keystroke perfectly well — it means the accessibility
/// layer could not read the change back, not that nothing happened. Left as a
/// bare `false` in a JSON dump it reads as failure, and an agent that believes
/// its keystroke was dropped escalates to the foreground mode the policy
/// refuses.
pub(super) fn outcome(tc: &ToolCall) -> Option<String> {
    let result = serde_json::from_str::<Value>(tc.result.as_deref()?).ok()?;
    let mut parts: Vec<String> = Vec::new();
    match result.get("effect").and_then(Value::as_str) {
        Some("confirmed") => parts.push("delivered".to_string()),
        Some("unverifiable") => parts.push(
            "delivered — the app did not report the change back, which is normal for key events \
             and does not mean it was dropped"
                .to_string(),
        ),
        Some(other) => parts.push(other.to_string()),
        None => {}
    }
    if let (Some(sent), Some(asked)) = (
        result.get("delivered_chars").and_then(Value::as_u64),
        result.get("characters").and_then(Value::as_u64),
    ) && sent != asked
    {
        // Only when they disagree. Equal counts are the ordinary case and say
        // nothing the verdict above has not already said.
        parts.push(format!("{sent} of {asked} characters"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The refusal on a screen call that did not run, or `None`.
///
/// Every reason the policy produces is already a sentence written for the user
/// — that is [`Refusal::reason`]'s whole job. What was missing was that a
/// settled card shows only its glyph until it is expanded, so the sentence
/// explaining why an agent stopped driving sat one click out of sight. The
/// caller renders this without waiting to be asked.
///
/// The two refusal paths land in different places, and both have to be read:
///
/// - The **gate** refuses out of process, so the CLI reports an errored tool
///   result and the reason arrives as the failure itself.
/// - The **in-process policy** refuses on the permission round-trip, where the
///   status is `Rejected` and carries nothing; the reason is recorded on the
///   result by [`set_tool_refusal`].
///
/// A `Rejected` with no result is the third case — the user clicked the button,
/// and needs no explanation of their own decision.
///
/// [`Refusal::reason`]: oximux_computer_use::tools::Refusal::reason
/// [`set_tool_refusal`]: oximux_agents::thread::ChatThread::set_tool_refusal
pub(super) fn refusal(tc: &ToolCall) -> Option<&str> {
    let reason = match &tc.status {
        ToolCallStatus::Failed(reason) => reason.as_str(),
        ToolCallStatus::Rejected => tc.result.as_deref()?,
        _ => return None,
    };
    Some(reason.trim()).filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns(tool: &str) -> String {
        format!("mcp__oximux-computer-use__{tool}")
    }

    fn call(tool: &str, input: Value) -> ToolCall {
        ToolCall::new("t", ns(tool), input)
    }

    /// The predicate has to be the redactor's, not a lookalike. A transcript
    /// that renders a call as screen control while `scrub_transcript` no longer
    /// recognises it is a screenshot on its way to a phone.
    #[test]
    fn the_renderer_and_the_redactor_agree_on_what_a_screen_call_is() {
        for tool in ["click", "type_text", "get_window_state"] {
            assert!(is_screen_call(&ns(tool)), "{tool}");
            assert!(oximux_computer_use::is_computer_use_tool(&ns(tool)), "{tool}");
        }
        for other in ["Bash", "Read", "mcp__computer-use__left_click", "mcp__github__issue"] {
            assert!(!is_screen_call(other), "{other}");
            assert!(!oximux_computer_use::is_computer_use_tool(other), "{other}");
        }
    }

    fn label(tool: &str) -> Option<String> {
        display_name(&call(tool, json!({})))
    }

    #[test]
    fn the_label_names_the_action_rather_than_the_server() {
        assert_eq!(label("click").as_deref(), Some("Computer use · Click"));
        assert_eq!(label("type_text").as_deref(), Some("Computer use · Type"));
        assert_eq!(label("right_click").as_deref(), Some("Computer use · Right click"));
        assert_eq!(label("list_windows").as_deref(), Some("Computer use · List windows"));
        // Every other tool keeps whatever label it already had.
        assert_eq!(display_name(&ToolCall::new("t", "Bash", json!({}))), None);
        assert_eq!(
            display_name(&ToolCall::new("t", "mcp__github__create_issue", json!({}))),
            None
        );
    }

    /// A tool the driver adds later must read as itself. Inventing a verb for
    /// behaviour nobody has seen is how a card ends up describing the wrong
    /// thing confidently.
    #[test]
    fn an_unknown_tool_reads_as_its_own_name() {
        assert_eq!(label("frobnicate_widget").as_deref(), Some("Computer use · Frobnicate widget"));
    }

    /// "Get window state" does not tell anyone a screenshot was taken, and it
    /// always is one by default. But `include_screenshot` can be turned off, and
    /// a label claiming a picture was taken when none was is the wrong kind of
    /// wrong for a record of what an agent did — so the verb reads the call.
    #[test]
    fn a_capture_says_it_captured_and_a_metadata_read_does_not() {
        assert_eq!(label("get_window_state").as_deref(), Some("Computer use · Capture window"));
        assert_eq!(
            display_name(&call("get_window_state", json!({"include_screenshot": false}))).as_deref(),
            Some("Computer use · Read window")
        );
        assert_eq!(label("get_desktop_state").as_deref(), Some("Computer use · Capture desktop"));
        // Metadata only — no pixels, so this one must not read as a capture.
        assert_eq!(label("get_accessibility_tree").as_deref(), Some("Computer use · Read elements"));
    }

    #[test]
    fn the_header_carries_what_was_delivered_and_where() {
        let typed = call("type_text", json!({ "pid": 4321, "text": "hello" }));
        assert_eq!(target(&typed, Some("Figma")).as_deref(), Some("\"hello\" → Figma"));
        // A chord joins; a lone key passes through.
        let chord = call("hotkey", json!({ "pid": 7, "keys": ["cmd", "s"] }));
        assert_eq!(target(&chord, Some("Notes")).as_deref(), Some("cmd+s → Notes"));
        let key = call("press_key", json!({ "pid": 7, "key": "Return" }));
        assert_eq!(target(&key, Some("Notes")).as_deref(), Some("Return → Notes"));
        // A click names only its target.
        let click = call("click", json!({ "pid": 4321 }));
        assert_eq!(target(&click, Some("Figma")).as_deref(), Some("Figma"));
        // A metadata read names nothing, and must not invent a target.
        assert_eq!(target(&call("list_apps", json!({})), None), None);
    }

    /// The fallback that keeps the trail honest. A reloaded transcript has no
    /// memo, and naming the app from a recycled pid would be a confident lie.
    #[test]
    fn an_unremembered_target_reads_as_its_pid() {
        let click = call("click", json!({ "pid": 4321 }));
        assert_eq!(target(&click, None).as_deref(), Some("process 4321"));
    }

    #[test]
    fn a_long_typed_string_elides_in_the_header() {
        let long = "x".repeat(200);
        let typed = call("type_text", json!({ "pid": 1, "text": long }));
        let header = target(&typed, None).expect("a header");
        assert!(header.chars().count() < 80, "{header}");
        assert!(header.contains('…'), "{header}");
    }

    /// The wire shapes are from the coverage spike, byte for byte.
    #[test]
    fn a_confirmed_delivery_reads_as_delivered() {
        let mut tc = call("type_text", json!({ "pid": 1, "text": "hello" }));
        tc.result = Some(
            json!({"characters": 19, "delivered_chars": 19, "effect": "confirmed",
                   "path": "ax", "verified": true})
            .to_string(),
        );
        assert_eq!(outcome(&tc).as_deref(), Some("delivered"));
    }

    /// The one that matters. `verified: false` from `press_key` means the
    /// accessibility layer could not read the change back — the keystroke was
    /// delivered. Shown raw it reads as a failure, and an agent that believes
    /// its input was dropped escalates to a delivery mode the policy refuses.
    #[test]
    fn an_unverifiable_delivery_does_not_read_as_a_failure() {
        let mut tc = call("press_key", json!({ "pid": 1, "key": "Return" }));
        tc.result =
            Some(json!({"effect": "unverifiable", "path": "key_events", "verified": false}).to_string());
        let line = outcome(&tc).expect("a verdict");
        assert!(line.starts_with("delivered"), "{line}");
        assert!(line.contains("does not mean it was dropped"), "{line}");
    }

    #[test]
    fn a_short_delivery_reports_the_shortfall() {
        let mut tc = call("type_text", json!({ "pid": 1, "text": "hello" }));
        tc.result = Some(
            json!({"characters": 19, "delivered_chars": 4, "effect": "confirmed"}).to_string(),
        );
        assert_eq!(outcome(&tc).as_deref(), Some("delivered · 4 of 19 characters"));
    }

    #[test]
    fn a_result_this_module_cannot_read_produces_no_verdict() {
        // Not an error — the raw result still renders below. Saying nothing is
        // how an unrecognized reply avoids being described wrongly.
        let mut tc = call("click", json!({ "pid": 1 }));
        tc.result = Some("clicked the button".into());
        assert_eq!(outcome(&tc), None);
        tc.result = Some(json!({"something": "new"}).to_string());
        assert_eq!(outcome(&tc), None);
        tc.result = None;
        assert_eq!(outcome(&tc), None);
    }

    /// The gate refuses out of process, so the reason arrives as the errored
    /// tool result.
    #[test]
    fn a_refusal_from_the_gate_surfaces_its_own_sentence() {
        let mut tc = call("type_text", json!({ "text": "hello" }));
        tc.status = ToolCallStatus::Failed(
            "`type_text` did not name a target process, so it would act on whatever window is in front"
                .into(),
        );
        assert_eq!(refusal(&tc), Some(tc_reason(&tc)));
        // A settled or running call has nothing to explain.
        let mut ok = call("click", json!({ "pid": 1 }));
        ok.status = ToolCallStatus::Completed;
        assert_eq!(refusal(&ok), None);
    }

    /// The in-process path settles as `Rejected`, whose status carries no
    /// message at all. Without reading the result, every refusal decided on the
    /// permission round-trip would show a glyph and nothing else — the agent
    /// would be told what was wrong and the user would not.
    #[test]
    fn a_refusal_from_the_policy_surfaces_the_reason_recorded_on_the_card() {
        let mut tc = call("click", json!({ "pid": 4321 }));
        tc.status = ToolCallStatus::Rejected;
        tc.result = Some("`click` targeted process 4321, which another chat is driving".into());
        assert_eq!(
            refusal(&tc),
            Some("`click` targeted process 4321, which another chat is driving")
        );
    }

    /// The third case, and the one that must stay quiet: the user clicked
    /// Reject. Explaining their own decision back to them is noise.
    #[test]
    fn a_refusal_the_user_clicked_explains_nothing() {
        let mut tc = call("click", json!({ "pid": 4321 }));
        tc.status = ToolCallStatus::Rejected;
        assert_eq!(refusal(&tc), None);
    }

    fn tc_reason(tc: &ToolCall) -> &str {
        match &tc.status {
            ToolCallStatus::Failed(reason) => reason,
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
