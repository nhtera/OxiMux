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

/// Every alert reads as brand new: the host has published none of them yet.
fn new_edges() -> Unrecorded {
    Unrecorded
}

/// Every alert was raised `ago` ms before [`NOW`], as if the host had
/// published it then.
fn raised_at(ago: i64) -> impl EdgeOnset {
    move |_: &AttentionEdge| Some(NOW - ago)
}

/// Only the edge whose id is `id` was raised, `ago` ms before [`NOW`]; any
/// other edge is one the host has never published, so it is new.
fn raised_only(id: &str, ago: i64) -> impl EdgeOnset + '_ {
    move |edge: &AttentionEdge| (edge.id == id).then_some(NOW - ago)
}

fn kind_of(h: &SessionHealth, p: &SessionPrefs) -> Option<AttentionKind> {
    of("s1", h, p, &[], &new_edges()).map(|a| a.kind)
}

/// What the session wants, given when its alerts were raised.
fn kind_with_onset(h: &SessionHealth, p: &SessionPrefs, onset: &dyn EdgeOnset) -> Option<AttentionKind> {
    of("s1", h, p, &[], onset).map(|a| a.kind)
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
    let raised = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
    assert_eq!(raised.kind, AttentionKind::Failed);
    assert_eq!(raised.since, h.last_activity_at, "since is when it last did anything");
    assert_eq!(raised.reason.as_deref(), Some("agent process exited (code 137)"));
    // Viewing the tab (or Dismiss, which writes the same stamp) clears it, as
    // long as the look came after the alert arose.
    assert_eq!(kind_with_onset(&h, &viewed(0), &raised_at(MIN)), None);
}

#[test]
fn a_new_failure_after_a_dismiss_is_raised_again() {
    let dismissed_at = NOW - 10 * MIN;
    let p = SessionPrefs { last_viewed_at: Some(dismissed_at), ..Default::default() };
    // The retry ran and failed again, so the host saw a new edge after the
    // dismiss: a different turn id makes it a different alert.
    let h = SessionHealth {
        turn_id: Some("inc:2".into()),
        last_activity_at: dismissed_at + MIN,
        ..health(HealthStatus::Failed)
    };
    assert_eq!(
        kind_with_onset(&h, &p, &raised_at(9 * MIN)),
        Some(AttentionKind::Failed),
        "raised after the dismiss"
    );
}

#[test]
fn a_dismiss_at_the_very_moment_the_alert_arose_counts_as_seen() {
    let h = health(HealthStatus::Failed);
    let p = SessionPrefs { last_viewed_at: Some(NOW - MIN), ..Default::default() };
    assert_eq!(kind_with_onset(&h, &p, &raised_at(MIN)), None, "viewed >= raised");
}

/// The bug this whole onset notion exists to stop. A person looks at a quiet
/// working session partway through its silence; the stall is declared later.
/// Comparing their look against `last_activity_at` — when the silence began —
/// would swallow it, and the agent would sit stalled with nothing saying so.
#[test]
fn a_stall_declared_after_the_person_looked_is_still_raised() {
    // Silence began 12 minutes ago; they looked 5 minutes ago; the host first
    // saw the stall 2 minutes ago.
    let h = stalled(12 * MIN);
    assert_eq!(
        kind_with_onset(&h, &viewed(5 * MIN), &raised_at(2 * MIN)),
        Some(AttentionKind::Stalled),
        "the look came before the stall existed"
    );
    // A look after it was declared does clear it.
    assert_eq!(kind_with_onset(&h, &viewed(MIN), &raised_at(2 * MIN)), None);
}

/// The same hole for a process that dies after a look: its `last_activity_at`
/// is the output before it died, which can predate the view.
#[test]
fn a_death_after_the_person_looked_is_still_raised() {
    let h = SessionHealth {
        last_activity_at: NOW - 10 * MIN,
        exited_unexpectedly: true,
        liveness: Liveness::Exited(ExitProof::ExitCode(1)),
        exit_seq: Some("inc:1".into()),
        ..health(HealthStatus::Failed)
    };
    assert_eq!(
        kind_with_onset(&h, &viewed(5 * MIN), &raised_at(MIN)),
        Some(AttentionKind::Failed),
        "it died after they looked away"
    );
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
    let a = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
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
    let this_episode = format!("stall:{episode}");
    let onset = raised_only(&this_episode, MIN);
    assert_eq!(kind_with_onset(&h, &p, &onset), None, "this episode has been seen");
    // Output resumed, then it went quiet again: a new episode is a new edge,
    // which the host has not recorded, so it is new by definition.
    let next = SessionHealth { stalled_since: Some(NOW + MIN), last_activity_at: NOW + MIN, ..h };
    assert_eq!(kind_with_onset(&next, &p, &onset), Some(AttentionKind::Stalled));
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
    let a = of_external("plan:run-1", &fresh(), &items, &new_edges()).expect("raised");
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
    let a = of_external("plan:run-1", &fresh(), &items, &new_edges()).expect("raised");
    assert_eq!(a.kind, AttentionKind::Failed);
    assert_eq!(a.since, NOW - 9 * MIN, "the one waiting longest");
}

#[test]
fn an_external_item_outranks_a_calmer_session_and_loses_a_tie_to_it() {
    // A ticket breaker tripped on a run whose own session is merely idle.
    let idle = SessionHealth { turn_id: None, ..health(HealthStatus::Idle) };
    let breaker = [external("plan:run-1:T-3:breaker", AttentionKind::Failed, MIN)];
    let a = of("plan:run-1", &idle, &fresh(), &breaker, &new_edges()).expect("raised");
    assert_eq!(a.kind, AttentionKind::Failed);
    assert_eq!(a.reason.as_deref(), Some("plan:run-1:T-3:breaker says so"));

    // Same kind on both sides: the session's own health is the local truth.
    let failed = SessionHealth { reason: Some("turn failed".into()), ..health(HealthStatus::Failed) };
    let a = of("plan:run-1", &failed, &fresh(), &breaker, &new_edges()).expect("raised");
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
    of("s1", h, p, items, &new_edges()).map(|a| a.kind)
}

#[test]
fn an_external_failure_that_has_been_seen_stops_counting() {
    let items = [external("ci", AttentionKind::Failed, 5 * MIN)];
    assert!(of_external("plan:run-1", &fresh(), &items, &new_edges()).is_some());
    assert_eq!(of_external("plan:run-1", &viewed(0), &items, &raised_at(MIN)), None, "viewed after it was raised");
    // A later trip of the same breaker is a new edge.
    let again = [external("ci", AttentionKind::Failed, 0)];
    assert!(of_external("plan:run-1", &viewed(MIN), &again, &raised_at(0)).is_some());
}

#[test]
fn an_external_prompt_is_never_dismissed_by_a_look() {
    let items = [external("cockpit:7", AttentionKind::NeedsPermission, 5 * MIN)];
    assert!(of_external("plan:run-1", &viewed(0), &items, &raised_at(MIN)).is_some(), "it still has to be answered");
}

#[test]
fn nothing_raised_is_nothing_wanted() {
    assert_eq!(of_external("plan:run-1", &fresh(), &[], &new_edges()), None);
    let working = health(HealthStatus::Working);
    assert_eq!(of("s1", &working, &fresh(), &[], &new_edges()), None);
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
    let a = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
    let first = edge_of("s1", &a, Some(&h), Some("req-1"));
    // The same request, a second snapshot a minute later: the same edge.
    let later = SessionHealth { last_activity_at: NOW + MIN, ..h.clone() };
    let again = of("s1", &later, &fresh(), &[], &new_edges()).expect("raised");
    assert_eq!(edge_of("s1", &again, Some(&later), Some("req-1")), first, "no second banner");
    // A different request is a different edge, so it alerts at once.
    assert_ne!(edge_of("s1", &again, Some(&later), Some("req-2")), first);
}

#[test]
fn a_prompt_with_no_provider_id_falls_back_to_when_it_arrived() {
    let h = health(HealthStatus::NeedsAnswer);
    let a = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
    let edge = edge_of("s1", &a, Some(&h), None);
    assert_eq!(edge.id, format!("prompt:{}", NOW - MIN));
    // A later prompt is later activity, so it is a new edge.
    let next = SessionHealth { last_activity_at: NOW, ..h };
    let a2 = of("s1", &next, &fresh(), &[], &new_edges()).expect("raised");
    assert_ne!(edge_of("s1", &a2, Some(&next), None), edge);
}

#[test]
fn a_failure_is_identified_by_the_exit_or_the_turn_that_failed() {
    let exited = SessionHealth {
        exit_seq: Some("inc:3".into()),
        turn_id: Some("inc:9".into()),
        ..health(HealthStatus::Failed)
    };
    let a = of("s1", &exited, &fresh(), &[], &new_edges()).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&exited), None).id, "inc:3", "the exit, when there was one");

    let turn_failed = SessionHealth { exit_seq: None, ..exited };
    let a = of("s1", &turn_failed, &fresh(), &[], &new_edges()).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&turn_failed), None).id, "inc:9", "else the turn");
}

#[test]
fn a_stall_edge_is_its_episode_and_an_external_edge_is_its_key_and_time() {
    let h = stalled(12 * MIN);
    let a = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
    assert_eq!(edge_of("s1", &a, Some(&h), None).id, format!("stall:{}", NOW - 12 * MIN));

    let items = [external("plan:r:T-3:breaker", AttentionKind::Failed, MIN)];
    let a = of_external("plan:run-1", &fresh(), &items, &new_edges()).expect("raised");
    let edge = edge_of("plan:r", &a, None, None);
    assert_eq!(edge.id, format!("plan:r:T-3:breaker@{}", NOW - MIN));
    assert_eq!(edge.target, "plan:r");
    // The producer re-publishing the same trip on boot is the same edge.
    assert_eq!(edge_of("plan:r", &a, None, None), edge);
}

#[test]
fn an_edge_carries_the_kind_so_two_alerts_on_one_session_never_merge() {
    let h = health(HealthStatus::NeedsPermission);
    let perm = of("s1", &h, &fresh(), &[], &new_edges()).expect("raised");
    let perm_edge = edge_of("s1", &perm, Some(&h), Some("x"));
    let ext =
        of_external("s1", &fresh(), &[external("ci", AttentionKind::Failed, MIN)], &new_edges())
            .expect("raised");
    let ci_edge = edge_of("s1", &ext, Some(&h), Some("x"));
    assert_ne!(perm_edge, ci_edge);
    assert_eq!(perm_edge.kind, AttentionKind::NeedsPermission);
    assert_eq!(ci_edge.kind, AttentionKind::Failed);
}

#[test]
fn an_external_unread_finish_is_read_by_looking_at_its_target() {
    // A producer can raise an unread finish, and nothing but a look reads it:
    // its producer has no idea whether the person opened the tab.
    let items = [external("run", AttentionKind::FinishedUnread, 5 * MIN)];
    assert!(of_external("plan:run-1", &fresh(), &items, &new_edges()).is_some());
    assert_eq!(
        of_external("plan:run-1", &viewed(0), &items, &new_edges()),
        None,
        "looked at it after it finished"
    );
    assert_eq!(
        of_external("plan:run-1", &viewed(9 * MIN), &items, &new_edges()).map(|a| a.kind),
        Some(AttentionKind::FinishedUnread),
        "it finished after they looked"
    );
    // Marking it unread brings it back.
    let marked = SessionPrefs { unread_override: true, ..viewed(0) };
    assert!(of_external("plan:run-1", &marked, &items, &new_edges()).is_some());
}

#[test]
fn a_rate_limit_raised_externally_is_not_read_by_looking() {
    // Unlike a finish, seeing a rate limit does not deal with it.
    let items = [external("provider", AttentionKind::Limited, 5 * MIN)];
    assert!(
        of_external("plan:run-1", &viewed(0), &items, &raised_at(MIN)).is_some(),
        "still limited"
    );
}

#[test]
fn two_external_items_tied_on_kind_and_time_are_picked_by_their_key() {
    let at = NOW - MIN;
    let item = |key: &str| ExternalAttention {
        key: key.into(),
        target: "plan:run-1".into(),
        kind: AttentionKind::Failed,
        reason: format!("{key} says so"),
        since: at,
    };
    // Whichever order the producer map iterated, the answer is the same.
    let one = [item("b"), item("a")];
    let other = [item("a"), item("b")];
    let pick = |items: &[ExternalAttention]| {
        of_external("plan:run-1", &fresh(), items, &new_edges()).unwrap().reason
    };
    assert_eq!(pick(&one), pick(&other), "a HashMap's order must not pick the reason");
    assert_eq!(pick(&one).as_deref(), Some("a says so"));
}
