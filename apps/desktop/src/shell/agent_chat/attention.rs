//! Which thread events are worth interrupting the user for.
//!
//! A pure classifier, deliberately separate from the view: it decides only
//! *what kind* of attention an event deserves. The host applies the focus,
//! visibility and per-kind gates before anything actually surfaces, so nothing
//! here needs to know whether the window is frontmost.

use oximux_agents::thread::ThreadEvent;

use super::bubble;
use crate::notifier::NotificationKind;

/// Classify a live thread event into an attention notification `(kind, body)`, or
/// `None` when it isn't attention-worthy. A turn end (finished/errored) requires a
/// turn to have been active (`was_active`) so a stray result can't banner; an
/// intentional Stop (`interrupted`) is not a failure. Permission / question / auth
/// prompts always signal — they block the user.
pub(super) fn attention_for_event(
    ev: &ThreadEvent,
    was_active: bool,
    interrupted: bool,
) -> Option<(NotificationKind, String)> {
    match ev {
        // An intentional Stop suppresses BOTH shapes of turn end it can produce: a
        // Claude interrupt arrives as `is_error: true`, but an ACP cancel replies
        // `StopReason::Cancelled` → `TurnEnded { is_error: false, turn_diff: None }`. Either way,
        // `interrupted` means the user pressed Stop, so no "finished"/"failed"
        // banner should fire — check it before the error split.
        ThreadEvent::TurnEnded { .. } if was_active && interrupted => None,
        ThreadEvent::TurnEnded { is_error: true, result, .. } if was_active => {
            let head = result
                .as_deref()
                .unwrap_or_default()
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            Some((NotificationKind::Failed, bubble::elide(head, 120)))
        }
        ThreadEvent::TurnEnded { is_error: false, .. } if was_active => {
            Some((NotificationKind::Done, String::new()))
        }
        ThreadEvent::PermissionRequested { tool_name, .. } => {
            Some((NotificationKind::NeedsApproval, tool_name.clone()))
        }
        ThreadEvent::QuestionAsked { .. } => {
            Some((NotificationKind::NeedsApproval, "waiting for your answer".to_string()))
        }
        ThreadEvent::AuthRequired { .. } => {
            Some((NotificationKind::NeedsApproval, "sign-in required".to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attention_classifies_turn_permission_question_auth() {
        // A finished turn (a turn was active) → Done, empty body.
        assert_eq!(
            attention_for_event(
                &ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                true,
                false,
            ),
            Some((NotificationKind::Done, String::new())),
        );
        // An errored turn → Failed, first-line body.
        assert_eq!(
            attention_for_event(
                &ThreadEvent::TurnEnded {
                    result: Some("boom happened\nmore".into()), usage: None, is_error: true, turn_diff: None },
                true,
                false,
            ),
            Some((NotificationKind::Failed, "boom happened".to_string())),
        );
        // An intentional Stop (interrupted) errored turn → no banner (Claude shape).
        assert_eq!(
            attention_for_event(
                &ThreadEvent::TurnEnded { result: Some("aborted".into()), usage: None, is_error: true, turn_diff: None },
                true,
                true,
            ),
            None,
        );
        // An intentional Stop on an ACP agent replies Cancelled → a NON-error turn
        // end while interrupted; it must NOT fire a "finished" banner.
        assert_eq!(
            attention_for_event(
                &ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                true,
                true,
            ),
            None,
        );
        // A turn end with no prior active turn → no banner (stray result).
        assert_eq!(
            attention_for_event(
                &ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                false,
                false,
            ),
            None,
        );
        // Permission / question / auth all → NeedsApproval regardless of was_active.
        assert!(matches!(
            attention_for_event(
                &ThreadEvent::PermissionRequested {
                    request_id: "r".into(), tool_use_id: None, tool_name: "Bash".into(),
                    input: json!({}), description: String::new(), suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                },
                false,
                false,
            ),
            Some((NotificationKind::NeedsApproval, ref b)) if b == "Bash",
        ));
        assert!(matches!(
            attention_for_event(&ThreadEvent::QuestionAsked {
                request_id: "r".into(), tool_use_id: None, questions: vec![] }, false, false),
            Some((NotificationKind::NeedsApproval, _)),
        ));
        assert!(matches!(
            attention_for_event(&ThreadEvent::AuthRequired { methods: vec![], error: None }, false, false),
            Some((NotificationKind::NeedsApproval, _)),
        ));
        // A plain text/tool event → nothing.
        assert_eq!(attention_for_event(&ThreadEvent::AssistantText("hi".into()), true, false), None);
    }
}
