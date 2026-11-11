//! One definition of "needs you": the table every surface is now held to.

use super::*;
use crate::health::{ExitProof, Liveness};
use crate::session_prefs::NotifyMode;

/// Noon, in Unix milliseconds. All times below are offsets from it, so a test
/// reads as "this happened five minutes ago".
const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn health(status: HealthStatus) -> SessionHealth {
    SessionHealth {
        status,
        liveness: Liveness::Live,
        limited_until: None,
        stalled_since: None,
        exited_unexpectedly: false,
        reason: None,
        last_activity_at: NOW - MIN,
        pending_unanswered_send: false,
        held_restored: 0,
        turn_id: Some("inc:1".into()),
        exit_seq: None,
    }
}

/// Never looked at.
fn fresh() -> SessionPrefs {
    SessionPrefs::default()
}

/// Looked at `ago` ms before [`NOW`].
fn viewed(ago: i64) -> SessionPrefs {
    SessionPrefs { last_viewed_at: Some(NOW - ago), ..Default::default() }
}

fn kind_of(h: &SessionHealth, p: &SessionPrefs) -> Option<AttentionKind> {
    of(h, p, &[]).map(|a| a.kind)
}

// ─── the status table ────────────────────────────────────────────────────────

#[test]
fn every_status_maps_to_one_kind_or_to_nothing() {
    let cases = [
        (HealthStatus::NeedsPermission, Some(AttentionKind::NeedsPermission)),
        (HealthStatus::NeedsAnswer, Some(AttentionKind::NeedsInput)),
        (HealthStatus::Failed, Some(AttentionKind::Failed)),
        (HealthStatus::Limited, Some(AttentionKind::Limited)),
        // A user's own Stop asks nothing back.
        (HealthStatus::Interrupted, None),
        // Working and talking: nothing wanted.
        (HealthStatus::Working, None),
    ];
    for (status, want) in cases {
        assert_eq!(kind_of(&health(status), &fresh()), want, "{status:?}");
    }
}

#[test]
fn the_priority_order_is_the_one_the_rail_renders() {
    let order = [
        AttentionKind::NeedsPermission,
        AttentionKind::NeedsInput,
        AttentionKind::Failed,
        AttentionKind::Stalled,
        AttentionKind::Limited,
        AttentionKind::FinishedUnread,
    ];
    let got: Vec<u8> = order.iter().map(|k| k.priority()).collect();
    assert_eq!(got, vec![1, 2, 3, 4, 5, 6]);
    for k in order {
        assert_eq!(k.is_needs_you(), k.priority() <= 4, "{k:?} counts iff 1-4");
    }
}

#[test]
fn a_rate_limit_and_an_unread_finish_are_shown_but_never_counted() {
    for kind in [AttentionKind::Limited, AttentionKind::FinishedUnread] {
        assert!(!kind.is_needs_you(), "{kind:?} needs nobody");
        assert!(!kind.is_dismissible(), "{kind:?} clears itself");
    }
}

// ─── finished and unread ─────────────────────────────────────────────────────

#[test]
fn an_idle_session_is_unread_until_it_is_viewed() {
    let h = health(HealthStatus::Idle);
    assert_eq!(kind_of(&h, &fresh()), Some(AttentionKind::FinishedUnread), "never viewed");
    // Viewed after the last output: read.
    assert_eq!(kind_of(&h, &viewed(0)), None);
    // Viewed before it: unread again.
    assert_eq!(kind_of(&h, &viewed(5 * MIN)), Some(AttentionKind::FinishedUnread));
}

#[test]
fn a_session_that_never_ran_a_turn_is_idle_not_unread() {
    let h = SessionHealth { turn_id: None, ..health(HealthStatus::Idle) };
    assert_eq!(kind_of(&h, &fresh()), None, "nothing to read yet");
}

#[test]
fn mark_as_unread_brings_back_a_finish_that_was_read() {
    let h = health(HealthStatus::Idle);
    let p = SessionPrefs { unread_override: true, ..viewed(0) };
    assert_eq!(kind_of(&h, &p), Some(AttentionKind::FinishedUnread));
}

// ─── the failure lifecycle ───────────────────────────────────────────────────

#[test]
fn a_failure_leaves_needs_you_once_it_has_been_seen() {
    let h = SessionHealth {
        reason: Some("agent process exited (code 137)".into()),
        exited_unexpectedly: true,
        liveness: Liveness::Exited(ExitProof::ExitCode(137)),
        ..health(HealthStatus::Failed)
    };
    let raised = of(&h, &fresh(), &[]).expect("raised");
    assert_eq!(raised.kind, AttentionKind::Failed);
    assert_eq!(raised.since, h.last_activity_at, "since is when it last did anything");
    assert_eq!(raised.reason.as_deref(), Some("agent process exited (code 137)"));
    // Viewing the tab (or Dismiss, which writes the same stamp) clears it.
    assert_eq!(kind_of(&h, &viewed(0)), None);
}

#[test]
fn a_new_failure_after_a_dismiss_is_raised_again() {
    let dismissed_at = NOW - 10 * MIN;
    let p = SessionPrefs { last_viewed_at: Some(dismissed_at), ..Default::default() };
    // The retry ran and failed: the session did something after the dismiss.
    let h = SessionHealth { last_activity_at: dismissed_at + MIN, ..health(HealthStatus::Failed) };
    assert_eq!(kind_of(&h, &p), Some(AttentionKind::Failed), "a newer edge is not seen");
}

#[test]
fn a_dismiss_at_the_very_moment_of_the_failure_counts_as_seen() {
    let at = NOW - MIN;
    let h = SessionHealth { last_activity_at: at, ..health(HealthStatus::Failed) };
    let p = SessionPrefs { last_viewed_at: Some(at), ..Default::default() };
    assert_eq!(kind_of(&h, &p), None, "viewed >= since");
}

// ─── the stall lifecycle ─────────────────────────────────────────────────────

fn stalled(since_ago: i64) -> SessionHealth {
    SessionHealth {
        stalled_since: Some(NOW - since_ago),
        last_activity_at: NOW - since_ago,
        ..health(HealthStatus::Working)
    }
}

#[test]
fn a_quiet_working_session_stalls_and_says_since_when() {
    let h = stalled(12 * MIN);
    let a = of(&h, &fresh(), &[]).expect("raised");
    assert_eq!(a.kind, AttentionKind::Stalled);
    assert_eq!(a.since, NOW - 12 * MIN, "the episode the silence was measured from");
}

#[test]
fn an_unanswered_send_is_working_never_stalled() {
    // Twenty minutes quiet on a rate-limit hold: the send is in the gate and
    // the provider has not answered, so the session is working.
    let h = SessionHealth { pending_unanswered_send: true, ..stalled(20 * MIN) };
    assert_eq!(kind_of(&h, &fresh()), None, "quiet by design, not stalled");
}

#[test]
fn output_resuming_ends_a_stall_on_its_own() {
    let h = SessionHealth { stalled_since: None, ..health(HealthStatus::Working) };
    assert_eq!(kind_of(&h, &fresh()), None);
}

#[test]
fn a_stall_episode_dismissed_stays_gone_until_new_silence() {
    let episode = NOW - 12 * MIN;
    let p = SessionPrefs { last_viewed_at: Some(NOW), ..Default::default() };
    let h = SessionHealth { stalled_since: Some(episode), last_activity_at: episode, ..health(HealthStatus::Working) };
    assert_eq!(kind_of(&h, &p), None, "this episode has been seen");
    // Output resumed, then it went quiet again: a new episode, a new alert.
    let next = SessionHealth { stalled_since: Some(NOW + MIN), last_activity_at: NOW + MIN, ..h };
    assert_eq!(kind_of(&next, &p), Some(AttentionKind::Stalled));
}

// ─── prompts are not dismissible ─────────────────────────────────────────────

#[test]
fn viewing_a_session_never_answers_its_prompt() {
    for status in [HealthStatus::NeedsPermission, HealthStatus::NeedsAnswer] {
        let h = health(status);
        assert!(
            kind_of(&h, &viewed(0)).is_some(),
            "{status:?} still needs an answer after a look"
        );
    }
}

#[test]
fn a_rate_limit_is_not_cleared_by_looking_at_it_either() {
    assert_eq!(kind_of(&health(HealthStatus::Limited), &viewed(0)), Some(AttentionKind::Limited));
}

// ─── external items ──────────────────────────────────────────────────────────

fn external(key: &str, kind: AttentionKind, since_ago: i64) -> ExternalAttention {
    ExternalAttention {
        key: key.into(),
        target: "plan:run-1".into(),
        kind,
        reason: format!("{key} says so"),
        since: NOW - since_ago,
    }
}

#[test]
fn a_target_with_no_health_is_judged_by_its_external_items_alone() {
    let items = [external("cockpit:7", AttentionKind::NeedsPermission, MIN)];
    let a = of_external(&fresh(), &items).expect("raised");
    assert_eq!(a.kind, AttentionKind::NeedsPermission);
    assert_eq!(a.reason.as_deref(), Some("cockpit:7 says so"));
}

#[test]
fn the_worst_external_item_wins_and_ties_go_to_the_older_one() {
    let items = [
        external("b", AttentionKind::Stalled, MIN),
        external("a", AttentionKind::Failed, MIN),
        external("c", AttentionKind::Failed, 9 * MIN),
    ];
    let a = of_external(&fresh(), &items).expect("raised");
    assert_eq!(a.kind, AttentionKind::Failed);
    assert_eq!(a.since, NOW - 9 * MIN, "the one waiting longest");
}

#[test]
fn an_external_item_outranks_a_calmer_session_and_loses_a_tie_to_it() {
    // A ticket breaker tripped on a run whose own session is merely idle.
    let idle = SessionHealth { turn_id: None, ..health(HealthStatus::Idle) };
    let breaker = [external("plan:run-1:T-3:breaker", AttentionKind::Failed, MIN)];
    let a = of(&idle, &fresh(), &breaker).expect("raised");
    assert_eq!(a.kind, AttentionKind::Failed);
    assert_eq!(a.reason.as_deref(), Some("plan:run-1:T-3:breaker says so"));

    // Same kind on both sides: the session's own health is the local truth.
    let failed = SessionHealth { reason: Some("turn failed".into()), ..health(HealthStatus::Failed) };
    let a = of(&failed, &fresh(), &breaker).expect("raised");
    assert_eq!(a.reason.as_deref(), Some("turn failed"), "health wins the tie");
}

#[test]
fn a_session_prompt_outranks_an_external_failure() {
    let h = health(HealthStatus::NeedsPermission);
    let items = [external("ci", AttentionKind::Failed, MIN)];
    assert_eq!(kind_of_with(&h, &fresh(), &items), Some(AttentionKind::NeedsPermission));
}

fn kind_of_with(
    h: &SessionHealth,
    p: &SessionPrefs,
    items: &[ExternalAttention],
) -> Option<AttentionKind> {
    of(h, p, items).map(|a| a.kind)
}

#[test]
fn an_external_failure_that_has_been_seen_stops_counting() {
    let items = [external("ci", AttentionKind::Failed, 5 * MIN)];
    assert!(of_external(&fresh(), &items).is_some());
    assert_eq!(of_external(&viewed(0), &items), None, "viewed after it was raised");
    // A later trip of the same breaker is a new edge.
    let again = [external("ci", AttentionKind::Failed, 0)];
    assert!(of_external(&viewed(MIN), &again).is_some());
}

#[test]
fn an_external_prompt_is_never_dismissed_by_a_look() {
    let items = [external("cockpit:7", AttentionKind::NeedsPermission, 5 * MIN)];
    assert!(of_external(&viewed(0), &items).is_some(), "it still has to be answered");
}

#[test]
fn nothing_raised_is_nothing_wanted() {
    assert_eq!(of_external(&fresh(), &[]), None);
    let working = health(HealthStatus::Working);
    assert_eq!(of(&working, &fresh(), &[]), None);
}

// ─── unread, on its own ──────────────────────────────────────────────────────

#[test]
fn unread_is_activity_after_the_last_look_or_the_explicit_mark() {
    let h = health(HealthStatus::Idle);
    assert!(is_unread(&h, &fresh()), "never looked at");
    assert!(is_unread(&h, &viewed(5 * MIN)), "it spoke after the look");
    assert!(!is_unread(&h, &viewed(0)), "nothing since the look");
    assert!(is_unread(&h, &SessionPrefs { unread_override: true, ..viewed(0) }));
}

// ─── presentation, pinned so the surfaces cannot drift ───────────────────────

#[test]
fn each_kind_keeps_its_verb_and_its_tone() {
    let table = [
        (AttentionKind::NeedsPermission, "Needs approval", AttentionTone::Warn),
        (AttentionKind::NeedsInput, "Waiting for input", AttentionTone::Warn),
        (AttentionKind::Failed, "Failed", AttentionTone::Error),
        (AttentionKind::Stalled, "Stalled", AttentionTone::Muted),
        (AttentionKind::Limited, "Limited", AttentionTone::Warn),
        (AttentionKind::FinishedUnread, "Done", AttentionTone::Ok),
    ];
    for (kind, verb, tone) in table {
        assert_eq!(kind.verb(), verb, "{kind:?}");
        assert_eq!(kind.tone(), tone, "{kind:?}");
    }
}

#[test]
fn a_mode_is_not_attention() {
    // Muting changes who hears, never whether the agent is stuck: the rail
    // still has to show it.
    let h = health(HealthStatus::NeedsPermission);
    let p = SessionPrefs { notify_mode: NotifyMode::Muted, ..Default::default() };
    assert_eq!(kind_of(&h, &p), Some(AttentionKind::NeedsPermission));
}

// ─── edge identity ───────────────────────────────────────────────────────────

#[test]
fn one_prompt_is_one_edge_however_often_health_is_re_derived() {
    let h = health(HealthStatus::NeedsPermission);
    let a = of(&h, &fresh(), &[]).expect("raised");
    let first = edge_of("s1", &a, Some(&h), Some("req-1"));
    // The same request, a second snapshot a minute later: the same edge.
    let later = SessionHealth { last_activity_at: NOW + MIN, ..h.clone() };
    let again = of(&later, &fresh(), &[]).expect("raised");
    assert_eq!(edge_of("s1", &again, Some(&later), Some("req-1")), first, "no second banner");
    // A different request is a different edge, so it alerts at once.
    assert_ne!(edge_of("s1", &again, Some(&later), Some("req-2")), first);
}

#[test]
fn a_prompt_with_no_provider_id_falls_back_to_when_it_arrived() {
    let h = health(HealthStatus::NeedsAnswer);
    let a = of(&h, &fresh(), &[]).expect("raised");
    let edge = edge_of("s1", &a, Some(&h), None);
    assert_eq!(edge.id, format!("prompt:{}", NOW - MIN));
    // A later prompt is later activity, so it is a new edge.
    let next = SessionHealth { last_activity_at: NOW, ..h };
    let a2 = of(&next, &fresh(), &[]).expect("raised");
    assert_ne!(edge_of("s1", &a2, Some(&next), None), edge);
}

#[test]
fn a_failure_is_identified_by_the_exit_or_the_turn_that_failed() {
    let exited = SessionHealth {
        exit_seq: Some("inc:3".into()),
        turn_id: Some("inc:9".into()),
        ..health(HealthStatus::Failed)
    };
    let a = of(&exited, &fresh(), &[]).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&exited), None).id, "inc:3", "the exit, when there was one");

    let turn_failed = SessionHealth { exit_seq: None, ..exited };
    let a = of(&turn_failed, &fresh(), &[]).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&turn_failed), None).id, "inc:9", "else the turn");
}

#[test]
fn a_stall_edge_is_its_episode_and_an_external_edge_is_its_key_and_time() {
    let h = stalled(12 * MIN);
    let a = of(&h, &fresh(), &[]).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&h), None).id, format!("stall:{}", NOW - 12 * MIN));

    let items = [external("plan:r:T-3:breaker", AttentionKind::Failed, MIN)];
    let a = of_external(&fresh(), &items).expect("raised");
    let edge = edge_of("plan:r", &a, None, None);
    assert_eq!(edge.id, format!("plan:r:T-3:breaker@{}", NOW - MIN));
    assert_eq!(edge.target, "plan:r");
    // The producer re-publishing the same trip on boot is the same edge.
    assert_eq!(edge_of("plan:r", &a, None, None), edge);
}

#[test]
fn an_edge_carries_the_kind_so_two_alerts_on_one_session_never_merge() {
    let h = health(HealthStatus::NeedsPermission);
    let perm = of(&h, &fresh(), &[]).expect("raised");
    let perm_edge = edge_of("s1", &perm, Some(&h), Some("x"));
    let ext = of_external(&fresh(), &[external("ci", AttentionKind::Failed, MIN)]).expect("raised");
    let ci_edge = edge_of("s1", &ext, Some(&h), Some("x"));
    assert_ne!(perm_edge, ci_edge);
    assert_eq!(perm_edge.kind, AttentionKind::NeedsPermission);
    assert_eq!(ci_edge.kind, AttentionKind::Failed);
}
