//! The outcome-to-UI half of `Merge into <default>`.
//!
//! `MergeOutcome::AutoStashed` **wraps** another variant instead of sitting
//! beside it, so the real shape of a merge result is a cross-product —
//! {no stash, stash popped, stash stranded} × {FastForward, Merged,
//! AlreadyUpToDate, Conflicted} — not a five-row table. A caller that matches
//! `AutoStashed` as a peer of the others reports "merged" for a merge that did
//! nothing, and loses the pointer to the user's stashed work while doing it.
//!
//! These tests walk that product. The real-git half (that git actually produces
//! each of these shapes) lives in
//! `crates/worktree-ops/tests/workspace_merge.rs`; this file asserts what the
//! user is told about each one.

use std::path::PathBuf;

use oximux_app::shell::merge_notices::{
    NoticeReason, StashNotice, acknowledge, for_project, load, record,
};
use oximux_app::shell::merge_ops::{MergeReport, MergeReportKind, report_for};
use oximux_core::{MergeOutcome, StashRef};
use oximux_storage::{SettingsRepo, open_memory};

const BRANCH: &str = "oximux/fix-login";
const DEFAULT: &str = "main";

fn report(outcome: MergeOutcome) -> MergeReport {
    report_for(outcome, BRANCH, DEFAULT)
}

/// Wrap an outcome the way a dirty project root does.
fn auto_stashed(inner: MergeOutcome, pop_failed: bool) -> MergeOutcome {
    MergeOutcome::AutoStashed {
        stash_ref: StashRef { index: 0 },
        inner: Box::new(inner),
        pop_failed,
    }
}

// ------------------------------------------------------- the clean-root column

#[test]
fn a_fast_forward_reads_as_landed_and_names_both_branches() {
    let r = report(MergeOutcome::FastForward);
    assert_eq!(r.kind, MergeReportKind::Landed);
    assert!(r.headline.contains(BRANCH), "{}", r.headline);
    assert!(r.headline.contains(DEFAULT), "{}", r.headline);
    assert_eq!(r.stranded, None);
    assert!(!r.stash_restored);
}

#[test]
fn a_true_merge_reads_the_same_as_a_fast_forward() {
    assert_eq!(report(MergeOutcome::Merged).kind, MergeReportKind::Landed);
}

#[test]
fn already_up_to_date_never_reads_as_a_successful_merge() {
    let r = report(MergeOutcome::AlreadyUpToDate);
    assert_eq!(r.kind, MergeReportKind::NothingToMerge);
    assert!(
        r.headline.to_lowercase().contains("nothing to merge"),
        "{}",
        r.headline
    );
    assert!(
        !r.headline.contains("Merged \u{201c}"),
        "a no-op must not claim a merge: {}",
        r.headline
    );
}

// ------------------------------------------------------- the dirty-root column

#[test]
fn a_popped_stash_lands_the_merge_and_says_the_changes_came_back() {
    let r = report(auto_stashed(MergeOutcome::FastForward, false));
    assert_eq!(r.kind, MergeReportKind::Landed);
    assert_eq!(r.stranded, None, "a popped stash strands nothing");
    assert!(r.stash_restored);
    assert!(r.headline.contains("put back"), "{}", r.headline);
}

/// **The trap.** `AutoStashed { inner: AlreadyUpToDate }` is what a dirty root
/// plus an already-merged branch produces, and a flat match reports it as a
/// successful merge.
#[test]
fn a_stashed_no_op_still_reads_as_nothing_to_merge() {
    let r = report(auto_stashed(MergeOutcome::AlreadyUpToDate, false));
    assert_eq!(
        r.kind,
        MergeReportKind::NothingToMerge,
        "the wrapper must be peeled before the inner variant is read"
    );
    assert!(
        r.headline.to_lowercase().contains("nothing to merge"),
        "{}",
        r.headline
    );
}

/// The merge **succeeded**; only the pop did not. Both halves are true at once
/// and the report has to carry both.
#[test]
fn a_failed_pop_reports_the_merge_as_landed_and_strands_the_stash() {
    let r = report(auto_stashed(MergeOutcome::Merged, true));
    assert_eq!(
        r.kind,
        MergeReportKind::Landed,
        "a failed pop does not un-merge the branch"
    );
    assert_eq!(r.stranded, Some(NoticeReason::PopFailed));
    assert!(!r.stash_restored);
    assert!(
        !r.headline.contains("put back"),
        "nothing was put back: {}",
        r.headline
    );
}

/// A failed pop on a no-op merge: both the peel AND the strand have to be right
/// at once.
#[test]
fn a_failed_pop_on_a_no_op_merge_strands_the_stash_without_claiming_a_merge() {
    let r = report(auto_stashed(MergeOutcome::AlreadyUpToDate, true));
    assert_eq!(r.kind, MergeReportKind::NothingToMerge);
    assert_eq!(r.stranded, Some(NoticeReason::PopFailed));
}

// ------------------------------------------------------------------- conflicts

#[test]
fn a_clean_root_conflict_lists_the_paths_and_strands_nothing() {
    let r = report(MergeOutcome::Conflicted {
        conflicts: vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
        auto_stash: None,
    });
    assert_eq!(r.kind, MergeReportKind::Conflicted);
    assert_eq!(r.conflicts.len(), 2);
    assert!(r.headline.contains('2'), "{}", r.headline);
    assert!(r.headline.contains("files"), "{}", r.headline);
    assert_eq!(r.stranded, None);
}

#[test]
fn one_conflict_is_not_pluralised_and_is_named() {
    let r = report(MergeOutcome::Conflicted {
        conflicts: vec![PathBuf::from("src/a.rs")],
        auto_stash: None,
    });
    assert!(r.headline.contains("1 file"), "{}", r.headline);
    assert!(!r.headline.contains("1 files"), "{}", r.headline);
    assert!(
        r.headline.contains("src/a.rs"),
        "the phase asks for the conflicting paths to be NAMED, not counted: {}",
        r.headline
    );
}

/// A long conflict list is capped, and says how many it left out. A toast is
/// one line; the full list is in the panel this routes to.
#[test]
fn a_long_conflict_list_is_capped_with_a_remainder() {
    let conflicts: Vec<PathBuf> = (0..7).map(|i| PathBuf::from(format!("f{i}.rs"))).collect();
    let r = report(MergeOutcome::Conflicted {
        conflicts,
        auto_stash: None,
    });
    assert!(r.headline.contains("f0.rs"), "{}", r.headline);
    assert!(r.headline.contains("and 4 more"), "{}", r.headline);
    assert!(!r.headline.contains("f5.rs"), "{}", r.headline);
}

/// A failed stash pop leaves conflict markers in the user's files even though
/// the merge itself landed. Reporting that as a plain success is the difference
/// between a user who fixes it now and one who finds `<<<<<<<` next week.
#[test]
fn a_failed_pop_reports_the_tree_as_conflicted_not_merely_merged() {
    let r = report(auto_stashed(MergeOutcome::Merged, true));
    assert!(
        r.tree_has_conflicts,
        "git stash pop writes markers before giving up"
    );
    assert!(
        r.headline.contains("conflicts"),
        "the headline must say so: {}",
        r.headline
    );
    // ...and a clean landing does not raise the alarm.
    assert!(!report(MergeOutcome::Merged).tree_has_conflicts);
    assert!(!report(auto_stashed(MergeOutcome::FastForward, false)).tree_has_conflicts);
}

/// The disagreeing shape from `every_outcome_holding_an_unpopped_stash_sets_stranded`,
/// asserted on its own so the reason it matters is legible: the wrapper holds
/// the ref, the inner conflict does not.
#[test]
fn a_wrapper_stash_over_a_conflict_without_one_is_still_stranded() {
    let r = report(auto_stashed(
        MergeOutcome::Conflicted {
            conflicts: vec![PathBuf::from("a.rs")],
            auto_stash: None,
        },
        false,
    ));
    assert_eq!(r.kind, MergeReportKind::Conflicted);
    assert_eq!(r.stranded, Some(NoticeReason::Conflicted));
}

/// The single point of data loss: a conflicted merge on a dirty root leaves the
/// stash on the stack, and the report is what tells the host to write it down.
#[test]
fn a_dirty_root_conflict_strands_the_stash() {
    let r = report(MergeOutcome::Conflicted {
        conflicts: vec![PathBuf::from("src/a.rs")],
        auto_stash: Some(StashRef { index: 0 }),
    });
    assert_eq!(r.kind, MergeReportKind::Conflicted);
    assert_eq!(r.stranded, Some(NoticeReason::Conflicted));
}

/// Every shape that carries an un-popped stash must set `stranded`. This is the
/// release-blocking property stated in the phase: a path where the stash
/// reaches only a transient toast is a bug, not a polish item.
#[test]
fn every_outcome_holding_an_unpopped_stash_sets_stranded() {
    let holding: Vec<MergeOutcome> = vec![
        auto_stashed(MergeOutcome::FastForward, true),
        auto_stashed(MergeOutcome::Merged, true),
        auto_stashed(MergeOutcome::AlreadyUpToDate, true),
        MergeOutcome::Conflicted {
            conflicts: vec![PathBuf::from("a")],
            auto_stash: Some(StashRef { index: 3 }),
        },
        // The shape where the two sources DISAGREE: the wrapper holds the ref
        // and the inner conflict does not. Reading only the variant's own
        // `auto_stash` here would answer "nothing stranded" and drop the
        // pointer. Not producible by `merge_branch` today; `peel` is
        // deliberately recursive, so the mapping must not depend on that.
        auto_stashed(
            MergeOutcome::Conflicted {
                conflicts: vec![PathBuf::from("a")],
                auto_stash: None,
            },
            false,
        ),
    ];
    for outcome in holding {
        let label = format!("{outcome:?}");
        assert!(
            report(outcome).stranded.is_some(),
            "an un-popped stash must be recorded: {label}"
        );
    }

    // ...and every shape that does NOT hold one must not raise a false alarm.
    let released: Vec<MergeOutcome> = vec![
        MergeOutcome::FastForward,
        MergeOutcome::Merged,
        MergeOutcome::AlreadyUpToDate,
        auto_stashed(MergeOutcome::FastForward, false),
        auto_stashed(MergeOutcome::AlreadyUpToDate, false),
        MergeOutcome::Conflicted {
            conflicts: vec![PathBuf::from("a")],
            auto_stash: None,
        },
    ];
    for outcome in released {
        let label = format!("{outcome:?}");
        assert!(
            report(outcome).stranded.is_none(),
            "nothing is stranded here: {label}"
        );
    }
}

/// A nested wrapper is not a shape `merge_branch` produces today, but the type
/// permits it and a partial peel would report the inner merge while dropping
/// the outer stash.
#[test]
fn a_nested_wrapper_is_peeled_all_the_way_down() {
    let nested = auto_stashed(auto_stashed(MergeOutcome::AlreadyUpToDate, true), false);
    let r = report(nested);
    assert_eq!(r.kind, MergeReportKind::NothingToMerge);
    assert_eq!(r.stranded, Some(NoticeReason::PopFailed));
}

// -------------------------------------------------- the notice store round-trip

fn store() -> SettingsRepo {
    SettingsRepo::new(open_memory().expect("open memory"))
}

/// The durability guarantee, driven through the real store: the pointer to a
/// stranded stash outlives the toast that first announced it.
#[test]
fn a_stranded_stash_is_readable_again_after_the_toast_is_gone() {
    let repo = store();
    let r = report(MergeOutcome::Conflicted {
        conflicts: vec![PathBuf::from("a.rs")],
        auto_stash: Some(StashRef { index: 0 }),
    });
    let reason = r.stranded.expect("stranded");
    record(
        &repo,
        StashNotice::new("p1", "/repos/app", BRANCH, "oximux: auto-stash before merge", reason),
    );

    // A new read, as a later session would do it.
    let pending = for_project(&load(&repo), "p1");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].branch, BRANCH);
    assert_eq!(pending[0].reason, NoticeReason::Conflicted);
    assert_eq!(
        pending[0].stash_message,
        oximux_git::AUTO_STASH_MESSAGE,
        "the notice must remember the message git actually wrote, or it can \
         never be resolved"
    );

    acknowledge(&repo, &pending[0].id);
    assert!(for_project(&load(&repo), "p1").is_empty());
}

/// The failed-merge notice, driven through the store the way the host writes it.
///
/// This is the case `crates/worktree-ops/tests/workspace_merge.rs` cannot force
/// end to end (the pop would have to fail inside a single `merge_branch` call),
/// so the half that IS testable is asserted here: that the reason survives the
/// round-trip and says the one thing that is true of it and of neither other
/// reason — nothing was merged, and the changes are still safe.
#[test]
fn a_failed_merge_notice_survives_the_store_and_claims_no_merge() {
    let repo = store();
    record(
        &repo,
        StashNotice::new(
            "p1",
            "/repos/app",
            BRANCH,
            oximux_git::AUTO_STASH_MESSAGE,
            NoticeReason::MergeFailed,
        ),
    );

    let pending = for_project(&load(&repo), "p1");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reason, NoticeReason::MergeFailed);
    let msg = pending[0].message();
    assert!(msg.contains("Nothing was merged"), "{msg}");
    assert!(msg.contains("still stashed"), "{msg}");
    assert!(
        !msg.contains("merged successfully"),
        "nothing landed: {msg}"
    );
}

/// The notice is what carries the host details, and it never carries an index.
///
/// The companion half — that nothing on the merge path writes them to the
/// workspace row's `comment`, which rides a lower remote read gate — is a
/// property of the code rather than of this value, and is asserted by
/// `no_merge_path_writes_to_the_workspace_row` below.
#[test]
fn the_notice_carries_the_host_details_and_never_an_index() {
    let n = StashNotice::new(
        "p1",
        "/repos/app",
        BRANCH,
        oximux_git::AUTO_STASH_MESSAGE,
        NoticeReason::PopFailed,
    );
    assert_eq!(n.project_root, "/repos/app");
    assert_eq!(n.branch, BRANCH);
    // ...and never an index, which renumbers.
    let json = serde_json::to_string(&n).expect("serialize");
    assert!(
        !json.contains("stash@"),
        "a stored stash@{{N}} is worse than no pointer: {json}"
    );
    assert!(!json.contains("index"), "{json}");
}

/// `WorktreeProgressWire` rides the *coordination* read gate precisely because
/// `comment` and `phase` carry "only an id and agent-authored text, never the
/// host paths and branch names". A merge writing a repository path or a branch
/// name there would push exactly that across a gate designed to withhold it.
///
/// Asserted where the property actually lives — the source of the merge paths —
/// because no value this test could construct would show it.
#[test]
fn no_merge_path_writes_to_the_workspace_row() {
    let sources = [
        include_str!("../src/shell/workspace/merge_ops.rs"),
        include_str!("../src/shell/workspace/merge_notices.rs"),
    ];
    for src in sources {
        for call in ["set_comment", "set_phase", "workspace_repo"] {
            assert!(
                !src.contains(call),
                "the merge path must not reach the workspace row: found `{call}`"
            );
        }
    }
}
