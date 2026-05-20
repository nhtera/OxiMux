//! Priority-ladder tests for `resolve_primary_action`. One test per row of
//! the design-doc table — pure data in, `PrimaryAction` out, no GPUI / tokio.

use oximux_app::shell::source_control::primary_action::{
    PrimaryAction, PrimaryActionInputs, PrimaryActionKind, RemoteOpKind, UpstreamStatus,
    resolve_primary_action,
};

fn clean_with_upstream(ahead: u32, behind: u32) -> PrimaryActionInputs {
    PrimaryActionInputs {
        upstream_status: Some(UpstreamStatus {
            has_upstream: true,
            ahead,
            behind,
        }),
        ..PrimaryActionInputs::default()
    }
}

fn unpack(a: &PrimaryAction) -> (PrimaryActionKind, &str, bool) {
    (a.kind, a.label.as_str(), a.disabled)
}

#[test]
fn committing_in_flight_locks_primary() {
    let inputs = PrimaryActionInputs {
        is_committing: true,
        staged_count: 3,
        has_message: true,
        upstream_status: Some(UpstreamStatus::default()),
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", true));
    assert_eq!(act.title, "Commit in progress…");
}

#[test]
fn remote_op_in_flight_mirrors_user_choice_when_different_from_natural() {
    // Natural primary would be Push (ahead 3, behind 0); user triggered Sync
    // from the dropdown. The primary should mirror Sync until it finishes.
    let inputs = PrimaryActionInputs {
        is_remote_operation_active: true,
        in_flight_remote_op_kind: Some(RemoteOpKind::Sync),
        ..clean_with_upstream(3, 0)
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Sync);
    assert_eq!(act.label, "Sync");
    assert!(act.disabled);
    assert_eq!(act.title, "Sync in progress…");
}

#[test]
fn remote_op_in_flight_keeps_natural_label_when_matches() {
    // Push in flight + ahead 3 → keep natural "Push" label so detail survives.
    let inputs = PrimaryActionInputs {
        is_remote_operation_active: true,
        in_flight_remote_op_kind: Some(RemoteOpKind::Push),
        ..clean_with_upstream(3, 0)
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Push);
    assert!(act.disabled);
    assert_eq!(act.title, "Remote operation in progress…");
}

#[test]
fn fetch_in_flight_leaves_primary_label_alone() {
    let inputs = PrimaryActionInputs {
        is_remote_operation_active: true,
        in_flight_remote_op_kind: Some(RemoteOpKind::Fetch),
        ..clean_with_upstream(2, 0)
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Push);
    assert!(act.disabled);
}

#[test]
fn unresolved_conflicts_block_commit_with_specific_tooltip() {
    let inputs = PrimaryActionInputs {
        has_unresolved_conflicts: true,
        staged_count: 1,
        has_message: true,
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", true));
    assert_eq!(act.title, "Resolve conflicts before committing");
}

#[test]
fn partially_staged_overrides_commit_with_stage_all() {
    // A file in both staged + unstaged sections (partial hunk stage) flips the
    // primary to Stage All so commit hooks don't corrupt the partial-stash on
    // restore. Beats both "staged + message → Commit" and
    // "staged no message → disabled Commit".
    let inputs = PrimaryActionInputs {
        staged_count: 3,
        has_unstaged_changes: true,
        has_partially_staged_changes: true,
        has_message: true,
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Stage, "Stage All", false));
    assert_eq!(
        act.title,
        "Stage all changes before committing partially staged files"
    );
}

#[test]
fn staged_plus_message_enables_commit() {
    let inputs = PrimaryActionInputs {
        staged_count: 2,
        has_message: true,
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", false));
    assert_eq!(act.title, "Commit staged changes");
}

#[test]
fn staged_without_message_disables_commit_with_hint() {
    let inputs = PrimaryActionInputs {
        staged_count: 1,
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", true));
    assert_eq!(act.title, "Enter a commit message to commit");
}

#[test]
fn dirty_with_no_staged_surfaces_stage_all() {
    let inputs = PrimaryActionInputs {
        has_unstaged_changes: true,
        upstream_status: Some(UpstreamStatus {
            has_upstream: true,
            ahead: 5,
            behind: 0,
        }),
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Stage, "Stage All", false));
}

#[test]
fn upstream_status_none_keeps_stable_disabled_commit_frame() {
    // Worktree clean, no staged, no message, upstream not resolved yet → must
    // be a disabled Commit so the button doesn't flash "Publish Branch" on
    // first paint.
    let inputs = PrimaryActionInputs::default();
    let act = resolve_primary_action(&inputs);
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", true));
    assert_eq!(act.title, "Stage at least one file to commit");
}

#[test]
fn unpublished_branch_promotes_publish() {
    let inputs = PrimaryActionInputs {
        upstream_status: Some(UpstreamStatus {
            has_upstream: false,
            ahead: 0,
            behind: 0,
        }),
        ..PrimaryActionInputs::default()
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(
        unpack(&act),
        (PrimaryActionKind::Publish, "Publish Branch", false)
    );
}

#[test]
fn diverged_promotes_sync() {
    let act = resolve_primary_action(&clean_with_upstream(2, 3));
    assert_eq!(unpack(&act), (PrimaryActionKind::Sync, "Sync", false));
    assert_eq!(act.title, "Pull 3, push 2");
}

#[test]
fn behind_promotes_pull_with_count() {
    let act = resolve_primary_action(&clean_with_upstream(0, 4));
    assert_eq!(unpack(&act), (PrimaryActionKind::Pull, "Pull", false));
    assert_eq!(act.title, "Pull 4 commits");

    let act_one = resolve_primary_action(&clean_with_upstream(0, 1));
    assert_eq!(act_one.title, "Pull 1 commit");
}

#[test]
fn ahead_promotes_push_with_count() {
    let act = resolve_primary_action(&clean_with_upstream(7, 0));
    assert_eq!(unpack(&act), (PrimaryActionKind::Push, "Push", false));
    assert_eq!(act.title, "Push 7 commits");

    let act_one = resolve_primary_action(&clean_with_upstream(1, 0));
    assert_eq!(act_one.title, "Push 1 commit");
}

#[test]
fn clean_tree_in_sync_disables_commit_with_message() {
    let act = resolve_primary_action(&clean_with_upstream(0, 0));
    assert_eq!(unpack(&act), (PrimaryActionKind::Commit, "Commit", true));
    assert_eq!(act.title, "Nothing to commit. Branch is up to date.");
}

#[test]
fn conflicts_during_fetch_show_conflict_tooltip() {
    // Fetch isn't primary-eligible (`primary_kind_for_remote(Fetch) == None`),
    // so the mirror branch never fires and the conflict tooltip wins over
    // the generic "remote in progress" copy.
    let inputs = PrimaryActionInputs {
        is_remote_operation_active: true,
        in_flight_remote_op_kind: Some(RemoteOpKind::Fetch),
        has_unresolved_conflicts: true,
        ..clean_with_upstream(0, 2)
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Commit);
    assert!(act.disabled);
    assert_eq!(act.title, "Resolve conflicts before committing");
}

#[test]
fn remote_op_mirror_wins_over_conflict_tooltip_when_kind_differs() {
    // User picked Sync from the dropdown while natural primary would be
    // Push (ahead-only). The mirror branch fires first and overwrites the
    // conflict tooltip with the in-flight label.
    let inputs = PrimaryActionInputs {
        is_remote_operation_active: true,
        in_flight_remote_op_kind: Some(RemoteOpKind::Sync),
        has_unresolved_conflicts: true,
        ..clean_with_upstream(3, 0)
    };
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Sync);
    assert_eq!(act.title, "Sync in progress…");
}
