//! The host's attention owner: one answer, published as changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oximux_agents::attention::{AttentionKind, AttentionSource, ExternalAttention};
use oximux_agents::health::{HealthInputs, Process, PromptKind};
use oximux_agents::lineage::LineageStore;
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::connection::StubConnection;

use super::AttentionChanged;
use crate::host_config::HostConfig;
use crate::host_core::{HostCore, HostCoreDeps, PriorOwner};

struct Fixture {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    _rt: tokio::runtime::Runtime,
}

fn fixture() -> Fixture {
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.start(PriorOwner::None);
    Fixture { core, registry, _rt: rt }
}

fn register(f: &Fixture, id: &str) -> Arc<SessionHandle> {
    f.registry.register(id.to_string(), Arc::new(StubConnection::default()))
}

fn now() -> i64 {
    oximux_agents::session_health::now_ms()
}

/// A session waiting on a person.
fn needs_permission() -> HealthInputs {
    HealthInputs {
        process: Process::Live,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("req-1".into()),
        last_activity_at: now(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    }
}

fn external(key: &str, target: &str, kind: AttentionKind) -> ExternalAttention {
    ExternalAttention {
        key: key.into(),
        target: target.into(),
        kind,
        reason: format!("{key} says so"),
        since: now(),
    }
}

/// Wait for a change about `target`, so a test never depends on how fast a
/// follower task is scheduled.
fn next_change_for(
    rx: &mut tokio::sync::broadcast::Receiver<AttentionChanged>,
    target: &str,
) -> AttentionChanged {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rx.try_recv() {
            Ok(change) if change.target == target => return change,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "no change for {target}");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("{err}"),
        }
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ─── the evaluator, through the host ─────────────────────────────────────────

#[test]
fn a_session_waiting_on_a_person_needs_you_and_a_quiet_one_does_not() {
    let f = fixture();
    let h = register(&f, "s1");
    assert_eq!(f.core.attention_of("s1"), None, "a session with no published health wants nothing");

    h.publish_health(needs_permission());
    let a = f.core.attention_of("s1").expect("raised");
    assert_eq!(a.kind, AttentionKind::NeedsPermission);
    assert_eq!(a.source, AttentionSource::Health);
    assert_eq!(f.core.needs_you().len(), 1);

    // Answered: it drops out of needs-you. What is left is the turn nobody
    // has read yet, which is shown but needs nobody.
    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    assert_eq!(f.core.attention_of("s1").unwrap().kind, AttentionKind::FinishedUnread);
    assert!(f.core.needs_you().is_empty(), "nothing waits on a person");
}

#[test]
fn needs_you_is_ordered_worst_first() {
    let f = fixture();
    let a = register(&f, "a-failed");
    let b = register(&f, "b-permission");
    a.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    b.publish_health(needs_permission());

    let order: Vec<_> = f.core.needs_you().into_iter().map(|(t, _)| t).collect();
    assert_eq!(order, vec!["b-permission", "a-failed"], "a prompt outranks a failure");
}

#[test]
fn an_informational_state_is_shown_but_not_counted() {
    let f = fixture();
    let h = register(&f, "s1");
    // A turn ran and nobody has read it.
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_activity_at: now(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    assert_eq!(f.core.attention_of("s1").unwrap().kind, AttentionKind::FinishedUnread);
    assert!(f.core.needs_you().is_empty(), "a finished turn needs nobody");
}

// ─── what the person has seen ────────────────────────────────────────────────

#[test]
fn viewing_a_failed_session_takes_it_out_of_needs_you_and_it_stays_out() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 1_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    assert_eq!(f.core.needs_you().len(), 1);

    f.core.mark_viewed("s1");
    assert!(f.core.needs_you().is_empty(), "a failure that has been looked at is not an alert");
    // And it is persisted, so a re-read says the same.
    assert!(f.core.prefs_of("s1").last_viewed_at.is_some());
    assert_eq!(f.core.attention_of("s1"), None);
}

#[test]
fn marking_a_session_unread_brings_back_a_finish_that_was_read() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_activity_at: now() - 1_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    f.core.mark_viewed("s1");
    assert_eq!(f.core.attention_of("s1"), None);
    f.core.mark_unread("s1");
    assert_eq!(f.core.attention_of("s1").unwrap().kind, AttentionKind::FinishedUnread);
}

#[test]
fn a_prefs_key_keeps_a_chat_apart_from_a_run_root() {
    let f = fixture();
    assert_eq!(f.core.prefs_key("s1"), "chat:s1");
    assert_eq!(f.core.prefs_key("plan:run-1"), "plan:run-1");
    assert_eq!(f.core.prefs_key("team:run-1"), "team:run-1");
    assert_eq!(f.core.prefs_key("cockpit:7"), "cockpit:7");
}

// ─── alerts no session's health can know about ───────────────────────────────

#[test]
fn an_external_alert_is_raised_on_a_target_with_no_session_and_cleared_by_its_producer() {
    let f = fixture();
    let item = external("cockpit:7:status", "cockpit:7", AttentionKind::NeedsPermission);
    f.core.publish_external(item.clone());

    let a = f.core.attention_of("cockpit:7").expect("raised");
    assert_eq!(a.kind, AttentionKind::NeedsPermission);
    assert_eq!(a.source, AttentionSource::External("cockpit:7:status".into()));
    assert_eq!(f.core.needs_you().len(), 1, "a terminal agent counts like any other");

    f.core.clear_external("cockpit:7:status");
    assert_eq!(f.core.attention_of("cockpit:7"), None);
    assert!(f.core.needs_you().is_empty());
}

#[test]
fn republishing_the_same_external_alert_changes_nothing() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let item = external("ci", "plan:run-1", AttentionKind::Failed);
    f.core.publish_external(item.clone());
    let first = next_change_for(&mut rx, "plan:run-1");
    assert!(first.attention.is_some());

    // A producer re-publishing from its own state on boot: the same edge.
    f.core.publish_external(item);
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "no second alert for the same trip"
    );
}

#[test]
fn an_external_alert_on_a_session_is_ranked_against_its_own_health() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    f.core.publish_external(external("ci", "s1", AttentionKind::Failed));
    // The session's own prompt is the more urgent of the two.
    assert_eq!(f.core.attention_of("s1").unwrap().kind, AttentionKind::NeedsPermission);
    assert_eq!(f.core.external_for("s1").len(), 1, "and the CI alert is still there to render");

    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    assert_eq!(f.core.attention_of("s1").unwrap().kind, AttentionKind::Failed, "now CI's turn");
}

// ─── the stream ──────────────────────────────────────────────────────────────

#[test]
fn a_health_change_publishes_one_change_with_its_edge() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());

    let change = next_change_for(&mut rx, "s1");
    let edge = change.edge.expect("an edge to alert on");
    assert_eq!(edge.kind, AttentionKind::NeedsPermission);
    assert_eq!(edge.id, "req:req-1", "the request being answered");
    assert_eq!(change.retired, None, "nothing was showing before");
}

#[test]
fn the_same_alert_re_derived_is_never_published_twice() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    next_change_for(&mut rx, "s1");

    // The host re-derives (the stall tick, a reconnect): the same request, so
    // the same edge, so nothing a surface has to hear about.
    f.core.reevaluate("s1");
    f.core.refresh_health(now(), false);
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "one edge, one alert"
    );
}

#[test]
fn answering_a_prompt_retires_its_edge() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let raised = next_change_for(&mut rx, "s1").edge.expect("raised");

    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    let change = next_change_for(&mut rx, "s1");
    assert!(
        !change.attention.as_ref().unwrap().kind.is_needs_you(),
        "it no longer waits on a person"
    );
    assert_eq!(change.retired, Some(raised), "and names what the banner was about");
}

#[test]
fn a_second_different_prompt_replaces_the_first_edge() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let first = next_change_for(&mut rx, "s1").edge.expect("raised");

    h.publish_health(HealthInputs {
        prompt_request: Some("req-2".into()),
        last_activity_at: now() + 1,
        ..needs_permission()
    });
    let change = next_change_for(&mut rx, "s1");
    let second = change.edge.expect("raised");
    assert_ne!(second, first, "a different request is a different alert");
    assert_eq!(change.retired, Some(first), "the first is retired as the second posts");
}

#[test]
fn a_session_ending_retires_what_it_wanted() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let raised = next_change_for(&mut rx, "s1").edge.expect("raised");

    drop(h);
    f.registry.unregister("s1");
    let change = next_change_for(&mut rx, "s1");
    assert_eq!(change.attention, None);
    assert_eq!(change.retired, Some(raised));
}

#[test]
fn a_session_unregistered_while_a_view_still_holds_it_retires_too() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    // The handle stays alive, as it does while a desktop view holds it, so
    // its health watch never closes and its follower never wakes.
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let raised = next_change_for(&mut rx, "s1").edge.expect("raised");

    f.registry.unregister("s1");
    let change = next_change_for(&mut rx, "s1");
    assert_eq!(change.attention, None, "a session the host no longer has wants nothing");
    assert_eq!(change.retired, Some(raised));
    drop(h);
}

#[test]
fn a_session_that_wants_nothing_publishes_nothing() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    // Working and talking: no alert, so no change to hear about.
    h.publish_health(HealthInputs {
        process: Process::Live,
        turn_active: true,
        last_activity_at: now(),
        ..Default::default()
    });
    wait_until("the follower to have looked", || f.core.attention_targets().contains(&"s1".to_string()));
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "a busy agent is not an alert"
    );
}

#[test]
fn viewing_a_session_retires_its_alert_on_the_stream() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 1_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    let raised = next_change_for(&mut rx, "s1").edge.expect("raised");

    f.core.mark_viewed("s1");
    let change = next_change_for(&mut rx, "s1");
    assert_eq!(change.attention, None);
    assert_eq!(change.retired, Some(raised));
}

#[test]
fn a_changed_line_on_the_same_alert_updates_it_without_retiring_it() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    let failing = HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 1_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    };
    h.publish_health(failing.clone());
    let raised = next_change_for(&mut rx, "s1").edge.expect("raised");

    // The same failed turn, reported with a fuller reason.
    h.publish_health(HealthInputs {
        last_turn_failure: Some("turn failed: rate limit retries exhausted".into()),
        ..failing
    });
    let change = next_change_for(&mut rx, "s1");
    assert_eq!(change.edge.as_ref(), Some(&raised), "still the same failure");
    assert_eq!(
        change.retired, None,
        "retiring and re-posting it would sound a second alert for a wording change"
    );
    assert!(change.attention.unwrap().reason.unwrap().contains("exhausted"), "the row updates");
}

#[test]
fn activity_alone_publishes_nothing() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    next_change_for(&mut rx, "s1");

    // The session keeps working on the same prompt: the timestamp moves and
    // nothing a person reads changes.
    h.publish_health(HealthInputs { last_activity_at: now() + 5_000, ..needs_permission() });
    f.core.refresh_health(now() + 5_000, false);
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "the events already say it is alive"
    );
}
