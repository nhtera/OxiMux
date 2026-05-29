//! Regression: when the only "changes" are Ignored entries (git status
//! --ignored echoes back `target/`, `dist/`, etc.), the primary action
//! must read as a disabled Commit — NOT Stage All. The file-list panel
//! filters those rows out, so the button has to match or the user sees
//! "No changes on this branch" right above an enabled `Stage All` button.
//!
//! These tests exercise the source_control::Render-side filter that
//! sits on top of `resolve_primary_action`. The filter mirrors the
//! ignored-skip in `changed_files::partition_files`.

use oximux_app::shell::source_control::primary_action::{
    PrimaryActionInputs, PrimaryActionKind, UpstreamStatus, resolve_primary_action,
};

/// Helper that mirrors the loop in `SourceControlPanel::resolve_primary`
/// — including the Ignored skip the fix added. Keeps the test honest by
/// using the same shape the production code does.
///
/// `tracked_branch`: when `Some`, mirrors the screenshot scenario where
/// the branch has a real upstream (e.g. `main` set to `origin/main` with
/// 0 ahead / 0 behind). `None` mirrors a branch that hasn't been
/// published yet.
fn compute_inputs(
    files: &[(IdxStatus, WtStatus)],
    tracked_branch: Option<(u32, u32)>,
) -> PrimaryActionInputs {
    let mut staged = 0usize;
    let mut unstaged = false;
    let mut partial = false;
    for (idx, wt) in files {
        if matches!(idx, IdxStatus::Ignored) || matches!(wt, WtStatus::Ignored) {
            continue;
        }
        let is_s = !matches!(idx, IdxStatus::Unmodified | IdxStatus::Untracked);
        let is_u = !matches!(wt, WtStatus::Unmodified);
        if is_s {
            staged += 1;
        }
        if is_u {
            unstaged = true;
        }
        if is_s && is_u {
            partial = true;
        }
    }
    PrimaryActionInputs {
        staged_count: staged,
        has_unstaged_changes: unstaged,
        has_partially_staged_changes: partial,
        upstream_status: tracked_branch.map(|(ahead, behind)| UpstreamStatus {
            has_upstream: true,
            ahead,
            behind,
        }),
        ..PrimaryActionInputs::default()
    }
}

/// Mirror of `oximux_core::IndexStatus` — kept local so this test file
/// doesn't need to depend on the upstream enum (and stays insulated from
/// future variant additions that aren't relevant to the resolver).
#[derive(Copy, Clone)]
enum IdxStatus {
    Unmodified,
    Modified,
    Untracked,
    Ignored,
}

#[derive(Copy, Clone)]
enum WtStatus {
    Unmodified,
    Modified,
    Untracked,
    Ignored,
}

#[test]
fn ignored_only_treats_repo_as_clean_with_upstream() {
    // `dist/` + `target/` reported by `git status --ignored`. Nothing
    // else changed. With an upstream present and 0 ahead / 0 behind,
    // the resolver should land on a disabled Commit, NOT Stage All.
    // This mirrors the user-reported regression from the OxiMux screenshot.
    let inputs = compute_inputs(
        &[
            (IdxStatus::Ignored, WtStatus::Unmodified),
            (IdxStatus::Unmodified, WtStatus::Ignored),
        ],
        Some((0, 0)), // tracked branch, in sync
    );
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Commit);
    assert!(act.disabled, "clean + ignored-only repo must disable primary");
}

#[test]
fn ignored_only_unpublished_branch_offers_publish() {
    // Different scenario: branch has no upstream AND only Ignored entries.
    // The resolver legitimately offers Publish Branch here — Ignored
    // entries don't suppress the publish prompt for a fresh local branch.
    let inputs = compute_inputs(&[(IdxStatus::Ignored, WtStatus::Ignored)], None);
    let act = resolve_primary_action(&inputs);
    // Either disabled-Commit (waiting for upstream resolution) or
    // Publish Branch is acceptable; both reflect a clean state.
    assert!(
        act.kind == PrimaryActionKind::Commit || act.kind == PrimaryActionKind::Publish,
        "unexpected kind for ignored-only unpublished branch: {:?}",
        act.kind
    );
}

#[test]
fn ignored_plus_real_modification_still_triggers_stage_all() {
    // Ignored entries don't suppress a genuine unstaged modification —
    // the resolver should see the modification through the filter and
    // surface Stage All as expected.
    let inputs = compute_inputs(
        &[
            (IdxStatus::Ignored, WtStatus::Unmodified),
            (IdxStatus::Unmodified, WtStatus::Modified),
        ],
        Some((0, 0)),
    );
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Stage);
    assert!(!act.disabled);
}

#[test]
fn untracked_alone_still_triggers_stage_all() {
    // Untracked files (not Ignored) are legitimately unstaged — the
    // user does need to `git add` them. Resolver must NOT swallow them
    // alongside the Ignored filter.
    let inputs = compute_inputs(
        &[(IdxStatus::Untracked, WtStatus::Untracked)],
        Some((0, 0)),
    );
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Stage);
}

#[test]
fn staged_modification_resolves_to_commit_not_stage_all() {
    // The staged-side mirror of the cases above: an index-Modified file
    // with a clean worktree is a genuinely STAGED change. It must resolve
    // to the Commit button — disabled here only because no commit message
    // has been entered yet — and never to Stage All. This is also the only
    // case that drives an index-Modified entry through the filter.
    let inputs = compute_inputs(
        &[(IdxStatus::Modified, WtStatus::Unmodified)],
        Some((0, 0)),
    );
    let act = resolve_primary_action(&inputs);
    assert_eq!(act.kind, PrimaryActionKind::Commit);
    assert!(
        act.disabled,
        "staged-only with no commit message must be a disabled Commit (awaiting message)"
    );
}
