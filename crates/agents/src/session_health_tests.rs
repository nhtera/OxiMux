use std::sync::Arc;

use super::*;
use crate::health::{HealthInputs, HealthStatus, Liveness, Process};
use crate::session_registry::SessionRegistry;
use crate::thread::StubConnection;

const MIN: i64 = 60_000;

fn registry_with(id: &str) -> (SessionRegistry, Arc<SessionHandle>) {
    let reg = SessionRegistry::new();
    let (stub, _rx, _tx) = StubConnection::new();
    let h = reg.register(id.to_string(), Arc::new(stub));
    (reg, h)
}

fn live_at(at: i64) -> HealthInputs {
    HealthInputs { process: Process::Live, last_activity_at: at, ..Default::default() }
}

#[test]
fn a_new_session_is_unverifiable_until_its_driver_publishes() {
    let (_reg, h) = registry_with("s");
    assert_eq!(h.health().liveness, Liveness::Unverifiable);
    h.publish_health(live_at(now_ms()));
    assert_eq!((h.health().status, h.health().liveness), (HealthStatus::Idle, Liveness::Live));
}

#[test]
fn a_continuation_keeps_the_session_working_for_the_lineage() {
    let (_reg, h) = registry_with("s");
    h.publish_health(HealthInputs { continuations_open: 1, ..live_at(now_ms()) });
    assert_eq!(h.health().status, HealthStatus::Working);
    assert!(h.status().borrow().working, "a child still reacting to its tasks keeps its slot");
    h.publish_health(live_at(now_ms()));
    assert!(!h.status().borrow().working);
}

#[test]
fn the_host_tick_marks_a_quiet_working_session_stalled() {
    let (_reg, h) = registry_with("s");
    let start = now_ms();
    h.publish_health(HealthInputs { turn_active: true, ..live_at(start) });
    assert!(h.health().stalled_since.is_none());
    let changed = h.refresh_health(start + 11 * MIN, ChildActivity::default());
    assert_eq!(changed.and_then(|c| c.stalled_since), Some(start));
}

#[test]
fn a_working_child_resets_its_parents_stall_clock() {
    let (_reg, parent) = registry_with("p");
    let start = now_ms();
    parent.publish_health(HealthInputs { turn_active: true, ..live_at(start) });
    let busy_child = ChildActivity { working: 1, last_activity_at: start + 10 * MIN };
    parent.refresh_health(start + 11 * MIN, busy_child);
    let h = parent.health();
    assert!(h.stalled_since.is_none(), "the child was active a minute ago");
    // An idle child's old activity does not count.
    let idle = ChildActivity { working: 0, last_activity_at: start + 10 * MIN };
    assert!(parent.refresh_health(start + 11 * MIN, idle).and_then(|c| c.stalled_since).is_some());
}

#[test]
fn a_forgiven_sleep_restarts_the_stall_clock() {
    let (_reg, h) = registry_with("s");
    let start = now_ms();
    h.publish_health(HealthInputs { turn_active: true, ..live_at(start) });
    // The laptop slept 30 minutes; the host noticed on waking.
    let woke = start + 30 * MIN;
    h.forgive_until(woke);
    h.refresh_health(woke + MIN, ChildActivity::default());
    assert!(h.health().stalled_since.is_none(), "no burst of stalls on wake");
    h.refresh_health(woke + 11 * MIN, ChildActivity::default());
    assert_eq!(h.health().stalled_since, Some(woke));
}

#[test]
fn child_activity_counts_only_working_children() {
    let reg = SessionRegistry::new();
    let mk = |id: &str| {
        let (stub, _rx, _tx) = StubConnection::new();
        reg.register(id.to_string(), Arc::new(stub))
    };
    let (a, b) = (mk("a"), mk("b"));
    let now = now_ms();
    a.publish_health(HealthInputs { turn_active: true, ..live_at(now - MIN) });
    b.publish_health(live_at(now));
    let act = child_activity([a.as_ref(), b.as_ref()]);
    assert_eq!(act, ChildActivity { working: 1, last_activity_at: now - MIN });
}

/// The compatibility promise for older phones and CLIs: turn brackets feed
/// the host's health and never take a seq in the ring remote clients read.
#[test]
fn turn_brackets_never_enter_the_remote_ring() {
    use crate::thread::ThreadEvent;
    let (_reg, h) = registry_with("s");
    h.ingest(ThreadEvent::Notice("before".into()));
    h.ingest(ThreadEvent::ProviderTurnStarted);
    h.ingest(ThreadEvent::ContinuationEnded);
    h.ingest(ThreadEvent::Notice("after".into()));
    let ring = h.events_since(0);
    assert_eq!(ring.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![1, 2], "no seq spent on a bracket");
    assert!(ring.iter().all(|(_, ev)| !ev.is_turn_bracket()));
}

/// Silence never closes a continuation (a long quiet tool looks the same as
/// a lost closing frame): past the stall window it reads stalled, still
/// working, so the lineage does not park a child that is running.
#[test]
fn a_quiet_continuation_stays_working_and_reads_stalled_on_the_host_tick() {
    let (_reg, h) = registry_with("s");
    let start = now_ms();
    h.publish_health(HealthInputs { continuations_open: 1, ..live_at(start) });
    assert!(h.status().borrow().working);
    h.refresh_health(start + 11 * MIN, ChildActivity::default());
    assert_eq!(h.health().status, HealthStatus::Working);
    assert_eq!(h.health().stalled_since, Some(start));
    assert!(h.status().borrow().working, "the lineage must not park it");
}

/// A spawn marks a child working before its view publishes anything; the
/// host's tick must not undo that from default inputs.
#[test]
fn the_tick_leaves_a_session_alone_until_its_driver_publishes() {
    let (_reg, h) = registry_with("child");
    h.set_working(true);
    assert!(h.refresh_health(now_ms() + 60 * MIN, ChildActivity::default()).is_none());
    assert!(h.status().borrow().working, "the spawn's working flag holds");
}

/// A prompt answered through the host (a phone, the CLI) reads answered at
/// once, though the driver's card only hears from the agent's next frame.
#[test]
fn a_prompt_decided_through_the_host_is_answered_at_once() {
    use crate::health::PromptKind;
    use crate::thread::PermissionDecision;
    let (_reg, h) = registry_with("s");
    h.publish_health(HealthInputs {
        turn_active: true,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("req-1".into()),
        ..live_at(now_ms())
    });
    assert_eq!(h.health().status, HealthStatus::NeedsPermission);
    assert!(h.resolve_permission("req-1", PermissionDecision::Allow { updated_input: serde_json::Value::Null }).unwrap());
    assert_eq!(h.health().status, HealthStatus::Working);
}

/// Waiting on a person is not silence: the answer is the last sign of life,
/// so a long-open prompt does not read stalled as it clears.
#[test]
fn an_answered_prompt_restarts_the_stall_clock() {
    use crate::health::PromptKind;
    let (_reg, h) = registry_with("s");
    let asked = now_ms() - 11 * MIN;
    let waiting = HealthInputs { turn_active: true, prompt: Some(PromptKind::Permission), ..live_at(asked) };
    h.publish_health(waiting.clone());
    assert_eq!(h.health().status, HealthStatus::NeedsPermission);
    h.publish_health(HealthInputs { prompt: None, ..waiting });
    assert_eq!(h.health().status, HealthStatus::Working);
    assert!(h.health().stalled_since.is_none(), "answered just now");
}

/// A continuation that waited on a person past the stall window keeps
/// counting when the answer comes through the host while the driver still
/// shows the card: Working, not Idle, and not stalled.
#[test]
fn a_continuation_answered_through_the_host_keeps_working() {
    use crate::health::PromptKind;
    use crate::thread::PermissionDecision;
    let (_reg, h) = registry_with("s");
    let asked = now_ms() - 11 * MIN;
    h.publish_health(HealthInputs {
        continuations_open: 1,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("req-1".into()),
        ..live_at(asked)
    });
    assert_eq!(h.health().status, HealthStatus::NeedsPermission);
    // A host tick while the card is up changes nothing.
    h.refresh_health(now_ms(), ChildActivity::default());
    assert!(h.resolve_permission("req-1", PermissionDecision::Allow { updated_input: serde_json::Value::Null }).unwrap());
    assert_eq!(h.health().status, HealthStatus::Working);
    assert!(h.health().stalled_since.is_none());
    h.refresh_health(now_ms() + MIN, ChildActivity::default());
    assert_eq!(h.health().status, HealthStatus::Working, "the tick does not take it for stale");
}

/// A new process numbers its requests afresh: an id answered for the old one
/// must neither hide nor refuse the new one's prompt.
#[test]
fn a_respawn_forgets_the_old_processes_decided_requests() {
    use crate::health::PromptKind;
    use crate::thread::PermissionDecision;
    let (reg, h) = registry_with("s");
    let allow = || PermissionDecision::Allow { updated_input: serde_json::Value::Null };
    assert!(h.resolve_permission("3", allow()).unwrap());
    let (respawned, _rx, _tx) = StubConnection::new();
    reg.register("s".into(), Arc::new(respawned));
    h.publish_health(HealthInputs {
        turn_active: true,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("3".into()),
        ..live_at(now_ms())
    });
    assert_eq!(h.health().status, HealthStatus::NeedsPermission, "the new prompt shows");
    assert!(h.resolve_permission("3", allow()).unwrap(), "and can be answered");
}
