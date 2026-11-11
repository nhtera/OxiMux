//! The host's attention owner: one answer, published as changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oximux_agents::attention::{AttentionKind, AttentionSource, ExternalAttention};
use oximux_agents::health::{HealthInputs, Process, PromptKind};
use oximux_agents::lineage::LineageStore;
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::connection::StubConnection;

use super::{AttentionChanged, chat_target};
use crate::host_config::HostConfig;
use crate::host_core::{HostCore, HostCoreDeps, PriorOwner};

struct Fixture {
    core: Arc<HostCore>,
    registry: Arc<SessionRegistry>,
    db: Arc<oximux_storage::db::Db>,
    _rt: tokio::runtime::Runtime,
}

fn fixture() -> Fixture {
    fixture_on(Arc::new(oximux_storage::db::open_memory().unwrap()))
}

/// A host on an existing database — a restart, for the prefs that outlive one.
fn fixture_on(db: Arc<oximux_storage::db::Db>) -> Fixture {
    let rt =
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.start(PriorOwner::None);
    Fixture { core, registry, db, _rt: rt }
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
    assert_eq!(f.core.attention_of(&chat_target("s1")), None, "a session with no published health wants nothing");

    h.publish_health(needs_permission());
    let a = f.core.attention_of(&chat_target("s1")).expect("raised");
    assert_eq!(a.kind, AttentionKind::NeedsPermission);
    assert_eq!(a.source, AttentionSource::Health);
    assert_eq!(f.core.needs_you().len(), 1);

    // Answered: it drops out of needs-you. What is left is the turn nobody
    // has read yet, which is shown but needs nobody.
    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    assert_eq!(f.core.attention_of(&chat_target("s1")).unwrap().kind, AttentionKind::FinishedUnread);
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
    assert_eq!(
        order,
        vec![chat_target("b-permission"), chat_target("a-failed")],
        "a prompt outranks a failure"
    );
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
    assert_eq!(f.core.attention_of(&chat_target("s1")).unwrap().kind, AttentionKind::FinishedUnread);
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

    f.core.mark_viewed(&chat_target("s1"));
    assert!(f.core.needs_you().is_empty(), "a failure that has been looked at is not an alert");
    // And it is persisted, so a re-read says the same.
    assert!(f.core.prefs_of(&chat_target("s1")).last_viewed_at.is_some());
    assert_eq!(f.core.attention_of(&chat_target("s1")), None);
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
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")), None);
    f.core.mark_unread(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")).unwrap().kind, AttentionKind::FinishedUnread);
}

#[test]
fn a_chat_and_a_run_root_that_share_a_name_are_different_targets() {
    let f = fixture();
    // A provider free to name its own session could call one `plan:7`. The
    // namespace is what keeps it off the run `plan:7`'s row and out of its
    // preference record.
    let h = register(&f, "plan:7");
    h.publish_health(needs_permission());
    f.core.publish_external(external("breaker", "plan:7", AttentionKind::Failed));

    assert_eq!(
        f.core.attention_of(&chat_target("plan:7")).unwrap().kind,
        AttentionKind::NeedsPermission,
        "the chat's own prompt"
    );
    assert_eq!(
        f.core.attention_of("plan:7").unwrap().kind,
        AttentionKind::Failed,
        "and the run's breaker, separately"
    );
    assert_eq!(f.core.needs_you().len(), 2, "two things need a person, not one");

    // Viewing one does not mark the other seen.
    f.core.mark_viewed("plan:7");
    assert!(f.core.prefs_of(&chat_target("plan:7")).last_viewed_at.is_none());
}

#[test]
fn a_target_with_no_session_is_judged_without_one() {
    let f = fixture();
    f.core.publish_external(external("ci", "plan:run-1", AttentionKind::Failed));
    assert!(f.core.attention_of("plan:run-1").is_some());
    assert_eq!(f.core.attention_of(&chat_target("plan:run-1")), None, "no such chat");
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
    f.core.publish_external(external("ci", &chat_target("s1"), AttentionKind::Failed));
    // The session's own prompt is the more urgent of the two.
    assert_eq!(f.core.attention_of(&chat_target("s1")).unwrap().kind, AttentionKind::NeedsPermission);
    assert_eq!(f.core.external_for(&chat_target("s1")).len(), 1, "and the CI alert is still there to render");

    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    assert_eq!(f.core.attention_of(&chat_target("s1")).unwrap().kind, AttentionKind::Failed, "now CI's turn");
}

// ─── the stream ──────────────────────────────────────────────────────────────

#[test]
fn a_health_change_publishes_one_change_with_its_edge() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());

    let change = next_change_for(&mut rx, &chat_target("s1"));
    let edge = change.edge.expect("an edge to alert on");
    assert_eq!(edge.kind, AttentionKind::NeedsPermission);
    assert_eq!(
        edge.id, "req:0:req-1",
        "the request being answered, qualified by the process that asked"
    );
    assert_eq!(change.retired, None, "nothing was showing before");
}

#[test]
fn the_same_alert_re_derived_is_never_published_twice() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    next_change_for(&mut rx, &chat_target("s1"));

    // The host re-derives (the stall tick, a reconnect): the same request, so
    // the same edge, so nothing a surface has to hear about.
    f.core.reevaluate(&chat_target("s1"));
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
    let raised = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    h.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    let first = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    h.publish_health(HealthInputs {
        prompt_request: Some("req-2".into()),
        last_activity_at: now() + 1,
        ..needs_permission()
    });
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    let raised = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    drop(h);
    f.registry.unregister("s1");
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    let raised = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    f.registry.unregister("s1");
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    wait_until("the follower to have looked", || {
        f.core.attention_targets().contains(&chat_target("s1"))
    });
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
    let raised = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    f.core.mark_viewed(&chat_target("s1"));
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    let raised = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    // The same failed turn, reported with a fuller reason.
    h.publish_health(HealthInputs {
        last_turn_failure: Some("turn failed: rate limit retries exhausted".into()),
        ..failing
    });
    let change = next_change_for(&mut rx, &chat_target("s1"));
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
    next_change_for(&mut rx, &chat_target("s1"));

    // The session keeps working on the same prompt: the timestamp moves and
    // nothing a person reads changes.
    h.publish_health(HealthInputs { last_activity_at: now() + 5_000, ..needs_permission() });
    f.core.refresh_health(now() + 5_000, false);
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "the events already say it is alive"
    );
}

// ─── when an alert arose, which is what a view is measured against ───────────

#[test]
fn a_failure_that_arose_after_the_person_looked_is_not_already_seen() {
    let f = fixture();
    let h = register(&f, "s1");
    // They look at a healthy session.
    f.core.mark_viewed(&chat_target("s1"));
    // Its last output predates that look, and only now does the turn fail.
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 60_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    assert_eq!(
        f.core.attention_of(&chat_target("s1")).map(|a| a.kind),
        Some(AttentionKind::Failed),
        "the look came before the failure existed"
    );
    // Looking again, now that it has failed, does clear it.
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")), None);
}

#[test]
fn an_alert_uncovered_again_is_still_judged_by_when_it_arrived() {
    let f = fixture();
    // A stall-level alert arrives first, then a worse one covers it.
    f.core.publish_external(external("slow", "plan:run-1", AttentionKind::Stalled));
    assert_eq!(f.core.attention_of("plan:run-1").unwrap().kind, AttentionKind::Stalled);
    f.core.publish_external(external("ci", "plan:run-1", AttentionKind::Failed));
    assert_eq!(f.core.attention_of("plan:run-1").unwrap().kind, AttentionKind::Failed);

    // The person deals with what they can see, then the worse one clears.
    f.core.mark_viewed("plan:run-1");
    f.core.clear_external("ci");
    assert_eq!(
        f.core.attention_of("plan:run-1"),
        None,
        "the stall was raised before that look too, so it has been seen"
    );
}

// ─── external alerts that move or are replaced ──────────────────────────────

#[test]
fn moving_an_external_key_to_another_target_retires_it_on_the_old_one() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    f.core.publish_external(external("ci", "plan:run-1", AttentionKind::Failed));
    let raised = next_change_for(&mut rx, "plan:run-1").edge.expect("raised");

    // The producer re-attributes its alert to a role under the run.
    f.core.publish_external(external("ci", "team:run-1", AttentionKind::Failed));
    assert!(f.core.attention_of("team:run-1").is_some(), "raised on the new target");
    let left = next_change_for(&mut rx, "plan:run-1");
    assert_eq!(left.attention, None, "and the target it left wants nothing");
    assert_eq!(left.retired, Some(raised));
}

// ─── followers ───────────────────────────────────────────────────────────────

#[test]
fn a_session_registered_again_under_one_id_is_followed_afresh() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    // A view keeps the first handle alive, so its watches never close.
    let first = register(&f, "s1");
    first.publish_health(needs_permission());
    next_change_for(&mut rx, &chat_target("s1"));

    // The session comes back under the same id on a new handle.
    f.registry.unregister("s1");
    next_change_for(&mut rx, &chat_target("s1"));
    let second = register(&f, "s1");
    wait_until("the new handle to be followed", || {
        f.core.attention().followed.lock().unwrap().len() >= 2
    });

    second.publish_health(needs_permission());
    let change = next_change_for(&mut rx, &chat_target("s1"));
    assert!(
        change.attention.is_some(),
        "the new handle's prompt reaches the stream, not only the old handle's"
    );
    drop(first);
}

#[test]
fn a_new_request_with_the_same_health_is_still_a_new_edge() {
    let f = fixture();
    let mut rx = f.core.subscribe_attention();
    let h = register(&f, "s1");
    let at = now();
    let waiting = HealthInputs { last_activity_at: at, ..needs_permission() };
    h.publish_health(waiting.clone());
    let first = next_change_for(&mut rx, &chat_target("s1")).edge.expect("raised");

    // One request answered and the next arriving leave the status and the
    // activity stamp alike: only the request id differs, and that lives in the
    // inputs rather than in the derived health.
    h.publish_health(HealthInputs { prompt_request: Some("req-2".into()), ..waiting });
    let change = next_change_for(&mut rx, &chat_target("s1"));
    let second = change.edge.expect("raised");
    assert_ne!(second, first, "a different request is a different alert");
    assert_eq!(change.retired, Some(first));
}

#[test]
fn a_viewed_finish_never_hides_a_failure_of_the_same_turn() {
    let f = fixture();
    let h = register(&f, "s1");
    let turn = HealthInputs {
        process: Process::Live,
        last_activity_at: now() - 60_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    };
    // The turn finished and nobody had read it.
    h.publish_health(turn.clone());
    assert_eq!(
        f.core.attention_of(&chat_target("s1")).map(|a| a.kind),
        Some(AttentionKind::FinishedUnread)
    );
    // The person reads it.
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")), None);

    // Then that same turn is reported failed. It carries the turn's id, so an
    // alert history keyed by id alone would hand it the finish's onset and
    // call it seen.
    h.publish_health(HealthInputs { last_turn_failure: Some("turn failed".into()), ..turn });
    assert_eq!(
        f.core.attention_of(&chat_target("s1")).map(|a| a.kind),
        Some(AttentionKind::Failed),
        "reading the finish is not reading the failure"
    );
}

#[test]
fn a_view_covers_an_alert_that_was_sitting_beneath_a_worse_one() {
    let f = fixture();
    // The failure is current; the stall arrives underneath it and is never
    // the winner, so it would never be recorded as having arrived.
    f.core.publish_external(external("ci", "plan:run-1", AttentionKind::Failed));
    f.core.publish_external(external("slow", "plan:run-1", AttentionKind::Stalled));
    assert_eq!(f.core.attention_of("plan:run-1").unwrap().kind, AttentionKind::Failed);

    // One view deals with what was on the row at the time — both of them.
    f.core.mark_viewed("plan:run-1");
    f.core.clear_external("ci");
    assert_eq!(
        f.core.attention_of("plan:run-1"),
        None,
        "the stall had arrived before the view too, so it was seen as well"
    );
}

#[test]
fn a_prompt_keeps_the_number_of_the_process_that_published_it() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let before = h.health_snapshot().generation;

    // The agent respawns. The old process's prompt is still in the published
    // facts — nothing clears it — so a generation read *now* rather than at
    // publish time would brand that prompt with the successor's number, and
    // the successor's own `req-1` would then be the very same alert.
    h.swap_connection(Arc::new(StubConnection::default()));
    assert_eq!(
        h.health_snapshot().generation,
        before,
        "the facts still belong to the process that published them"
    );

    // Once the successor publishes, its prompt carries its own number.
    h.publish_health(HealthInputs { last_activity_at: now() + 1, ..needs_permission() });
    assert_ne!(h.health_snapshot().generation, before, "a new publish, a new process");
}

#[test]
fn a_prompt_after_a_respawn_is_a_new_alert_even_with_the_same_request_id() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let first = f.core.attention_of(&chat_target("s1")).expect("raised");
    let edge_before = oximux_agents::attention::edge_of(
        &chat_target("s1"),
        &first,
        Some(&h.health()),
        Some(&format!("{}:req-1", h.health_snapshot().generation)),
    );

    // The agent respawns and numbers its first request "req-1" again.
    h.swap_connection(Arc::new(StubConnection::default()));
    h.publish_health(HealthInputs { last_activity_at: now() + 1, ..needs_permission() });
    let after = f.core.attention_of(&chat_target("s1")).expect("raised");
    let edge_after = oximux_agents::attention::edge_of(
        &chat_target("s1"),
        &after,
        Some(&h.health()),
        Some(&format!("{}:req-1", h.health_snapshot().generation)),
    );
    assert_ne!(
        edge_before, edge_after,
        "a consumer that alerts once per edge has to announce the new prompt"
    );
}

#[test]
fn viewing_a_failure_the_host_never_evaluated_still_clears_it() {
    let f = fixture();
    let h = register(&f, "s1");
    // No surface is listening, so no follower ever looked at this session.
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 60_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });

    // The person opens it. The view has to cover a failure that was already
    // there, even though the host is seeing it for the first time now.
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(
        f.core.attention_of(&chat_target("s1")),
        None,
        "the failure had arrived before the view, so the view dealt with it"
    );
    // The failure was observed before the look was placed, so the look covers
    // it — an order, not two timestamps that might tie.
    assert!(f.core.prefs_of(&chat_target("s1")).last_viewed_at.is_some(), "the look was stored");
    let state = f.core.attention().state.lock().unwrap();
    let observed = state
        .onset_for_test(&chat_target("s1"), AttentionKind::Failed, "inc:1")
        .expect("its arrival was recorded");
    let looked = state.viewed_position_for_test(&chat_target("s1")).expect("the look was placed");
    assert!(observed <= looked, "observed at {observed}, looked at {looked}");
}

#[test]
fn a_failure_that_arrives_just_after_a_look_stays_visible_however_often_it_is_read() {
    let f = fixture();
    let h = register(&f, "s1");
    // A look, then a failure, then two reads — all of which can land inside
    // one millisecond. A timestamp cannot order the look and the failure, so
    // the second read used to decide the person had already seen it and the
    // failure vanished until something else changed.
    f.core.mark_viewed(&chat_target("s1"));
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now(),
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });

    for read in 1..=5 {
        assert_eq!(
            f.core.attention_of(&chat_target("s1")).map(|a| a.kind),
            Some(AttentionKind::Failed),
            "read {read}: it arrived after the look, so no number of reads makes it seen"
        );
    }
    assert_eq!(f.core.needs_you().len(), 1, "and it still needs a person");

    // Looking again does deal with it.
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")), None);
}

#[test]
fn a_look_from_before_a_restart_still_covers_what_it_covered() {
    // After a restart the host has observed no looks, so the only record is
    // the stored timestamp. An alert the person had dealt with must stay dealt
    // with rather than coming back with the badge.
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 60_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    f.core.mark_viewed(&chat_target("s1"));
    assert_eq!(f.core.attention_of(&chat_target("s1")), None, "dealt with");

    // A fresh host on the same database: the prefs survive, the observation
    // order does not.
    let restarted = fixture_on(f.db.clone());
    let h2 = restarted.registry.register("s1".into(), Arc::new(StubConnection::default()));
    h2.publish_health(HealthInputs {
        process: Process::Live,
        last_turn_failure: Some("turn failed".into()),
        last_activity_at: now() - 60_000,
        turn_id: Some("inc:1".into()),
        ..Default::default()
    });
    assert_eq!(
        restarted.core.attention_of(&chat_target("s1")),
        None,
        "the look it was dealt with by is still on record"
    );
}

#[test]
fn republishing_the_same_facts_does_not_re_attribute_them() {
    let f = fixture();
    let h = register(&f, "s1");
    h.publish_health(needs_permission());
    let published_by = h.health_snapshot().generation;

    // The agent respawns and something re-announces what the chat already
    // knows — a rebind does exactly this. The facts are unchanged, so they are
    // still the old process's and must keep its number; otherwise the
    // successor's own first prompt, numbered the same by its provider, is the
    // very same alert and nothing announces it.
    h.swap_connection(Arc::new(StubConnection::default()));
    h.publish_health(needs_permission());
    assert_eq!(
        h.health_snapshot().generation,
        published_by,
        "a republish of the same facts is not a new publication"
    );

    // Facts that genuinely change do carry the new process's number.
    h.publish_health(HealthInputs { prompt_request: Some("req-2".into()), ..needs_permission() });
    assert_ne!(h.health_snapshot().generation, published_by);
}
