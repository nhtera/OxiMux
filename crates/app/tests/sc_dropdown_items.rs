//! Integration tests for `dropdown_items::resolve`.
//!
//! Lifted out of `dropdown_items.rs` so that source file stays under the
//! 800-LOC fail cap. Resolver is pure-data, so this file does not need a
//! TestAppContext.

use oximux_app::shell::source_control::dropdown_items::{
    DropdownActionKind, DropdownEntry, DropdownInputs, resolve,
};
use oximux_app::shell::source_control::primary_action::{PrimaryActionInputs, UpstreamStatus};

// --- helpers ----------------------------------------------------------------

fn find(entries: &[DropdownEntry], kind: DropdownActionKind) -> &DropdownEntry {
    entries
        .iter()
        .find(|e| matches!(e, DropdownEntry::Item { kind: k, .. } if *k == kind))
        .unwrap_or_else(|| panic!("kind {:?} not found in entries", kind))
}

fn label_of(entry: &DropdownEntry) -> &str {
    match entry {
        DropdownEntry::Item { label, .. } => label,
        DropdownEntry::Separator => panic!("separator has no label"),
    }
}

fn title_of(entry: &DropdownEntry) -> &str {
    match entry {
        DropdownEntry::Item { title, .. } => title,
        DropdownEntry::Separator => panic!("separator has no title"),
    }
}

fn disabled_of(entry: &DropdownEntry) -> bool {
    match entry {
        DropdownEntry::Item { disabled, .. } => *disabled,
        DropdownEntry::Separator => panic!("separator has no disabled state"),
    }
}

/// Construct an `UpstreamStatus` for the resolver inputs.
///
/// `has_upstream=true` produces a populated status with the given ahead/behind.
/// `has_upstream=false` produces `UpstreamStatus::default()` (branch exists
/// but no remote tracking) — `ahead` and `behind` are ignored in that case,
/// matching the resolver's behaviour, and the helper enforces zeros so the
/// caller can't accidentally encode an impossible state.
fn upstream(has_upstream: bool, ahead: u32, behind: u32) -> Option<UpstreamStatus> {
    if has_upstream {
        Some(UpstreamStatus {
            has_upstream: true,
            ahead,
            behind,
        })
    } else {
        assert_eq!(
            (ahead, behind),
            (0, 0),
            "no-upstream state cannot have ahead/behind > 0",
        );
        Some(UpstreamStatus::default())
    }
}

// --- tests -----------------------------------------------------------------

#[test]
fn empty_state_disables_every_commit_and_remote_row() {
    let r = resolve(&DropdownInputs::default());
    assert!(disabled_of(find(&r, DropdownActionKind::Commit)));
    assert!(disabled_of(find(&r, DropdownActionKind::CommitPush)));
    assert!(disabled_of(find(&r, DropdownActionKind::CommitSync)));
    assert!(disabled_of(find(&r, DropdownActionKind::Push)));
    assert!(disabled_of(find(&r, DropdownActionKind::Pull)));
    assert!(disabled_of(find(&r, DropdownActionKind::Sync)));
    assert!(disabled_of(find(&r, DropdownActionKind::Rebase)));
    // Fetch is always available on a quiet repo.
    assert!(!disabled_of(find(&r, DropdownActionKind::Fetch)));
}

#[test]
fn ready_to_commit_enables_commit() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            staged_count: 1,
            has_message: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let commit = find(&r, DropdownActionKind::Commit);
    assert!(!disabled_of(commit));
    assert_eq!(label_of(commit), "Commit");
}

#[test]
fn missing_message_disables_commit_with_specific_reason() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            staged_count: 1,
            has_message: false,
            ..Default::default()
        },
        ..Default::default()
    });
    let commit = find(&r, DropdownActionKind::Commit);
    assert!(disabled_of(commit));
    assert_eq!(title_of(commit), "Enter a commit message to commit");
}

#[test]
fn ahead_only_shows_push_count_and_disables_pull() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 3, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    let push = find(&r, DropdownActionKind::Push);
    assert!(!disabled_of(push));
    assert_eq!(label_of(push), "Push (3)");
    let pull = find(&r, DropdownActionKind::Pull);
    assert!(disabled_of(pull));
    assert_eq!(title_of(pull), "Nothing to pull");
}

#[test]
fn behind_only_shows_pull_count_and_disables_push() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 0, 2),
            ..Default::default()
        },
        ..Default::default()
    });
    let pull = find(&r, DropdownActionKind::Pull);
    assert!(!disabled_of(pull));
    assert_eq!(label_of(pull), "Pull (2)");
    let push = find(&r, DropdownActionKind::Push);
    assert!(disabled_of(push));
    assert_eq!(title_of(push), "Nothing to push");
}

#[test]
fn diverged_shows_arrow_counts_on_sync() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 3, 2),
            ..Default::default()
        },
        ..Default::default()
    });
    let sync = find(&r, DropdownActionKind::Sync);
    assert!(!disabled_of(sync));
    assert_eq!(label_of(sync), "Sync (↓2 ↑3)");
}

#[test]
fn lease_swaps_push_to_force_in_both_slots() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 4, 0),
            ..Default::default()
        },
        force_push_with_lease: true,
        ..Default::default()
    });
    // No `Push` kind when lease is on — both slots emit ForcePush.
    let force_rows: Vec<_> = r
        .iter()
        .filter(|e| matches!(e, DropdownEntry::Item { kind: DropdownActionKind::ForcePush, .. }))
        .collect();
    assert_eq!(force_rows.len(), 2, "expected ForcePush in Push AND Sync slots");
    for row in &force_rows {
        assert_eq!(label_of(row), "Force Push (4)");
    }
    // Commit & Push flips to Commit & Force Push.
    let cp = find(&r, DropdownActionKind::CommitForcePush);
    assert_eq!(label_of(cp), "Commit & Force Push");
    // Plain Sync is gone in lease mode.
    assert!(
        !r.iter()
            .any(|e| matches!(e, DropdownEntry::Item { kind: DropdownActionKind::Sync, .. })),
        "Sync kind should not appear when lease is on"
    );
}

#[test]
fn rebase_uses_base_ref_in_label() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs::default(),
        base_ref: Some("origin/main".to_string()),
        ..Default::default()
    });
    let rebase = find(&r, DropdownActionKind::Rebase);
    assert_eq!(label_of(rebase), "Rebase from origin/main");
    assert!(!disabled_of(rebase));
}

#[test]
fn rebase_falls_back_when_no_base_ref() {
    let r = resolve(&DropdownInputs::default());
    let rebase = find(&r, DropdownActionKind::Rebase);
    assert_eq!(label_of(rebase), "Rebase from Base");
    assert!(disabled_of(rebase));
    assert_eq!(title_of(rebase), "Configure a base ref first");
}

#[test]
fn rebase_blocked_by_unstaged_changes() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            has_unstaged_changes: true,
            ..Default::default()
        },
        base_ref: Some("origin/main".to_string()),
        ..Default::default()
    });
    let rebase = find(&r, DropdownActionKind::Rebase);
    assert!(disabled_of(rebase));
    assert_eq!(title_of(rebase), "Commit or stash changes before rebasing");
}

#[test]
fn in_flight_remote_op_disables_every_network_row() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 2, 2),
            is_remote_operation_active: true,
            ..Default::default()
        },
        ..Default::default()
    });
    assert!(disabled_of(find(&r, DropdownActionKind::Push)));
    assert!(disabled_of(find(&r, DropdownActionKind::Pull)));
    assert!(disabled_of(find(&r, DropdownActionKind::Sync)));
    assert!(disabled_of(find(&r, DropdownActionKind::Fetch)));
    assert!(disabled_of(find(&r, DropdownActionKind::Publish)));
}

#[test]
fn unresolved_conflicts_block_commits_and_remotes() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            staged_count: 1,
            has_message: true,
            has_unresolved_conflicts: true,
            upstream_status: upstream(true, 2, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    let commit = find(&r, DropdownActionKind::Commit);
    assert!(disabled_of(commit));
    assert_eq!(title_of(commit), "Resolve conflicts before committing");
    let push = find(&r, DropdownActionKind::Push);
    assert!(disabled_of(push));
    assert_eq!(title_of(push), "Resolve conflicts before pushing");
}

#[test]
fn pr_rows_always_disabled_with_v1_1_tooltip() {
    let r = resolve(&DropdownInputs::default());
    let cpr = find(&r, DropdownActionKind::CreatePr);
    assert!(disabled_of(cpr));
    assert_eq!(title_of(cpr), "Lands in v1.1");
    let pbpr = find(&r, DropdownActionKind::PushBeforePr);
    assert!(disabled_of(pbpr));
    assert_eq!(title_of(pbpr), "Lands in v1.1");
}

#[test]
fn publish_disabled_when_branch_already_has_upstream() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 0, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    let pub_row = find(&r, DropdownActionKind::Publish);
    assert!(disabled_of(pub_row));
    assert_eq!(title_of(pub_row), "Branch is already published");
}

#[test]
fn publish_enabled_when_branch_has_no_upstream() {
    let r = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(false, 0, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    let pub_row = find(&r, DropdownActionKind::Publish);
    assert!(!disabled_of(pub_row));
    assert_eq!(title_of(pub_row), "Publish this branch to origin");
}

#[test]
fn singular_plural_in_titles() {
    let r_one = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 1, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(title_of(find(&r_one, DropdownActionKind::Push)), "Push 1 commit");
    let r_many = resolve(&DropdownInputs {
        primary: PrimaryActionInputs {
            upstream_status: upstream(true, 3, 0),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(title_of(find(&r_many, DropdownActionKind::Push)), "Push 3 commits");
}

#[test]
fn idempotent_resolution() {
    // Same inputs MUST produce equal Vecs — render layer can short-
    // circuit on equality. Use a non-trivial state to exercise every
    // branch.
    let inputs = DropdownInputs {
        primary: PrimaryActionInputs {
            staged_count: 2,
            has_message: true,
            has_unstaged_changes: true,
            upstream_status: upstream(true, 5, 1),
            ..Default::default()
        },
        base_ref: Some("origin/main".to_string()),
        force_push_with_lease: true,
        is_pr_operation_active: false,
    };
    assert_eq!(resolve(&inputs), resolve(&inputs));
}

#[test]
fn stable_row_order_with_default_inputs() {
    let r = resolve(&DropdownInputs::default());
    let mut kinds = Vec::new();
    for entry in &r {
        if let DropdownEntry::Item { kind, .. } = entry {
            kinds.push(*kind);
        }
    }
    // Default (no lease) — Sync slot is Sync, not ForcePush.
    assert_eq!(
        kinds,
        vec![
            DropdownActionKind::Commit,
            DropdownActionKind::CommitPush,
            DropdownActionKind::CommitSync,
            DropdownActionKind::Push,
            DropdownActionKind::CreatePr,
            DropdownActionKind::PushBeforePr,
            DropdownActionKind::Pull,
            DropdownActionKind::Sync,
            DropdownActionKind::Rebase,
            DropdownActionKind::Fetch,
            DropdownActionKind::Publish,
        ]
    );
    // Separator between Commit-row group and Push-row group.
    assert!(matches!(r[3], DropdownEntry::Separator));
}
