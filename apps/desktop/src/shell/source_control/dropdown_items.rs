//! Source Control dropdown menu — pure data resolver.
//!
//! Computes the dropdown menu rows (kind + label + tooltip + disabled
//! state) from a single inputs snapshot. No GPUI imports — every
//! input/output is plain data so the whole module is unit-testable from
//! `cargo test --workspace` without a TestAppContext.
//!
//! Mirrors the state machine in `primary_action.rs` but emits the full
//! menu rather than collapsing to a single verb. Stable row order keeps
//! the menu shape consistent across releases so the user's muscle memory
//! doesn't change post-upgrade.

use crate::shell::forge::MergeMethod;
use crate::shell::source_control::primary_action::PrimaryActionInputs;

/// One row of the dropdown — either an actionable item or a separator line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropdownEntry {
    Item {
        kind: DropdownActionKind,
        label: String,
        title: String,
        disabled: bool,
    },
    Separator,
}

/// Distinct verbs the dropdown can dispatch. The render layer in
/// `commit_area.rs` maps each kind to a method on `CommitArea`.
///
/// `ForcePush` reuses the same kind whether it appears in the Push or
/// Sync slot — the slot only changes which menu position the user
/// clicked, the backend action is identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropdownActionKind {
    Commit,
    CommitPush,
    CommitForcePush,
    CommitSync,
    Push,
    ForcePush,
    /// Disabled in v1; lands with the hosted-review adapter in v1.1.
    CreatePr,
    /// Disabled in v1; lands with the hosted-review adapter in v1.1.
    PushBeforePr,
    /// Merge the current branch's open PR with the chosen method. Only present
    /// (one row per method) when a PR is open.
    MergePr(MergeMethod),
    Pull,
    /// Fast-forward-only pull (`git pull --ff-only`). Distinct menu row from
    /// `Pull` so the user sees both affordances (matching the benchmark menu);
    /// enabled only when behind with no local commits ahead. Shares the
    /// ff-only backend op with `Pull` until a merge-pull path lands.
    FastForward,
    Sync,
    Rebase,
    Fetch,
    Publish,
}

/// Inputs to the resolver. Wraps `PrimaryActionInputs` (shared with the
/// primary split-button) plus the dropdown-only flags.
#[derive(Debug, Clone, Default)]
pub struct DropdownInputs {
    pub primary: PrimaryActionInputs,
    /// True when the local branch has rewritten commits that the upstream
    /// still has (rebase / amend / squash). Drives the Push / Sync /
    /// Commit & Push label swap to Force Push.
    pub force_push_with_lease: bool,
    /// Configured base ref for the worktree (e.g. `origin/main`). Drives
    /// the "Rebase from <base>" label; `None` disables the Rebase row.
    pub base_ref: Option<String>,
    /// True when the branch has at least one commit beyond its base
    /// (i.e. `GitState.branch_committed` is non-empty). Drives the Publish
    /// row's label/enablement: an unpublished branch with no branch commits
    /// has nothing to publish, so the row reads "No Branch Changes" /
    /// "Commit Changes First" (when dirty) instead of "Publish Branch".
    pub has_branch_commits: bool,
    /// True when a hosted-review-creation backend op is in flight. Disables
    /// Create PR / Push before PR rows to prevent re-entry. Always false
    /// in v1 — the hosted-review adapter ships in v1.1.
    pub is_pr_operation_active: bool,
}

/// Build the dropdown menu rows for the given inputs.
///
/// Returns entries in the stable order:
/// Commit · Commit & Push · Commit & Sync · ─ · Push · Force Push · Create PR ·
/// Push before PR · Pull · Fast-forward · Sync · Rebase · Fetch · Publish.
/// (Merge-PR rows are inserted after Push before PR only when a PR is open.)
///
/// Result is purely a function of `inputs` — calling twice with the same
/// inputs yields equal `Vec`s. This invariant powers the unit tests below
/// and means the render layer can `==` two resolutions to short-circuit a
/// re-render if needed.
pub fn resolve(inputs: &DropdownInputs) -> Vec<DropdownEntry> {
    let p = &inputs.primary;
    let upstream = p.upstream_status;
    let ahead = upstream.map(|u| u.ahead).unwrap_or(0);
    let behind = upstream.map(|u| u.behind).unwrap_or(0);
    let has_upstream = upstream.map(|u| u.has_upstream).unwrap_or(false);
    let has_staged = p.staged_count > 0;
    let lease = inputs.force_push_with_lease;

    // "Can the user commit right now?" — gates every Commit* row.
    let can_commit_base = has_staged
        && p.has_message
        && !p.has_unresolved_conflicts
        && !p.is_committing
        && !p.is_remote_operation_active;

    let mut out = Vec::with_capacity(12);

    // 1. Commit
    out.push(item(
        DropdownActionKind::Commit,
        "Commit".to_string(),
        if can_commit_base {
            "Commit staged changes".to_string()
        } else {
            commit_disabled_reason(p, has_staged).to_string()
        },
        !can_commit_base,
    ));

    // 2. Commit & Push (or Commit & Force Push when the upstream needs a
    //    lease-aware rewrite). Stays as one row — label flip only.
    let cp_kind = if lease {
        DropdownActionKind::CommitForcePush
    } else {
        DropdownActionKind::CommitPush
    };
    let cp_label = if lease {
        "Commit & Force Push".to_string()
    } else {
        "Commit & Push".to_string()
    };
    // Diverged (behind without a lease rewrite) → steer to Commit & Sync so the
    // push isn't rejected for being behind.
    let cp_disabled = !can_commit_base || !has_upstream || (behind > 0 && !lease);
    out.push(item(
        cp_kind,
        cp_label,
        commit_push_tooltip(p, can_commit_base, has_staged, has_upstream, lease, behind),
        cp_disabled,
    ));

    // 3. Commit & Sync — only meaningful when behind (there's something to pull
    //    before pushing); a lease rewrite routes through Commit & Force Push.
    let cs_disabled = !can_commit_base || !has_upstream || lease || behind == 0;
    out.push(item(
        DropdownActionKind::CommitSync,
        "Commit & Sync".to_string(),
        commit_sync_tooltip(p, can_commit_base, has_staged, has_upstream, lease, behind),
        cs_disabled,
    ));

    out.push(DropdownEntry::Separator);

    // Push — clean push only. A diverged branch (behind) routes to Sync and a
    //    lease rewrite routes to Force Push; Push stays its own stable row in
    //    every state so the menu shape never shifts under the cursor.
    let push_disabled = !has_upstream
        || ahead == 0
        || lease
        || behind > 0
        || p.has_unresolved_conflicts
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::Push,
        format!("Push{}", suffix_ahead(ahead)),
        push_tooltip(p, has_upstream, ahead, behind, lease, push_disabled),
        push_disabled,
    ));

    // Force Push (with lease) — always visible so the escape hatch for a
    //    rewritten history stays discoverable; enabled whenever the branch is
    //    ahead of a known upstream.
    let force_push_disabled = !has_upstream
        || ahead == 0
        || p.has_unresolved_conflicts
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::ForcePush,
        format!("Force Push{}", suffix_ahead(ahead)),
        force_push_tooltip(p, has_upstream, ahead, force_push_disabled),
        force_push_disabled,
    ));

    // 6. Create PR — enabled when the branch is published, in sync with its
    //    upstream, and has no open PR yet (same gate as the primary Create-PR
    //    rung). Runs `gh pr create --fill` / `glab mr create --fill`.
    let in_sync = has_upstream && ahead == 0 && behind == 0;
    let create_pr_disabled = !p.forge_supports_pr
        || p.is_detached_head
        || p.on_default_branch
        || p.has_open_pr
        || p.pr_merged
        || !in_sync
        || p.is_creating_pr
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::CreatePr,
        "Create PR".to_string(),
        if create_pr_disabled {
            create_pr_disabled_reason(p, has_upstream, ahead, behind, lease).to_string()
        } else {
            "Create a pull request for this branch".to_string()
        },
        create_pr_disabled,
    ));

    // 7. Push & Create PR — compound (push then create) not yet wired; the
    //    two-step flow (Push, then Create PR) covers it. Kept as a disabled
    //    row so the menu shape stays stable.
    out.push(item(
        DropdownActionKind::PushBeforePr,
        if lease {
            "Force Push before PR".to_string()
        } else {
            "Push before PR".to_string()
        },
        "Push first, then use Create PR".to_string(),
        true,
    ));

    // 7b. Merge PR — one row per method, present only when a PR is open so the
    //     menu stays uncluttered the rest of the time. Disabled while another
    //     op is in flight to prevent overlapping remote actions.
    if p.has_open_pr {
        let merge_disabled = !p.forge_supports_pr
            || p.is_committing
            || p.is_remote_operation_active
            || p.is_creating_pr
            || p.is_merging_pr;
        let merge_reason = |verb: &str| {
            if merge_disabled {
                "Another operation is in progress".to_string()
            } else {
                format!("Merge the open pull request ({verb})")
            }
        };
        out.push(DropdownEntry::Separator);
        out.push(item(
            DropdownActionKind::MergePr(MergeMethod::Squash),
            "Merge PR (squash)".to_string(),
            merge_reason("squash"),
            merge_disabled,
        ));
        out.push(item(
            DropdownActionKind::MergePr(MergeMethod::Merge),
            "Merge PR (merge commit)".to_string(),
            merge_reason("merge commit"),
            merge_disabled,
        ));
        out.push(item(
            DropdownActionKind::MergePr(MergeMethod::Rebase),
            "Merge PR (rebase)".to_string(),
            merge_reason("rebase"),
            merge_disabled,
        ));
    }

    // Pull — fetch + integrate remote commits. A lease rewrite means the
    //    remote only holds older copies of local commits, so there's nothing to
    //    pull; that state disables the row.
    let pull_disabled = !has_upstream
        || behind == 0
        || lease
        || p.has_unresolved_conflicts
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::Pull,
        format!("Pull{}", suffix_behind(behind)),
        pull_tooltip(p, has_upstream, behind, lease, pull_disabled),
        pull_disabled,
    ));

    // Fast-forward — advance the branch only when it can move without a merge
    //    (behind, with no local commits ahead). Its own row so the safe
    //    fast-path sits next to the general Pull.
    let fast_forward_disabled = !has_upstream
        || behind == 0
        || ahead > 0
        || lease
        || p.has_unresolved_conflicts
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::FastForward,
        format!("Fast-forward{}", suffix_behind(behind)),
        fast_forward_tooltip(p, has_upstream, ahead, behind, lease, fast_forward_disabled),
        fast_forward_disabled,
    ));

    // Sync — pull then push. Disabled when the branch is level or when a lease
    //    rewrite is needed (Force Push is the correct path there, and it has its
    //    own row above).
    let sync_disabled = !has_upstream
        || lease
        || (ahead == 0 && behind == 0)
        || p.has_unresolved_conflicts
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::Sync,
        format!("Sync{}", suffix_diverged(ahead, behind)),
        sync_tooltip(p, has_upstream, ahead, behind, lease, sync_disabled),
        sync_disabled,
    ));

    // 10. Rebase from <base>. Label adapts to the configured base ref;
    //     missing base ref or a dirty worktree disable the row.
    let rebase_label = match inputs.base_ref.as_deref() {
        Some(base) => format!("Rebase from {base}"),
        None => "Rebase from Base".to_string(),
    };
    let rebase_disabled = inputs.base_ref.is_none()
        || p.has_unresolved_conflicts
        || p.has_unstaged_changes
        || p.is_committing
        || p.is_remote_operation_active;
    out.push(item(
        DropdownActionKind::Rebase,
        rebase_label,
        if rebase_disabled {
            rebase_disabled_reason(p, inputs.base_ref.as_deref()).to_string()
        } else {
            format!(
                "Rebase current branch with latest commits from {}",
                inputs.base_ref.as_deref().unwrap_or("")
            )
        },
        rebase_disabled,
    ));

    // 11. Fetch — almost always enabled; only blocked by an in-flight
    //     remote op. Fetch never mutates the working tree, so conflicts
    //     and dirty index don't gate it.
    let fetch_disabled = p.is_remote_operation_active || p.is_committing;
    out.push(item(
        DropdownActionKind::Fetch,
        "Fetch".to_string(),
        if fetch_disabled {
            "Another operation is in progress".to_string()
        } else {
            "Fetch from remote without merging".to_string()
        },
        fetch_disabled,
    ));

    // 12. Publish — only meaningful when the branch lacks an upstream;
    //     stays in the menu (disabled) when an upstream already exists so
    //     the row order is stable across states. The label adapts to WHY
    //     publish is unavailable so the row is self-explanatory:
    //       - "PR Status"           — unpublished, but the branch's PR is
    //                                  already merged (don't re-publish)
    //       - "Commit Changes First" — unpublished, no commits, dirty changes
    //                                  the user likely means to commit first
    //       - "No Branch Changes"    — unpublished, no commits, clean
    //       - "Publish Branch"       — ready (has branch commits, no upstream)
    let busy = p.is_committing || p.is_remote_operation_active;
    let dirty = p.staged_count > 0 || p.has_unstaged_changes;
    let merged_pr = !has_upstream && p.pr_merged;
    let no_branch_commits = !has_upstream && !merged_pr && !inputs.has_branch_commits;
    let uncommitted_changes = no_branch_commits && dirty;
    let publish_label = if merged_pr {
        "PR Status"
    } else if uncommitted_changes {
        "Commit Changes First"
    } else if no_branch_commits {
        "No Branch Changes"
    } else {
        "Publish Branch"
    };
    let publish_title = if has_upstream {
        "Branch is already published"
    } else if busy {
        "An operation is already in progress"
    } else if merged_pr {
        "PR is already merged"
    } else if uncommitted_changes {
        "Commit changes before publishing the branch"
    } else if no_branch_commits {
        "Nothing to publish"
    } else {
        "Publish this branch to origin"
    };
    let publish_disabled = has_upstream || busy || merged_pr || no_branch_commits;
    out.push(item(
        DropdownActionKind::Publish,
        publish_label.to_string(),
        publish_title.to_string(),
        publish_disabled,
    ));

    out
}

fn item(kind: DropdownActionKind, label: String, title: String, disabled: bool) -> DropdownEntry {
    DropdownEntry::Item {
        kind,
        label,
        title,
        disabled,
    }
}

// --- Disabled-reason helpers -------------------------------------------------
//
// Each helper returns the FIRST applicable reason in order of severity. The
// resolver only calls these when the row is actually disabled, so an empty
// string here would be a bug — but we still default to a generic fallback
// rather than panic so a stale call site can't crash render.

fn commit_disabled_reason(p: &PrimaryActionInputs, has_staged: bool) -> &'static str {
    if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if p.has_unresolved_conflicts {
        "Resolve conflicts before committing"
    } else if !has_staged {
        "Stage at least one file to commit"
    } else if !p.has_message {
        "Enter a commit message to commit"
    } else {
        "Cannot commit"
    }
}

fn push_disabled_reason(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
) -> &'static str {
    if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if p.has_unresolved_conflicts {
        "Resolve conflicts before pushing"
    } else if !has_upstream {
        "Publish the branch first"
    } else if ahead == 0 {
        "Nothing to push"
    } else {
        "Cannot push"
    }
}

/// Precise "why is Create PR disabled" reason, in severity order. The
/// `!in_sync` case is split into needs-push (only ahead) vs needs-sync
/// (behind — pull or force-push first), and detached-HEAD / default-branch
/// get their own actionable steers. (A possible `auth_required` →
/// "Run gh auth login" reason isn't distinguished here, since OxiMux
/// doesn't poll `gh auth status`.)
fn create_pr_disabled_reason(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    behind: u32,
    lease: bool,
) -> &'static str {
    if p.is_creating_pr {
        "Creating pull request…"
    } else if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if !p.forge_supports_pr {
        "No GitHub or GitLab remote"
    } else if p.is_detached_head {
        "Check out a branch first"
    } else if p.on_default_branch {
        "Switch to a feature branch"
    } else if p.pr_merged {
        "This branch's PR is already merged"
    } else if p.has_open_pr {
        "This branch already has an open PR"
    } else if !has_upstream {
        "Publish the branch first"
    } else if behind > 0 {
        // Behind upstream — a plain push is rejected; pull (or force-push if
        // the local history was rewritten) before opening the PR.
        if lease {
            "Force Push first, then create a PR"
        } else {
            "Sync first, then create a PR"
        }
    } else if ahead > 0 {
        "Push first, then create a PR"
    } else {
        "Cannot create PR"
    }
}

fn pull_disabled_reason(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    behind: u32,
) -> &'static str {
    if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if p.has_unresolved_conflicts {
        "Resolve conflicts before pulling"
    } else if !has_upstream {
        "Publish the branch first"
    } else if behind == 0 {
        "Nothing to pull"
    } else {
        "Cannot pull"
    }
}

fn sync_disabled_reason(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    behind: u32,
) -> &'static str {
    if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if p.has_unresolved_conflicts {
        "Resolve conflicts before syncing"
    } else if !has_upstream {
        "Publish the branch first"
    } else if ahead == 0 && behind == 0 {
        "Branch is up to date"
    } else {
        "Cannot sync"
    }
}

fn rebase_disabled_reason(
    p: &PrimaryActionInputs,
    base_ref: Option<&str>,
) -> &'static str {
    if p.is_committing {
        "Commit in progress…"
    } else if p.is_remote_operation_active {
        "Remote operation in progress"
    } else if p.has_unresolved_conflicts {
        "Resolve conflicts before rebasing"
    } else if p.has_unstaged_changes {
        "Commit or stash local changes before rebasing"
    } else if base_ref.is_none() {
        "Configure a base ref first"
    } else {
        "Cannot rebase"
    }
}

/// Tooltip for Commit & Push / Commit & Force Push. Priority mirrors the
/// benchmark menu: publish-first, then commit blockers, then the lease /
/// divergence steer, else the plain "what will happen" line.
fn commit_push_tooltip(
    p: &PrimaryActionInputs,
    can_commit: bool,
    has_staged: bool,
    has_upstream: bool,
    lease: bool,
    behind: u32,
) -> String {
    if !has_upstream {
        "Publish the branch first to push commits".to_string()
    } else if !can_commit {
        commit_disabled_reason(p, has_staged).to_string()
    } else if lease {
        "Commit staged changes and force push with lease".to_string()
    } else if behind > 0 {
        "Use Commit & Sync to pull remote changes before pushing".to_string()
    } else {
        "Commit staged changes and push".to_string()
    }
}

/// Tooltip for Commit & Sync. A lease rewrite or a level branch steers the
/// user to the correct sibling verb before the commit blockers are consulted.
fn commit_sync_tooltip(
    p: &PrimaryActionInputs,
    can_commit: bool,
    has_staged: bool,
    has_upstream: bool,
    lease: bool,
    behind: u32,
) -> String {
    if !has_upstream {
        "Publish the branch first to sync commits".to_string()
    } else if lease {
        "Use Commit & Force Push — the remote only has older copies of local commits".to_string()
    } else if behind == 0 {
        "Nothing to pull — use Commit & Push instead".to_string()
    } else if !can_commit {
        commit_disabled_reason(p, has_staged).to_string()
    } else {
        "Commit, then pull and push".to_string()
    }
}

// --- Per-row tooltips --------------------------------------------------------
//
// Each returns the hover-tooltip text: a "what will happen" line when the row
// is enabled, or the most relevant "why not" reason when disabled. The wording
// steers the user toward the correct sibling verb (Sync on divergence, Force
// Push on a lease rewrite) so the menu explains itself on hover.

fn push_tooltip(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    behind: u32,
    lease: bool,
    disabled: bool,
) -> String {
    if !disabled {
        return format!("Push {ahead} commit{}", plural(ahead));
    }
    if !has_upstream {
        "Publish the branch first to push commits".to_string()
    } else if lease {
        "Use Force Push — the remote only has older copies of local commits".to_string()
    } else if behind > 0 && ahead > 0 {
        "Sync first to pull remote changes before pushing".to_string()
    } else if ahead == 0 {
        "Nothing to push".to_string()
    } else {
        push_disabled_reason(p, has_upstream, ahead).to_string()
    }
}

fn force_push_tooltip(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    disabled: bool,
) -> String {
    if !disabled {
        return format!("Force-push {ahead} commit{} with lease", plural(ahead));
    }
    if !has_upstream {
        "Publish the branch first to force push commits".to_string()
    } else if ahead == 0 {
        "Nothing to force push".to_string()
    } else {
        push_disabled_reason(p, has_upstream, ahead).to_string()
    }
}

fn pull_tooltip(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    behind: u32,
    lease: bool,
    disabled: bool,
) -> String {
    if !disabled {
        return format!("Pull {behind} commit{}", plural(behind));
    }
    if !has_upstream {
        "Publish the branch first to pull commits".to_string()
    } else if lease {
        "Nothing new to pull — the remote only has older copies of local commits".to_string()
    } else if behind == 0 {
        "Nothing to pull".to_string()
    } else {
        pull_disabled_reason(p, has_upstream, behind).to_string()
    }
}

fn fast_forward_tooltip(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    behind: u32,
    lease: bool,
    disabled: bool,
) -> String {
    if !disabled {
        return format!("Fast-forward {behind} commit{}", plural(behind));
    }
    if !has_upstream {
        "Publish the branch first to fast-forward".to_string()
    } else if lease {
        "Nothing new to fast-forward — the remote only has older copies of local commits".to_string()
    } else if behind == 0 {
        "Nothing to fast-forward".to_string()
    } else if ahead > 0 {
        "Local commits prevent a fast-forward — use Pull or Sync".to_string()
    } else {
        pull_disabled_reason(p, has_upstream, behind).to_string()
    }
}

fn sync_tooltip(
    p: &PrimaryActionInputs,
    has_upstream: bool,
    ahead: u32,
    behind: u32,
    lease: bool,
    disabled: bool,
) -> String {
    if !disabled {
        return format!("Pull {behind}, push {ahead}");
    }
    if !has_upstream {
        "Publish the branch first to sync commits".to_string()
    } else if lease {
        "Use Force Push — the remote only has older copies of local commits".to_string()
    } else if ahead == 0 && behind == 0 {
        "Branch is up to date".to_string()
    } else {
        sync_disabled_reason(p, has_upstream, ahead, behind).to_string()
    }
}

// --- Label suffix helpers ----------------------------------------------------

fn suffix_ahead(ahead: u32) -> String {
    if ahead > 0 {
        format!(" ({ahead})")
    } else {
        String::new()
    }
}

fn suffix_behind(behind: u32) -> String {
    if behind > 0 {
        format!(" ({behind})")
    } else {
        String::new()
    }
}

fn suffix_diverged(ahead: u32, behind: u32) -> String {
    // Show the directional counts whenever the branch isn't level with its
    // upstream — `(↓behind ↑ahead)` — so Sync reads its scope inline (e.g.
    // `Sync (↓0 ↑1)`), matching the benchmark git menu. Level (0/0) Sync is
    // disabled anyway, so an empty suffix there is never user-visible.
    if ahead > 0 || behind > 0 {
        format!(" (↓{behind} ↑{ahead})")
    } else {
        String::new()
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

