//! ACP permission requests ↔ OxiMux's permission card.
//!
//! An agent's `session/request_permission` carries a tool call plus an
//! agent-defined `options` list (each with an `option_id` + a `kind`:
//! allow-once / allow-always / reject-once / reject-always). We surface it as a
//! [`ThreadEvent::PermissionRequested`] card, park the async responder on a
//! oneshot, and — when the user answers via `resolve_permission` — translate the
//! provider-neutral [`PermissionDecision`] into the best-matching `option_id`.

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, SelectedPermissionOutcome,
};
use serde_json::{Value, json};

use crate::thread::event::ThreadEvent;
use crate::thread::tool_call::{PermissionDecision, PermissionKind, PermissionSuggestion};

/// The suggestion `kind` marking a pill that carries an exact ACP `option_id`
/// (as opposed to a Claude-style setMode/addRules suggestion).
const ACP_OPTION_KIND: &str = "acp_option";

/// Surface an incoming permission request as a card. `request_id` is the stable
/// key the UI echoes back to `resolve_permission` (we use the tool-call id).
pub(crate) fn permission_event(req: &RequestPermissionRequest, request_id: &str) -> ThreadEvent {
    let fields = &req.tool_call.fields;
    let input = fields.raw_input.clone().unwrap_or(Value::Null);
    // The card label: the tool's title if the agent gave one, else the command it
    // wants to run (from raw_input), else a generic fallback — so the prompt reads
    // "Allow curl -sI …?" rather than "Allow tool?".
    let label = fields
        .title
        .clone()
        .or_else(|| input.get("command").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "tool".to_string());
    ThreadEvent::PermissionRequested {
        request_id: request_id.to_string(),
        tool_use_id: Some(request_id.to_string()),
        tool_name: label.clone(),
        input,
        description: label,
        suggestions: allow_option_suggestions(&req.options),
        kind: PermissionKind::Tool,
    }
}

/// The `option_id` the base **Allow** button maps to (via [`decision_to_outcome`]'s
/// Allow preference: first allow-once, else first allow-always) — excluded from
/// the pills so they surface only the *extra* allow options.
fn primary_allow_id(options: &[PermissionOption]) -> Option<String> {
    options
        .iter()
        .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce))
        .or_else(|| options.iter().find(|o| matches!(o.kind, PermissionOptionKind::AllowAlways)))
        .map(|o| o.option_id.0.to_string())
}

/// Build suggestion pills from the agent's ALLOW-kind permission options beyond
/// the one the base Allow button already maps to (e.g. "Always allow edits").
/// Reject-kind options stay behind the base Reject button — the shared
/// `AllowWithSuggestion` wrapper is allow-shaped, so a reject riding it would be
/// semantically wrong. Each pill carries the exact `option_id` so
/// [`decision_to_outcome`] answers with it verbatim.
fn allow_option_suggestions(options: &[PermissionOption]) -> Vec<PermissionSuggestion> {
    let primary = primary_allow_id(options);
    options
        .iter()
        .filter(|o| {
            matches!(o.kind, PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways)
                && Some(o.option_id.0.as_ref()) != primary.as_deref()
        })
        .map(|o| PermissionSuggestion {
            kind: ACP_OPTION_KIND.to_string(),
            label: o.name.clone(),
            raw: json!({ "optionId": o.option_id.0.to_string() }),
        })
        .collect()
}

/// Best-match a decision onto the agent's option list: match by kind first, then
/// fall back (allow → first option; deny → cancel). Never panics on unknown kinds.
pub(crate) fn decision_to_outcome(
    decision: &PermissionDecision,
    options: &[PermissionOption],
) -> RequestPermissionOutcome {
    // An ACP option pill was clicked: answer with its exact `option_id`. The
    // wrapper is allow-shaped, but the suggestion carries the real option (only
    // allow-kind options are pilled, so the allow semantics stay honest).
    if let PermissionDecision::AllowWithSuggestion { suggestion, .. } = decision
        && suggestion.kind == ACP_OPTION_KIND
        && let Some(id) = suggestion.raw.get("optionId").and_then(Value::as_str)
    {
        return RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new(id.to_string()),
        ));
    }
    let preferred: &[PermissionOptionKind] = match decision {
        PermissionDecision::Allow { .. } => {
            &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
        }
        PermissionDecision::AllowWithSuggestion { .. } => {
            &[PermissionOptionKind::AllowAlways, PermissionOptionKind::AllowOnce]
        }
        PermissionDecision::Deny { .. } => {
            &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
        }
    };
    for kind in preferred {
        if let Some(opt) = options.iter().find(|o| &o.kind == kind) {
            return selected(opt);
        }
    }
    // No kind match: allow-ish decisions take the first offered option; a denial
    // with no reject option cancels the request outright.
    match decision {
        PermissionDecision::Deny { .. } => RequestPermissionOutcome::Cancelled,
        _ => options.first().map(selected).unwrap_or(RequestPermissionOutcome::Cancelled),
    }
}

fn selected(opt: &PermissionOption) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(opt.option_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opt(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(id.to_string(), id.to_string(), kind)
    }

    fn outcome_id(o: &RequestPermissionOutcome) -> Option<String> {
        match o {
            RequestPermissionOutcome::Selected(s) => Some(s.option_id.0.to_string()),
            _ => None,
        }
    }

    fn allow() -> PermissionDecision {
        PermissionDecision::Allow { updated_input: json!({}) }
    }
    fn deny() -> PermissionDecision {
        PermissionDecision::Deny { message: "no".into() }
    }

    #[test]
    fn allow_picks_allow_once() {
        let opts = vec![
            opt("yes", PermissionOptionKind::AllowOnce),
            opt("no", PermissionOptionKind::RejectOnce),
        ];
        assert_eq!(outcome_id(&decision_to_outcome(&allow(), &opts)).as_deref(), Some("yes"));
    }

    #[test]
    fn deny_picks_reject() {
        let opts = vec![
            opt("yes", PermissionOptionKind::AllowOnce),
            opt("no", PermissionOptionKind::RejectOnce),
        ];
        assert_eq!(outcome_id(&decision_to_outcome(&deny(), &opts)).as_deref(), Some("no"));
    }

    #[test]
    fn deny_with_no_reject_option_cancels() {
        let opts = vec![opt("yes", PermissionOptionKind::AllowOnce)];
        assert!(matches!(decision_to_outcome(&deny(), &opts), RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn allow_falls_back_to_first_when_no_kind_matches() {
        // Only reject-ish options offered, but the user allowed → take the first.
        let opts =
            vec![opt("a", PermissionOptionKind::RejectAlways), opt("b", PermissionOptionKind::RejectOnce)];
        assert_eq!(outcome_id(&decision_to_outcome(&allow(), &opts)).as_deref(), Some("a"));
    }

    #[test]
    fn allow_option_pills_exclude_the_base_allow_and_all_rejects() {
        // Options: allow-once (→ base Allow), allow-always (→ pill), reject-once
        // (→ base Reject, NOT pilled). Only the extra allow-always becomes a pill.
        let opts = vec![
            opt("yes", PermissionOptionKind::AllowOnce),
            opt("always", PermissionOptionKind::AllowAlways),
            opt("no", PermissionOptionKind::RejectOnce),
        ];
        let sugg = allow_option_suggestions(&opts);
        assert_eq!(sugg.len(), 1, "only the extra allow option is pilled");
        assert_eq!(sugg[0].kind, ACP_OPTION_KIND);
        assert_eq!(sugg[0].label, "always");
        assert_eq!(sugg[0].raw["optionId"], "always");
    }

    #[test]
    fn claude_style_permission_has_no_acp_pills() {
        // A request with just one allow + one reject (the common shape) yields no
        // pills — the Claude permission card stays byte-identical (empty suggestions).
        let opts = vec![
            opt("yes", PermissionOptionKind::AllowOnce),
            opt("no", PermissionOptionKind::RejectOnce),
        ];
        assert!(allow_option_suggestions(&opts).is_empty());
    }

    #[test]
    fn acp_option_pill_answers_with_its_exact_option_id() {
        // A clicked pill routes AllowWithSuggestion(kind=acp_option) → the carried
        // option_id verbatim, bypassing the kind-matching fallback.
        let opts = vec![
            opt("yes", PermissionOptionKind::AllowOnce),
            opt("always", PermissionOptionKind::AllowAlways),
        ];
        let decision = PermissionDecision::AllowWithSuggestion {
            updated_input: json!({}),
            suggestion: PermissionSuggestion {
                kind: ACP_OPTION_KIND.into(),
                label: "always".into(),
                raw: json!({ "optionId": "always" }),
            },
        };
        assert_eq!(outcome_id(&decision_to_outcome(&decision, &opts)).as_deref(), Some("always"));
    }
}
