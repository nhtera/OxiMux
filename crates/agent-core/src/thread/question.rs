//! AskUserQuestion model + answer serialization for the Agent Chat thread.
//!
//! Claude's `AskUserQuestion` tool arrives over the stream-json control channel
//! as a `can_use_tool` request whose input carries a `questions` array. Unlike a
//! permission prompt (Allow/Reject), the user *picks answers*, which travel back
//! as a control_response `allow` whose `updatedInput` carries an `answers` map
//! keyed by question TEXT (plus an optional overall free-text `response` that
//! supersedes the map). This module is the pure (gpui-free) model for that flow.
//!
//! Draft answers are keyed internally by a stable per-question id (`q-{idx}`) so
//! the UI never has to use long/duplicate question text as a map key; the id is
//! remapped to the question text only at the wire boundary in
//! [`updated_input_json`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One selectable choice in a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// Single- vs multi-select. An explicit enum (not a bool) so a future MCP/ACP
/// elicitation adapter can grow Text/Number/Boolean kinds without reshaping the
/// card or this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestionKind {
    SingleSelect,
    MultiSelect,
}

/// One clarifying question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskQuestion {
    /// Stable id for keying draft answers in the UI. Claude synthesizes it as
    /// `q-{idx}` (remapped to `question` text at the wire boundary); Codex
    /// supplies its own native question id here and the answer keys by it
    /// directly. Either way it's an opaque map key — nothing parses the format.
    pub id: String,
    /// Short label chip (the tool caps it at ≤12 chars).
    pub header: String,
    /// Full question text — also the wire key for this question's answer.
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub kind: QuestionKind,
    /// Whether a free-text "Other" answer is offered. Always `true` for
    /// AskUserQuestion; a field so a future adapter can disable it.
    pub other_allowed: bool,
    /// The backend flagged this question's answer as sensitive (Codex
    /// `isSecret`) — a secret/credential the UI should treat with care (shown
    /// with a "sensitive" hint; full input-masking + no-persist is a follow-up).
    /// Always `false` for Claude AskUserQuestion. `#[serde(default)]` keeps older
    /// persisted transcript blobs loadable.
    #[serde(default)]
    pub is_secret: bool,
}

impl AskQuestion {
    pub fn multi_select(&self) -> bool {
        matches!(self.kind, QuestionKind::MultiSelect)
    }

    /// The wire shape the tool expects back in `updatedInput.questions` — the
    /// documented fields only (`question`, `header`, `options`, `multiSelect`).
    fn to_wire(&self) -> Value {
        json!({
            "question": self.question,
            "header": self.header,
            "options": self
                .options
                .iter()
                .map(|o| json!({"label": o.label, "description": o.description}))
                .collect::<Vec<_>>(),
            "multiSelect": self.multi_select(),
        })
    }
}

/// A pending AskUserQuestion request: the `request_id` to answer on plus the
/// parsed questions. Serde-friendly so it can ride a persisted transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub request_id: String,
    pub questions: Vec<AskQuestion>,
}

/// One question's draft answer: selected option labels and/or a free-text
/// custom ("Other") answer. When `custom` is non-empty it supersedes the
/// selections (mutually exclusive, matching the tool's own resolution).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub selected: Vec<String>,
    pub custom: Option<String>,
}

impl QuestionAnswer {
    /// The resolved wire value for one question, or `None` when unanswered.
    /// Custom text wins; otherwise the selected labels joined with `", "`
    /// (single-select yields the one label).
    pub fn resolved(&self) -> Option<String> {
        if let Some(c) = self.custom.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(c.to_string());
        }
        let labels: Vec<&str> = self
            .selected
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        (!labels.is_empty()).then(|| labels.join(", "))
    }
}

/// The full answer set for a request: one draft per question (keyed by question
/// id) plus an optional overall free-text response that supersedes the
/// per-question map when the user types a general reply instead of answering.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswers {
    /// Keyed by question `id`. Remapped to question text in [`updated_input_json`].
    pub by_question: HashMap<String, QuestionAnswer>,
    pub response: Option<String>,
}

impl QuestionAnswers {
    /// True when every question has a resolved answer (strict submit gating).
    pub fn all_answered(&self, questions: &[AskQuestion]) -> bool {
        questions
            .iter()
            .all(|q| self.by_question.get(&q.id).and_then(QuestionAnswer::resolved).is_some())
    }

    /// A short human summary for the settled/collapsed state, e.g.
    /// `Format: Summary · Sections: Introduction, Conclusion`.
    pub fn summary(&self, questions: &[AskQuestion]) -> String {
        if let Some(r) = self.response.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return format!("Responded: {r}");
        }
        questions
            .iter()
            .filter_map(|q| {
                self.by_question
                    .get(&q.id)
                    .and_then(QuestionAnswer::resolved)
                    .map(|a| format!("{}: {a}", q.header))
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Parse the `input.questions` array of an AskUserQuestion `can_use_tool`
/// request into typed questions. Missing/malformed fields degrade to empty
/// rather than panicking (the wire schema drifts across `claude` versions).
pub fn parse_questions(input: &Value) -> Vec<AskQuestion> {
    let Some(arr) = input.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .map(|(idx, q)| {
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .map(|o| QuestionOption {
                            label: str_field(o, "label"),
                            description: str_field(o, "description"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
            AskQuestion {
                id: format!("q-{idx}"),
                header: str_field(q, "header"),
                question: str_field(q, "question"),
                options,
                kind: if multi { QuestionKind::MultiSelect } else { QuestionKind::SingleSelect },
                other_allowed: true,
                is_secret: false,
            }
        })
        .collect()
}

/// Build the `updatedInput` object for the answer control_response: echoes the
/// questions (required by the tool), maps each answered question's TEXT to its
/// resolved value, and adds the overall `response` only when non-empty. A
/// question with no resolved answer is omitted (Skip semantics = "did not
/// answer").
pub fn updated_input_json(questions: &[AskQuestion], answers: &QuestionAnswers) -> Value {
    let wire_questions: Vec<Value> = questions.iter().map(AskQuestion::to_wire).collect();
    let mut answer_map = serde_json::Map::new();
    for q in questions {
        if let Some(a) = answers.by_question.get(&q.id).and_then(QuestionAnswer::resolved) {
            answer_map.insert(q.question.clone(), Value::String(a));
        }
    }
    let mut updated = json!({ "questions": wire_questions, "answers": Value::Object(answer_map) });
    if let Some(r) = answers.response.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        updated["response"] = Value::String(r.to_string());
    }
    updated
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(idx: usize, text: &str, header: &str, opts: &[&str], multi: bool) -> AskQuestion {
        AskQuestion {
            id: format!("q-{idx}"),
            header: header.into(),
            question: text.into(),
            options: opts
                .iter()
                .map(|l| QuestionOption { label: (*l).into(), description: String::new() })
                .collect(),
            kind: if multi { QuestionKind::MultiSelect } else { QuestionKind::SingleSelect },
            other_allowed: true,
            is_secret: false,
        }
    }

    #[test]
    fn parse_questions_reads_all_fields() {
        let input = json!({"questions":[
            {"question":"Tabs or spaces?","header":"Indent",
             "options":[{"label":"Tabs","description":"tab chars"},{"label":"Spaces","description":"space chars"}],
             "multiSelect":false},
            {"question":"Which sections?","header":"Sections",
             "options":[{"label":"Intro","description":""}],"multiSelect":true},
        ]});
        let qs = parse_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].id, "q-0");
        assert_eq!(qs[0].question, "Tabs or spaces?");
        assert_eq!(qs[0].header, "Indent");
        assert_eq!(qs[0].options.len(), 2);
        assert!(!qs[0].multi_select());
        assert!(qs[1].multi_select());
        // malformed → empty, no panic
        assert!(parse_questions(&json!({})).is_empty());
    }

    #[test]
    fn resolved_prefers_custom_then_joins_labels() {
        let mut a = QuestionAnswer { selected: vec!["Tabs".into()], custom: None };
        assert_eq!(a.resolved().as_deref(), Some("Tabs"));
        a.selected = vec!["Intro".into(), "Outro".into()];
        assert_eq!(a.resolved().as_deref(), Some("Intro, Outro"));
        a.custom = Some("  jquery ".into());
        assert_eq!(a.resolved().as_deref(), Some("jquery")); // custom wins + trimmed
        assert!(QuestionAnswer::default().resolved().is_none());
    }

    #[test]
    fn updated_input_maps_answers_by_question_text() {
        let questions = vec![
            q(0, "Tabs or spaces?", "Indent", &["Tabs", "Spaces"], false),
            q(1, "Which sections?", "Sections", &["Intro", "Outro"], true),
        ];
        let mut answers = QuestionAnswers::default();
        answers.by_question.insert("q-0".into(), QuestionAnswer { selected: vec!["Tabs".into()], custom: None });
        answers.by_question.insert(
            "q-1".into(),
            QuestionAnswer { selected: vec!["Intro".into(), "Outro".into()], custom: None },
        );
        let updated = updated_input_json(&questions, &answers);
        // answers keyed by full question TEXT; multiSelect joined with ", "
        assert_eq!(updated["answers"]["Tabs or spaces?"], json!("Tabs"));
        assert_eq!(updated["answers"]["Which sections?"], json!("Intro, Outro"));
        // questions echoed in wire shape (multiSelect bool, not the kind enum)
        assert_eq!(updated["questions"][0]["multiSelect"], json!(false));
        assert_eq!(updated["questions"][1]["multiSelect"], json!(true));
        assert!(updated.get("response").is_none());
    }

    #[test]
    fn other_text_and_skip_and_response() {
        let questions = vec![
            q(0, "Framework?", "FW", &["React", "Vue"], false),
            q(1, "Skipped?", "Skip", &["A", "B"], false),
        ];
        // q-0 answered via custom "Other" text; q-1 left unanswered → omitted.
        let mut answers = QuestionAnswers::default();
        answers.by_question.insert("q-0".into(), QuestionAnswer { selected: vec![], custom: Some("Svelte".into()) });
        let updated = updated_input_json(&questions, &answers);
        assert_eq!(updated["answers"]["Framework?"], json!("Svelte"));
        assert!(updated["answers"].get("Skipped?").is_none(), "unanswered question omitted");

        // overall response added when set
        answers.response = Some("just pick sensible defaults".into());
        let updated = updated_input_json(&questions, &answers);
        assert_eq!(updated["response"], json!("just pick sensible defaults"));
    }

    #[test]
    fn all_answered_is_strict() {
        let questions = vec![
            q(0, "One?", "One", &["A", "B"], false),
            q(1, "Two?", "Two", &["A", "B"], false),
        ];
        let mut answers = QuestionAnswers::default();
        answers.by_question.insert("q-0".into(), QuestionAnswer { selected: vec!["A".into()], custom: None });
        assert!(!answers.all_answered(&questions), "one unanswered → not all");
        answers.by_question.insert("q-1".into(), QuestionAnswer { selected: vec!["B".into()], custom: None });
        assert!(answers.all_answered(&questions));
        assert_eq!(answers.summary(&questions), "One: A · Two: B");
    }
}
