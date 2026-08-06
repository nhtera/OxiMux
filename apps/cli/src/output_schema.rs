//! `--output-schema`: hold an agent's final answer to a JSON Schema, and
//! re-prompt it with the validation errors when it misses.
//!
//! **Entirely client-side.** The loop is `turn ends` → read the transcript's
//! last assistant message → extract JSON → validate → on a miss, send the
//! errors back as a correction prompt and wait again. Nothing here needs a host
//! change, which is why this is the one automation primitive that carries no
//! protocol risk.
//!
//! **Bounded on purpose.** [`MAX_RETRIES`] is a constant, not a flag: each
//! retry is a full agent turn, and an agent that cannot produce the shape twice
//! is far more likely to be wrong about the schema than one nudge away from
//! right. Burning tokens in a loop the caller cannot see is the failure mode
//! this cap exists to prevent.

use std::path::Path;

use serde_json::Value;

use crate::cli::exit;
use crate::client::Client;
use crate::output::Failure;

/// How many corrections an agent gets after its first answer fails. Three
/// turns total, worst case.
pub const MAX_RETRIES: usize = 2;

/// How many validation errors a correction prompt carries. Beyond a handful the
/// prompt stops being guidance and starts being noise — and a schema missing in
/// twenty places is not going to be fixed by listing all twenty.
const MAX_REPORTED_ERRORS: usize = 10;

/// A compiled schema, plus the text of where it came from for error messages.
pub struct OutputSchema {
    validator: jsonschema::Validator,
    /// How the caller named it (a path, or `"inline"`), for error text.
    origin: String,
}

impl OutputSchema {
    /// Compile `spec`, which is either inline JSON (it starts with `{` or `[`)
    /// or a path to a schema file.
    ///
    /// The discriminator is the first non-space character rather than "does a
    /// file exist at this name": a typo'd path that happens not to exist would
    /// otherwise be parsed as JSON and fail with a baffling syntax error
    /// instead of "no such file".
    pub fn load(spec: &str) -> Result<Self, Failure> {
        let trimmed = spec.trim_start();
        let (origin, text) = if trimmed.starts_with('{') || trimmed.starts_with('[') {
            ("inline".to_string(), spec.to_string())
        } else {
            let path = Path::new(spec);
            let text = std::fs::read_to_string(path).map_err(|e| {
                Failure::new(
                    "schema",
                    exit::USAGE,
                    format!("cannot read the output schema {}: {e}", path.display()),
                )
                .with_steps([
                    "pass a readable file path, or the schema itself as inline JSON".into(),
                ])
            })?;
            (path.display().to_string(), text)
        };

        let schema: Value = serde_json::from_str(&text).map_err(|e| {
            Failure::new("schema", exit::USAGE, format!("{origin} is not valid JSON: {e}"))
        })?;
        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            Failure::new("schema", exit::USAGE, format!("{origin} is not a valid JSON Schema: {e}"))
        })?;
        Ok(Self { validator, origin })
    }

    /// Validate one value, returning every error as `<json-pointer>: <message>`
    /// (the pointer omitted at the root, where it would be an empty string).
    pub fn check(&self, value: &Value) -> Result<(), Vec<String>> {
        let errors: Vec<String> = self
            .validator
            .iter_errors(value)
            .take(MAX_REPORTED_ERRORS)
            .map(|e| {
                let at = e.instance_path.to_string();
                if at.is_empty() { e.to_string() } else { format!("{at}: {e}") }
            })
            .collect();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// Pull a JSON value out of an agent's prose.
///
/// Three shapes, most-trusted first: the whole message is JSON; the message
/// contains a fenced code block; the message has a bare `{…}`/`[…]` span in it.
/// The fence is tried before the bare span because an agent that fenced its
/// answer told us exactly where it is, whereas brace-scanning can catch a JSON
/// example the agent was *discussing*.
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    for block in fenced_blocks(trimmed) {
        if let Ok(value) = serde_json::from_str::<Value>(block.trim()) {
            return Some(value);
        }
    }
    balanced_span(trimmed).and_then(|span| serde_json::from_str::<Value>(span).ok())
}

/// Every ```-fenced block's body, in order. The info string (```json) is
/// dropped; a block whose body is not JSON is simply skipped by the caller.
fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        // Past the fence and its info string, to the start of the body.
        let after_fence = &rest[open + 3..];
        let Some(body_start) = after_fence.find('\n') else { break };
        let body = &after_fence[body_start + 1..];
        let Some(close) = body.find("```") else { break };
        blocks.push(&body[..close]);
        rest = &body[close + 3..];
    }
    blocks
}

/// The first brace- or bracket-balanced span, ignoring delimiters inside
/// strings (and their escapes) so a `"}"` in a value cannot close the object
/// early.
fn balanced_span(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match *b {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The correction sent back to the agent. Deliberately prescriptive — an agent
/// told only "that was wrong" tends to apologize rather than re-answer.
pub fn correction_prompt(errors: &[String], had_json: bool) -> String {
    let mut prompt = String::from(
        "Your previous answer did not satisfy the required output schema.\n\n",
    );
    if had_json {
        prompt.push_str("Validation errors:\n");
        for err in errors {
            prompt.push_str("- ");
            prompt.push_str(err);
            prompt.push('\n');
        }
    } else {
        prompt.push_str("No JSON object could be found in your answer.\n");
    }
    prompt.push_str(
        "\nReply with ONLY the corrected JSON — no prose before or after it, no code fence.",
    );
    prompt
}

/// The last non-empty assistant message in a folded transcript, or `None` when
/// the agent has not spoken.
pub fn last_assistant_text(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .rev()
        .filter_map(|entry| entry.get("Assistant")?.get("text")?.as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_string)
}

/// One validate-and-correct pass over a session that has just finished a turn.
///
/// Returns the schema-valid value, or a failure carrying the *last* validation
/// error — the one the caller's script has to act on. Streaming of the
/// correction turns goes through the same renderer as the original, so a human
/// watching sees the retries happen rather than a silent pause.
/// `turn_timeout` is the caller's `--turn-timeout`, carried through so it bounds
/// each correction turn too. Applying it only to the first turn would let a
/// bounded `run` hang here instead — the retries are ordinary turns and can park
/// on a permission request exactly as the first one can.
pub async fn enforce(
    client: &Client,
    session: &str,
    schema: &OutputSchema,
    turn_timeout: Option<u64>,
    json_mode: bool,
) -> Result<Value, Failure> {
    let mut attempt = 0usize;
    loop {
        let entries = crate::commands::transcript::fetch_entries(client, session).await?.entries;
        let answer = last_assistant_text(&entries);
        let parsed = answer.as_deref().and_then(extract_json);

        let errors = match &parsed {
            Some(value) => match schema.check(value) {
                Ok(()) => return Ok(value.clone()),
                Err(errors) => errors,
            },
            None => Vec::new(),
        };

        if attempt >= MAX_RETRIES {
            let detail = if parsed.is_none() {
                "the agent's final answer contained no JSON".to_string()
            } else {
                format!("the agent's final answer does not match {}: {}", schema.origin(), errors.join("; "))
            };
            return Err(Failure::new("schema-mismatch", exit::ERROR, detail).with_steps([
                format!("{} correction attempts were made", MAX_RETRIES),
                format!("inspect the answer with `oximux transcript {session}`"),
            ]));
        }
        attempt += 1;

        if !json_mode {
            println!("· output did not match the schema — correction {attempt}/{MAX_RETRIES}");
        }
        let prompt = correction_prompt(&errors, parsed.is_some());
        // The correction is an ordinary turn: send it and stream to its end,
        // exactly as `send` does, so a mid-turn queue or a permission prompt
        // behaves the same way it would for a human's follow-up.
        crate::commands::send::run(client, session, &prompt, false, turn_timeout, None, json_mode)
            .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> OutputSchema {
        OutputSchema::load(
            r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#,
        )
        .expect("compile")
    }

    #[test]
    fn a_bare_json_answer_validates() {
        let value = extract_json(r#"{"ok": true}"#).expect("extracted");
        assert!(schema().check(&value).is_ok());
    }

    /// The common shape: the agent fences its JSON and adds prose around it.
    #[test]
    fn a_fenced_answer_is_extracted() {
        let text = "Here you go:\n\n```json\n{\"ok\": false}\n```\n\nLet me know!";
        assert_eq!(extract_json(text), Some(json!({"ok": false})));
    }

    /// A fence with no info string still works — agents fence both ways.
    #[test]
    fn an_unlabelled_fence_is_extracted() {
        assert_eq!(extract_json("```\n{\"ok\": true}\n```"), Some(json!({"ok": true})));
    }

    /// Bare braces in prose, with a `}` inside a string value that must not
    /// close the object early.
    #[test]
    fn a_bare_span_survives_braces_inside_strings() {
        let text = r#"The result is {"ok": true, "note": "closes with }"} — done."#;
        assert_eq!(extract_json(text), Some(json!({"ok": true, "note": "closes with }"})));
    }

    #[test]
    fn prose_with_no_json_extracts_nothing() {
        assert_eq!(extract_json("I could not determine an answer."), None);
    }

    /// The failure path a caller scripts against: errors name the offending
    /// pointer so a correction prompt is actionable.
    #[test]
    fn a_mismatch_reports_the_failing_path() {
        let errors = schema().check(&json!({"ok": "yes"})).expect_err("invalid");
        assert!(errors.iter().any(|e| e.starts_with("/ok")), "errors: {errors:?}");
    }

    #[test]
    fn a_missing_required_field_is_reported() {
        let errors = schema().check(&json!({})).expect_err("invalid");
        assert!(errors.iter().any(|e| e.contains("ok")), "errors: {errors:?}");
    }

    /// The last assistant message wins, and empty ones are skipped — a fold
    /// ends with an empty assistant entry whenever the turn produced only tool
    /// calls.
    #[test]
    fn the_last_nonempty_assistant_message_is_the_answer() {
        let entries = vec![
            json!({"Assistant": {"text": "first"}}),
            json!({"ToolCall": {"name": "Bash"}}),
            json!({"Assistant": {"text": "second"}}),
            json!({"Assistant": {"text": "   "}}),
        ];
        assert_eq!(last_assistant_text(&entries).as_deref(), Some("second"));
    }

    #[test]
    fn a_transcript_with_no_assistant_text_has_no_answer() {
        assert!(last_assistant_text(&[json!({"User": {"text": "hi"}})]).is_none());
    }

    /// A path that does not exist is a usage error naming the file, never a
    /// JSON syntax error about its contents.
    #[test]
    fn a_missing_schema_file_is_a_usage_error() {
        let Err(err) = OutputSchema::load("/nonexistent/schema.json") else {
            panic!("a missing file must not compile");
        };
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("schema.json"), "{}", err.message);
    }

    #[test]
    fn a_malformed_schema_is_a_usage_error() {
        let Err(err) = OutputSchema::load(r#"{"type": 7}"#) else {
            panic!("`type: 7` is not a schema");
        };
        assert_eq!(err.exit, exit::USAGE);
    }

    /// The correction names every error and demands bare JSON back.
    #[test]
    fn the_correction_prompt_carries_the_errors() {
        let prompt = correction_prompt(&["/ok: not a boolean".into()], true);
        assert!(prompt.contains("/ok: not a boolean"));
        assert!(prompt.contains("ONLY the corrected JSON"));
    }

    #[test]
    fn the_correction_prompt_says_so_when_no_json_was_found() {
        let prompt = correction_prompt(&[], false);
        assert!(prompt.contains("No JSON object"));
    }
}
