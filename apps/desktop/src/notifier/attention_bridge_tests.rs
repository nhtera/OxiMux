//! The bridge's rules: one banner per edge, one banner per root.

use super::*;
use oximux_agents::attention::AttentionSource;

fn attention(kind: AttentionKind, reason: &str) -> Attention {
    Attention {
        kind,
        since: 1_000,
        reason: Some(reason.to_string()),
        source: AttentionSource::Health,
    }
}

fn edge(target: &str, kind: AttentionKind, id: &str) -> AttentionEdge {
    AttentionEdge { target: target.to_string(), kind, id: id.to_string() }
}

/// A session's own alert.
fn raise(target: &str, kind: AttentionKind, id: &str, reason: &str) -> Change {
    Change {
        target: target.to_string(),
        attention: Some(attention(kind, reason)),
        edge: Some(edge(target, kind, id)),
        retired: None,
        root: None,
    }
}

/// The same, for a session hanging under `root`.
fn raise_child(root: &str, target: &str, kind: AttentionKind, id: &str) -> Change {
    Change { root: Some(root.to_string()), ..raise(target, kind, id, "because") }
}

fn quiet(target: &str) -> Change {
    Change {
        target: target.to_string(),
        attention: None,
        edge: None,
        retired: None,
        root: None,
    }
}

fn quiet_child(root: &str, target: &str) -> Change {
    Change { root: Some(root.to_string()), ..quiet(target) }
}

/// The handle inside a `session:<n>` / `root:<n>` identifier. The number
/// itself is minting order and not worth pinning; what matters is that the
/// identifier resolves back to the right target, and that two posts for one
/// target carry the same one.
fn handle(key: &str) -> u64 {
    key.rsplit(':').next().expect("a namespaced key").parse().expect("a handle")
}

/// Assert `key` is `prefix:<handle>` and that the handle names `target`.
#[track_caller]
fn assert_names(state: &BridgeState, key: &str, prefix: &str, target: &str) {
    assert!(key.starts_with(&format!("{prefix}:")), "{key} is not a {prefix} key");
    assert_eq!(
        state.target_of(handle(key)),
        Some(target),
        "{key} should resolve back to {target}"
    );
}

fn posts(actions: &[BridgeAction]) -> Vec<&Post> {
    actions
        .iter()
        .filter_map(|a| match a {
            BridgeAction::Post(post) => Some(post),
            _ => None,
        })
        .collect()
}

#[test]
fn one_edge_banners_once_however_often_it_is_re_derived() {
    // The whole point of edge identity. A health re-snapshot, a tab
    // reopening, a reconnect and a rekey all re-publish the same alert, and
    // the old 30-second window let the fourth one through.
    let mut state = BridgeState::new();
    let change = raise("chat:s1", AttentionKind::NeedsPermission, "req:abc", "rm -rf build/");

    let first = state.on_change(&change);
    assert_eq!(posts(&first).len(), 1, "the first sighting is the banner");
    assert_names(&state, &posts(&first)[0].replace_key, "session", "chat:s1");
    assert_eq!(posts(&first)[0].kind, NotificationKind::NeedsApproval);
    assert_eq!(posts(&first)[0].body, "rm -rf build/");
    assert!(!posts(&first)[0].quiet, "a new alert is worth a sound");

    for _ in 0..4 {
        assert!(state.on_change(&change).is_empty(), "the same edge is not news");
    }
}

#[test]
fn a_different_request_on_one_session_replaces_its_banner_in_place() {
    // Two outstanding permissions on one session are one banner showing the
    // newer: the identifier is the session's, so posting again replaces.
    // Under the old chat policy the second one stayed silent until the window
    // regained focus, which is the behaviour change this PR flags.
    let mut state = BridgeState::new();
    let first = state.on_change(&raise("chat:s1", AttentionKind::NeedsPermission, "req:a", "first?"));
    let first_key = posts(&first)[0].replace_key.clone();
    let second =
        state.on_change(&raise("chat:s1", AttentionKind::NeedsPermission, "req:b", "second?"));

    let posts = posts(&second);
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].replace_key,
        first_key,
        "the same identifier, so the delivered banner is replaced rather than joined"
    );
    assert_eq!(posts[0].body, "second?");
    assert!(!posts[0].quiet, "a different request is a new alert, and sounds like one");
}

#[test]
fn an_answered_alert_takes_its_banner_back() {
    // A permission answered anywhere — the card, the row, a phone, the CLI —
    // ends with the target wanting nothing. A banner left behind would still
    // be asking for something that is already done.
    let mut state = BridgeState::new();
    state.on_change(&raise("chat:s1", AttentionKind::NeedsPermission, "req:a", "rm?"));
    let retired = state.on_change(&quiet("chat:s1"));
    let BridgeAction::Retire { replace_key } = &retired[0] else {
        panic!("expected a retire, got {retired:?}");
    };
    assert_names(&state, replace_key, "session", "chat:s1");
}

#[test]
fn a_boot_snapshot_announces_nothing_it_contains() {
    // Four old alerts outstanding at launch are four rows in the rail and
    // zero banners: a restart must not replay what the person already knows.
    let mut state = BridgeState::new();
    let outstanding = [
        ("chat:s1".to_string(), edge("chat:s1", AttentionKind::Failed, "turn:7")),
        ("cockpit:9".to_string(), edge("cockpit:9", AttentionKind::NeedsPermission, "req:a")),
    ];
    state.seed(outstanding.clone());
    assert!(state.seeded());

    for (target, e) in outstanding {
        let change = Change {
            target: target.clone(),
            attention: Some(attention(e.kind, "still")),
            edge: Some(e),
            retired: None,
            root: None,
        };
        assert!(
            state.on_change(&change).is_empty(),
            "{target} was outstanding at boot, so it is not news"
        );
    }

    // Something that happens *after* the boot still banners.
    let fresh = state.on_change(&raise("chat:s1", AttentionKind::Failed, "turn:8", "boom"));
    assert_eq!(posts(&fresh).len(), 1);
}

#[test]
fn three_children_under_one_root_are_one_banner() {
    // A team run with three children that all need permission is one banner
    // saying so. The rail still lists all three; this is only what the OS
    // pops up.
    let mut state = BridgeState::new();
    let first = state.on_change(&raise_child(
        "team:r1",
        "chat:c1",
        AttentionKind::NeedsPermission,
        "req:1",
    ));
    assert_eq!(posts(&first).len(), 1);
    assert_names(&state, &posts(&first)[0].replace_key, "root", "team:r1");
    assert_eq!(posts(&first)[0].body, "1 agent needs you");
    assert!(!posts(&first)[0].quiet, "the first child is worth a sound");
    assert!(posts(&first)[0].root);

    for (child, req, body) in
        [("chat:c2", "req:2", "2 agents need you"), ("chat:c3", "req:3", "3 agents need you")]
    {
        let more = state.on_change(&raise_child(
            "team:r1",
            child,
            AttentionKind::NeedsPermission,
            req,
        ));
        let posts = posts(&more);
        assert_eq!(posts.len(), 1, "still one banner");
        assert_names(&state, &posts[0].replace_key, "root", "team:r1");
        assert_eq!(posts[0].body, body);
        assert!(!posts[0].quiet, "another agent needing you is news");
    }

    // Children answered one at a time: the count comes down quietly, and the
    // banner goes when the last one is dealt with.
    let two = state.on_change(&quiet_child("team:r1", "chat:c2"));
    assert_eq!(posts(&two)[0].body, "2 agents need you");
    assert!(posts(&two)[0].quiet, "a count going down is not news");

    state.on_change(&quiet_child("team:r1", "chat:c1"));
    let last = state.on_change(&quiet_child("team:r1", "chat:c3"));
    let BridgeAction::Retire { replace_key } = &last[0] else {
        panic!("the last child dealt with ends the banner, got {last:?}");
    };
    assert_names(&state, replace_key, "root", "team:r1");
}

#[test]
fn a_roots_banner_is_titled_by_the_worst_thing_a_child_wants() {
    // It decides which of the person's per-kind toggles gates the banner, so
    // a permission under a root must not be gated by, say, the stall toggle.
    let mut state = BridgeState::new();
    state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::Stalled, "stall:1"));
    let with_permission =
        state.on_change(&raise_child("team:r1", "chat:c2", AttentionKind::NeedsPermission, "r:1"));
    assert_eq!(posts(&with_permission)[0].kind, NotificationKind::NeedsApproval);

    // And with one child left it reads exactly as that child's own banner.
    let back_to_one = state.on_change(&quiet_child("team:r1", "chat:c2"));
    assert_eq!(posts(&back_to_one)[0].kind, NotificationKind::Stalled);
    assert_eq!(posts(&back_to_one)[0].body, "1 agent needs you");
}

#[test]
fn a_childs_finish_does_not_interrupt_anyone() {
    // A root's banner is about who needs a person. A child finishing is in
    // the rail's FINISHED · UNREAD, and three children finishing a turn each
    // is not three banners.
    let mut state = BridgeState::new();
    let finished = state.on_change(&raise_child(
        "team:r1",
        "chat:c1",
        AttentionKind::FinishedUnread,
        "turn:1",
    ));
    assert!(finished.is_empty(), "nothing to post and nothing to take back");

    // The root's own finish is its own business: a root is not a child.
    let root_finished =
        state.on_change(&raise("team:r1", AttentionKind::FinishedUnread, "turn:9", "Finished"));
    assert_eq!(posts(&root_finished)[0].kind, NotificationKind::Done);
    assert_names(&state, &posts(&root_finished)[0].replace_key, "session", "team:r1");
}

#[test]
fn a_roots_own_alert_is_not_counted_among_its_children() {
    // Two banners, two counts: the root's own permission under
    // `session:team:r1`, its children's under `root:team:r1`.
    let mut state = BridgeState::new();
    state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:1"));
    let own = state.on_change(&raise("team:r1", AttentionKind::NeedsPermission, "req:root", "go?"));
    assert_eq!(posts(&own).len(), 1);
    assert_names(&state, &posts(&own)[0].replace_key, "session", "team:r1");

    let more =
        state.on_change(&raise_child("team:r1", "chat:c2", AttentionKind::NeedsPermission, "req:2"));
    assert_eq!(
        posts(&more)[0].body,
        "2 agents need you",
        "the root's own alert is not one of its children"
    );
}

#[test]
fn a_child_re_deriving_its_edge_does_not_re_sound_the_root() {
    // The dedupe has to hold through the root path too: a child's health
    // re-snapshot used to be a fresh "N agents need you" with a sound.
    let mut state = BridgeState::new();
    let change = raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:1");
    state.on_change(&change);
    assert!(state.on_change(&change).is_empty(), "the same child edge says nothing");
}

#[test]
fn a_childs_second_request_updates_the_root_without_double_counting_it() {
    let mut state = BridgeState::new();
    state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:1"));
    let second =
        state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:2"));
    assert_eq!(
        posts(&second)[0].body,
        "1 agent needs you",
        "one child is one child, however many times it asks"
    );
}

#[test]
fn a_click_on_a_roots_banner_goes_to_the_child_that_has_waited_longest() {
    let mut state = BridgeState::new();
    state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:1"));
    state.on_change(&raise_child("team:r1", "chat:c2", AttentionKind::NeedsPermission, "req:2"));
    assert_eq!(state.oldest_waiting("team:r1"), Some("chat:c1"));

    state.on_change(&quiet_child("team:r1", "chat:c1"));
    assert_eq!(
        state.oldest_waiting("team:r1"),
        Some("chat:c2"),
        "once the first is dealt with, the next in line"
    );
    state.on_change(&quiet_child("team:r1", "chat:c2"));
    assert_eq!(state.oldest_waiting("team:r1"), None);
}

#[test]
fn a_target_remembers_a_bounded_number_of_edges() {
    // A long-lived session must not accumulate forever. Anything older than
    // the cap has been superseded many times over.
    let mut state = BridgeState::new();
    for i in 0..(ANNOUNCED_MEMORY + 10) {
        let id = format!("req:{i}");
        let change = raise("chat:s1", AttentionKind::NeedsPermission, &id, "go?");
        assert_eq!(posts(&state.on_change(&change)).len(), 1, "{id} is a new alert");
    }
    // The newest is still remembered…
    let newest = format!("req:{}", ANNOUNCED_MEMORY + 9);
    assert!(
        state
            .on_change(&raise("chat:s1", AttentionKind::NeedsPermission, &newest, "go?"))
            .is_empty()
    );
    // …and the oldest has been forgotten, which is the cost of the bound.
    assert_eq!(
        posts(&state.on_change(&raise("chat:s1", AttentionKind::NeedsPermission, "req:0", "go?")))
            .len(),
        1
    );
}

#[test]
fn nothing_wanted_by_a_target_that_never_banners_says_nothing() {
    // The stream publishes a change whenever a target's attention becomes
    // something else, including for alerts the bridge never posted.
    let mut state = BridgeState::new();
    let retired = state.on_change(&quiet("chat:s1"));
    assert!(
        matches!(retired.as_slice(), [BridgeAction::Retire { .. }]),
        "retiring a banner that is not there is a no-op for the notifier, and \
         cheaper than tracking what is on screen"
    );
    assert!(
        state.on_change(&quiet_child("team:r1", "chat:c1")).is_empty(),
        "but a child that was never waiting does not touch its root's count"
    );
}

#[test]
fn what_the_bridge_is_holding_is_what_a_re_read_has_to_revisit() {
    // After falling behind, the changes that were dropped are gone. Reading
    // only what needs someone *now* misses the opposite case — an answer or a
    // retirement — because such a target is absent from that list, and its
    // banner would stand for the rest of the run.
    let mut state = BridgeState::new();
    state.on_change(&raise("chat:s1", AttentionKind::NeedsPermission, "req:a", "rm?"));
    state.on_change(&raise_child("team:r1", "chat:c1", AttentionKind::NeedsPermission, "req:1"));

    let tracked = state.tracked();
    assert!(tracked.contains(&"chat:s1".to_string()), "a session with a banner up");
    assert!(
        tracked.contains(&"chat:c1".to_string()),
        "and a child counted under a root, whose answer has to reach the root's count"
    );
}

#[test]
fn handles_do_not_start_over_after_a_restart() {
    // macOS keeps delivered banners across a restart. A handle counted from
    // zero would be minted again next run, and an old `session:7` would open
    // whichever session happened to be seventh this time.
    let mut first = BridgeState::new();
    let posts = first.on_change(&raise("chat:s1", AttentionKind::Failed, "turn:1", "boom"));
    let BridgeAction::Post(post) = &posts[0] else {
        panic!("expected a post, got {posts:?}");
    };

    // A fresh process, raising for the same session: its handle must not be
    // the one the delivered banner is still carrying.
    let mut next = BridgeState::new();
    let again = next.on_change(&raise("chat:s1", AttentionKind::Failed, "turn:1", "boom"));
    let BridgeAction::Post(fresh) = &again[0] else {
        panic!("expected a post, got {again:?}");
    };
    assert_ne!(
        handle(&fresh.replace_key),
        handle(&post.replace_key),
        "two runs minting the same handle means an old banner opens whatever \
         holds that number now"
    );
    assert_eq!(
        next.target_of(handle(&post.replace_key)),
        None,
        "and the old handle resolves to nothing, so a stale click opens nothing"
    );
}
