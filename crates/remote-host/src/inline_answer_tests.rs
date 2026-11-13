//! Answering a session from a row: what the host offers, and what it refuses.
//!
//! The paint and the click are different moments, so most of these are about
//! the second one — a proposal answered in between, a question raised in
//! between, a process replaced in between.

use std::sync::Arc;

use oximux_agents::attention::{Attention, AttentionKind, AttentionSource};
use oximux_agents::health::{HealthInputs, Process, PromptKind};
use oximux_agents::lineage::LineageStore;
use oximux_agents::row_actions::{InlineAnswer, RowAction};
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::connection::StubConnection;
use oximux_agents::thread::{
    AskQuestion, PermissionKind, QuestionKind, QuestionOption, ThreadEvent,
};

use crate::attention::chat_target;
use crate::host_config::HostConfig;
use crate::host_core::{HostCore, HostCoreDeps, PriorOwner};

struct Fixture {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    conn: Arc<StubConnection>,
    handle: Arc<SessionHandle>,
    _rt: tokio::runtime::Runtime,
}

/// A host with one live session, waiting on a person.
fn fixture() -> Fixture {
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = Arc::new(oximux_storage::db::open_memory().unwrap());
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.start(PriorOwner::None);
    let (conn, _rx, _tx) = StubConnection::new();
    let conn = Arc::new(conn);
    let handle = registry.register("s1".into(), conn.clone());
    handle.publish_health(HealthInputs {
        process: Process::Live,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("req-1".into()),
        last_activity_at: oximux_agents::session_health::now_ms(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    // The handle holds the connection, so the fixture's own `conn` is the
    // same object the dispatch writes to.
    Fixture { core, registry, conn, handle, _rt: rt }
}

fn target() -> String {
    chat_target("s1")
}

fn asking(kind: AttentionKind) -> Attention {
    Attention { kind, since: 0, reason: None, source: AttentionSource::Health }
}

fn permission(id: &str, tool: &str, input: serde_json::Value) -> ThreadEvent {
    ThreadEvent::PermissionRequested {
        request_id: id.into(),
        tool_use_id: Some(format!("toolu_{id}")),
        tool_name: tool.into(),
        input,
        description: String::new(),
        suggestions: vec![],
        kind: PermissionKind::Tool,
    }
}

fn question(id: &str, options: Vec<&str>) -> ThreadEvent {
    ThreadEvent::QuestionAsked {
        request_id: id.into(),
        tool_use_id: Some(format!("toolu_{id}")),
        questions: vec![AskQuestion {
            id: "q-0".into(),
            header: "README".into(),
            question: "Which README?".into(),
            options: options
                .into_iter()
                .map(|label| QuestionOption { label: label.into(), description: String::new() })
                .collect(),
            kind: QuestionKind::SingleSelect,
            other_allowed: true,
            is_secret: false,
        }],
    }
}

/// The digest the row was shown with, for the one pending proposal.
fn shown_digest(f: &Fixture) -> String {
    let pending = f.handle.outstanding_permissions();
    let [only] = pending.as_slice() else {
        panic!("exactly one proposal should be pending");
    };
    only.args_digest.clone()
}

#[test]
fn a_row_offers_allow_and_reject_for_the_one_call_it_can_show() {
    let f = fixture();
    f.registry.ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "ls -la"})));
    let actions = f.core.row_actions(&target(), Some(&asking(AttentionKind::NeedsPermission)));
    assert_eq!(actions.actions, vec![RowAction::Allow, RowAction::Reject]);
    assert_eq!(actions.summary.as_deref(), Some("Bash: ls -la"));
}

#[test]
fn a_target_with_no_live_session_offers_its_dismiss_and_nothing_else() {
    let f = fixture();
    // A history row or a cockpit agent: nothing in the registry under this
    // key, so nothing to answer — but a failure there must still be
    // clearable, or the row asks forever.
    let actions = f.core.row_actions("cockpit:7", Some(&asking(AttentionKind::Failed)));
    assert_eq!(actions.actions, vec![RowAction::Dismiss]);
    assert_eq!(actions.answer, InlineAnswer::Nothing);
    // And nothing to approve, since there is no proposal to hold.
    assert!(f.core.row_actions("cockpit:7", Some(&asking(AttentionKind::NeedsPermission))).is_empty());
    // Nor a Reply. The review's finding: a terminal agent parsed as "waiting
    // for input" is indistinguishable from a live chat with nothing pending,
    // so this offered a button whose every press failed with "no live
    // session to answer".
    assert!(f.core.row_actions("cockpit:7", Some(&asking(AttentionKind::NeedsInput))).is_empty());
}

#[test]
fn allow_echoes_the_proposal_the_host_itself_still_holds() {
    let f = fixture();
    f.registry.ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "ls -la"})));
    let digest = shown_digest(&f);
    assert!(f.core.allow_from_row(&target(), "req-1", &digest).unwrap());

    let sent = f.conn.sent();
    let [only] = sent.as_slice() else { panic!("one decision, got {sent:?}") };
    assert_eq!(
        only.pointer("/response/response/behavior").and_then(|v| v.as_str()),
        Some("allow"),
        "{only}"
    );
    // The arguments echoed back are the ones the host holds, read in the same
    // breath as the digest check — not a copy the surface was carrying, which
    // would be stale exactly when it mattered. The digest itself stops here:
    // it is the host's own check and no part of the agent's protocol, so
    // `control_response_json` is right not to forward it.
    assert_eq!(
        only.pointer("/response/response/updatedInput"),
        Some(&serde_json::json!({"command": "ls -la"})),
        "{only}"
    );
    assert!(f.handle.outstanding_permissions().is_empty(), "the proposal is answered");
}

#[test]
fn allow_is_refused_when_the_arguments_changed_under_the_row() {
    let f = fixture();
    f.registry.ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "ls -la"})));
    let digest = shown_digest(&f);
    // The adapter re-raises the same id with a different call — the one case
    // the digest exists to stop. A frame is long enough for this.
    f.registry
        .ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "rm -rf build/"})));

    let err = f.core.allow_from_row(&target(), "req-1", &digest).expect_err("must refuse");
    assert!(err.to_string().contains("changed since they were shown"), "{err}");
    assert!(f.conn.sent().is_empty(), "nothing reached the agent");
    // This is also the guard on the row carrying the digest at all: the host
    // only checks a decision that has one, so an approval sent from here
    // without it would be accepted against the arguments nobody was shown.
}

#[test]
fn allow_is_refused_once_the_proposal_has_been_answered_elsewhere() {
    let f = fixture();
    f.registry.ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "ls -la"})));
    let digest = shown_digest(&f);
    // The chat card got there first.
    f.handle
        .resolve_permission(
            "req-1",
            oximux_agents::thread::PermissionDecision::allow(serde_json::Value::Null),
        )
        .unwrap();

    let err = f.core.allow_from_row(&target(), "req-1", &digest).expect_err("must refuse");
    assert!(err.to_string().contains("no longer waiting"), "{err}");
}

#[test]
fn reject_needs_no_digest_so_a_changed_call_can_still_be_refused() {
    let f = fixture();
    f.registry.ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "ls -la"})));
    f.registry
        .ingest("s1", permission("req-1", "Bash", serde_json::json!({"command": "rm -rf /"})));
    // Approving the wrong request does something; refusing the wrong one is
    // at worst a refusal the agent reports. Demanding a match here would
    // leave a row you cannot say no from.
    assert!(f.core.reject_from_row(&target(), "req-1").unwrap());
    let sent = f.conn.sent();
    let [only] = sent.as_slice() else { panic!("one decision, got {sent:?}") };
    assert_eq!(
        only.pointer("/response/response/behavior").and_then(|v| v.as_str()),
        Some("deny"),
        "{only}"
    );
}

#[test]
fn a_reply_answers_the_pending_question_rather_than_starting_a_turn() {
    let f = fixture();
    f.registry.ingest("s1", question("q-1", vec![]));
    let actions = f.core.row_actions(&target(), Some(&asking(AttentionKind::NeedsPermission)));
    let answer = InlineAnswer::Question { request_id: "q-1".into() };
    assert_eq!(actions.answer, answer);

    assert!(f.core.reply_from_row(&target(), &answer, "  the root one  ").unwrap());
    let sent = f.conn.sent();
    let [only] = sent.as_slice() else { panic!("one answer, got {sent:?}") };
    // A question answer, not a user message: a prompt sent beside an
    // unanswered question leaves the agent still waiting on it.
    assert_eq!(
        only.pointer("/response/response/updatedInput/response").and_then(|v| v.as_str()),
        Some("the root one"),
        "trimmed, and in the response field: {only}"
    );
    // And in the per-question map as well. Codex reads ONLY that map and
    // ignores `response`, so a response-only answer reached a Codex session
    // as `{"answers":{}}`: the text dropped, the request settled, the box
    // closing as though it had worked.
    assert_eq!(
        only
            .pointer("/response/response/updatedInput/answers")
            .and_then(|v| v.as_object())
            .map(|m| m.len()),
        Some(1),
        "the sentence is in both dialects: {only}"
    );
}

#[test]
fn a_question_raised_between_the_paint_and_the_click_is_not_answered_by_accident() {
    let f = fixture();
    // Painted while the session merely waited on input: the row's Reply was
    // a prompt. The question arrives before the click.
    let painted = f.core.row_actions(&target(), Some(&asking(AttentionKind::NeedsInput)));
    assert_eq!(painted.answer, InlineAnswer::Prompt);
    f.registry.ingest("s1", question("q-1", vec![]));

    // Both routes would be wrong, so neither is taken. A prompt leaves the
    // question waiting; answering it uses words written for something else,
    // about a question the person has not seen. The text stays in the box.
    let err = f.core.reply_from_row(&target(), &painted.answer, "carry on").expect_err("refused");
    assert!(err.to_string().contains("asking a question now"), "{err}");
    assert!(f.conn.sent().is_empty(), "nothing reached the agent");
}

#[test]
fn a_reply_does_not_land_on_the_question_that_replaced_the_one_it_was_for() {
    let f = fixture();
    f.registry.ingest("s1", question("q-1", vec![]));
    let painted = f.core.row_actions(&target(), Some(&asking(AttentionKind::NeedsPermission)));
    assert_eq!(painted.answer, InlineAnswer::Question { request_id: "q-1".into() });

    // Answered in the chat, and the agent asks something else.
    f.handle.answer_question("q-1", &[], &Default::default()).unwrap();
    f.registry.ingest("s1", question("q-2", vec![]));

    // The digest rule, applied to a question: an answer has to name what it
    // answers, or a reply typed for one question settles another.
    let err = f.core.reply_from_row(&target(), &painted.answer, "the root one").expect_err("refused");
    assert!(err.to_string().contains("no longer the one waiting"), "{err}");
    assert_eq!(f.handle.pending_question().map(|q| q.request_id), Some("q-2".into()));
}

#[test]
fn a_reply_is_refused_while_a_choice_is_waiting() {
    let f = fixture();
    f.registry.ingest("s1", question("q-1", vec!["The root one", "The crate's"]));
    // The row offers nothing for a question with options, but the dispatch
    // must refuse it too: a row painted before the options arrived would
    // otherwise answer a pick with free text.
    assert!(f.core.row_actions(&target(), Some(&asking(AttentionKind::NeedsPermission))).is_empty());
    let answer = InlineAnswer::Question { request_id: "q-1".into() };
    let err = f.core.reply_from_row(&target(), &answer, "the root one").expect_err("must refuse");
    assert!(err.to_string().contains("waiting on a choice"), "{err}");
    assert!(f.conn.sent().is_empty(), "nothing reached the agent");
}

#[test]
fn a_reply_with_no_question_pending_is_a_prompt() {
    let f = fixture();
    f.handle.publish_health(HealthInputs {
        process: Process::Live,
        last_activity_at: oximux_agents::session_health::now_ms(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    assert!(f.core.reply_from_row(&target(), &InlineAnswer::Prompt, "carry on").unwrap());
    let sent = f.conn.sent();
    let [only] = sent.as_slice() else { panic!("one send, got {sent:?}") };
    assert_eq!(only.get("type").and_then(|v| v.as_str()), Some("user"), "{only}");
}

#[test]
fn an_empty_reply_sends_nothing() {
    let f = fixture();
    assert!(
        !f.core.reply_from_row(&target(), &InlineAnswer::Prompt, "   ").unwrap(),
        "nothing to say"
    );
    assert!(f.conn.sent().is_empty());
}

#[test]
fn replying_clears_unread_because_the_person_has_plainly_seen_it() {
    // B15: a session you have just answered can never sit in "unread".
    let f = fixture();
    f.handle.publish_health(HealthInputs {
        process: Process::Live,
        last_activity_at: oximux_agents::session_health::now_ms() - 1_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    f.core.mark_unread(&target());
    assert_eq!(
        f.core.attention_of(&target()).map(|a| a.kind),
        Some(AttentionKind::FinishedUnread),
        "held unread by hand"
    );

    assert!(f.core.reply_from_row(&target(), &InlineAnswer::Prompt, "carry on").unwrap());
    assert_eq!(f.core.attention_of(&target()), None, "answering it read it");
}

#[test]
fn nothing_can_be_answered_on_a_session_that_is_gone() {
    let f = fixture();
    for err in [
        f.core.allow_from_row("chat:missing", "req-1", "deadbeef").expect_err("no session"),
        f.core.reject_from_row("chat:missing", "req-1").expect_err("no session"),
        f.core
            .reply_from_row("chat:missing", &InlineAnswer::Prompt, "hello")
            .expect_err("no session"),
    ] {
        assert!(err.to_string().contains("no live session"), "{err}");
    }
}
