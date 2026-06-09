//! Pure state machine deriving the Source Control primary split-button label.
//!
//! Priority ladder for the source-control primary action button.
//! No GPUI imports — every input/output is plain data, so the whole module is
//! unit-testable from `cargo test --workspace` without a TestAppContext.
//!
//! Compound kinds (Commit & Push, Sync, Publish dropdown) are out of scope
//! for v1; the primary button collapses to one label per state.

/// Distinct labels the primary split-button can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryActionKind {
    Commit,
    Stage,
    Push,
    Pull,
    Sync,
    Publish,
    /// Branch is published and in sync with its GitHub upstream and has no
    /// open PR yet — offer one-click `gh pr create`.
    CreatePR,
}

/// Remote operations that can be in flight. `Fetch` participates in the busy
/// flag but never becomes the primary's natural label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOpKind {
    Push,
    Pull,
    Sync,
    Fetch,
    Publish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryAction {
    pub kind: PrimaryActionKind,
    pub label: String,
    pub title: String,
    pub disabled: bool,
}

/// Snapshot of the branch's upstream tracking ref. Mirrors the relevant fields
/// of `oximux_core::GitState` so the resolver stays a pure data function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpstreamStatus {
    pub has_upstream: bool,
    pub ahead: u32,
    pub behind: u32,
}

/// Inputs to the primary-action resolver.
///
/// `upstream_status: None` represents "fetchUpstreamStatus has not resolved
/// yet" — the resolver returns a disabled Commit so the button has a stable
/// frame on first paint.
///
/// `force_push_with_lease` and `base_ref` are consumed by the dropdown
/// resolver (`dropdown_items::resolve`) but not by the primary-button
/// resolver (which only ever renders a single verb). They live here so the
/// two resolvers share a single inputs struct — there's no behavioural
/// difference for the primary, just an unused field on the dropdown's
/// "additional inputs" struct wrapping this one.
#[derive(Debug, Clone, Default)]
pub struct PrimaryActionInputs {
    pub staged_count: usize,
    pub has_unstaged_changes: bool,
    /// True when at least one file appears in BOTH the staged and unstaged
    /// sections (a partial-hunk stage). Drives the "Stage All" override so the
    /// index matches the worktree before commit hooks rewrite staged copies.
    pub has_partially_staged_changes: bool,
    pub has_message: bool,
    pub has_unresolved_conflicts: bool,
    pub is_committing: bool,
    pub is_remote_operation_active: bool,
    pub upstream_status: Option<UpstreamStatus>,
    pub in_flight_remote_op_kind: Option<RemoteOpKind>,
    /// `origin` is a GitHub remote (so `gh` can create a PR). Gates the
    /// terminal Create-PR rung; non-GitHub remotes fall through to the
    /// plain "up to date" frame.
    pub is_github_remote: bool,
    /// The current branch already has an open PR. Suppresses Create-PR so the
    /// button doesn't offer a redundant action.
    pub has_open_pr: bool,
    /// The current branch's PR was merged. Also suppresses Create-PR — a new
    /// PR for already-merged work would be a duplicate — with a distinct
    /// "already merged" reason rather than the generic "open PR exists".
    pub pr_merged: bool,
    /// A `gh pr create` is in flight — locks the primary to a disabled
    /// "Creating PR…" frame, mirroring the remote-op busy treatment.
    pub is_creating_pr: bool,
    /// A `gh pr merge` is in flight — disables the merge rows so the menu
    /// doesn't offer a redundant action mid-merge.
    pub is_merging_pr: bool,
    /// True when the branch has commits beyond its base. Gates the
    /// Publish primary: an unpublished branch with no branch commits has
    /// nothing to publish, so the button disables itself (kept consistent
    /// with the dropdown's "No Branch Changes" row rather than offering an
    /// enabled Publish that would push a branch identical to its base).
    pub has_branch_commits: bool,
    /// HEAD is detached (no current branch). A PR can't be created from a
    /// detached HEAD, so the Create-PR reason steers "Check out a branch
    /// first" rather than a sync message.
    pub is_detached_head: bool,
    /// The current branch IS the PR base branch (e.g. on `main`, base
    /// `origin/main`). A PR from a branch to itself is invalid, so Create-PR
    /// is suppressed with "Switch to a feature branch" — without this an
    /// in-sync default branch would wrongly offer an enabled Create PR.
    pub on_default_branch: bool,
}

/// Resolve the primary split-button action. Priority ladder mirrors the
/// design-doc state machine documented in phase-04.
pub fn resolve_primary_action(inputs: &PrimaryActionInputs) -> PrimaryAction {
    // 1. Commit in flight — lock the primary regardless of other state.
    if inputs.is_committing {
        return disabled_commit("Commit in progress…");
    }

    // 1b. PR creation in flight — lock to a disabled Create-PR frame.
    if inputs.is_creating_pr {
        return PrimaryAction {
            kind: PrimaryActionKind::CreatePR,
            label: "Create PR".to_string(),
            title: "Creating pull request…".to_string(),
            disabled: true,
        };
    }

    // 2. Remote op in flight — derive the candidate via a recursive call with
    //    the busy flag cleared, then mirror the in-flight kind onto the label
    //    when the user picked something other than the natural primary.
    if inputs.is_remote_operation_active {
        let mut without_busy = inputs.clone();
        without_busy.is_remote_operation_active = false;
        let candidate = resolve_primary_action(&without_busy);

        if let Some(kind) = inputs.in_flight_remote_op_kind
            && primary_kind_for_remote(kind).is_some_and(|pk| pk != candidate.kind)
        {
            let primary_kind = primary_kind_for_remote(kind).expect("checked above");
            let label = label_for_primary(primary_kind);
            return PrimaryAction {
                kind: primary_kind,
                label: label.to_string(),
                title: format!("{label} in progress…"),
                disabled: true,
            };
        }

        let title = if inputs.has_unresolved_conflicts {
            "Resolve conflicts before committing".to_string()
        } else if candidate.kind == PrimaryActionKind::Commit {
            "Remote operation in progress — try again once it finishes".to_string()
        } else {
            "Remote operation in progress…".to_string()
        };
        return PrimaryAction {
            disabled: true,
            title,
            ..candidate
        };
    }

    // 3. Unresolved conflicts block any commit path.
    if inputs.has_unresolved_conflicts {
        return disabled_commit("Resolve conflicts before committing");
    }

    let has_staged = inputs.staged_count > 0;

    // 4. Has staged AND partially staged → Stage All. A path with both staged
    //    and unstaged edits can make partial-stash restore fail after commit
    //    hooks (e.g. formatters) rewrite the staged copy, so unify the index
    //    with the worktree before letting the commit through.
    if has_staged && inputs.has_partially_staged_changes {
        return PrimaryAction {
            kind: PrimaryActionKind::Stage,
            label: "Stage All".to_string(),
            title: "Stage all changes before committing partially staged files".to_string(),
            disabled: false,
        };
    }

    // 5. Has staged + message → plain Commit. (Compound flows live in the
    //    dropdown, out of scope for v1.)
    if has_staged && inputs.has_message {
        return PrimaryAction {
            kind: PrimaryActionKind::Commit,
            label: "Commit".to_string(),
            title: "Commit staged changes".to_string(),
            disabled: false,
        };
    }

    // 5. Has staged + no message → disabled Commit with a hint.
    if has_staged && !inputs.has_message {
        return disabled_commit("Enter a commit message to commit");
    }

    // 5b. Nothing staged but worktree dirty → push the user through Stage All
    //     before any remote op runs.
    if !has_staged && inputs.has_unstaged_changes {
        return PrimaryAction {
            kind: PrimaryActionKind::Stage,
            label: "Stage All".to_string(),
            title: "Stage all changes".to_string(),
            disabled: false,
        };
    }

    // 6a. Upstream hasn't resolved yet — stable disabled-Commit frame so the
    //     button doesn't flash "Publish Branch" on first paint.
    let Some(upstream) = inputs.upstream_status else {
        return disabled_commit("Stage at least one file to commit");
    };

    // 6b. Branch never published. Disabled when there are no branch commits
    //     to publish — pushing a branch identical to its base is pointless,
    //     and an enabled button here would contradict the dropdown's
    //     "No Branch Changes" row for the same action.
    if !upstream.has_upstream {
        return PrimaryAction {
            kind: PrimaryActionKind::Publish,
            label: "Publish Branch".to_string(),
            title: if inputs.has_branch_commits {
                "Publish this branch to origin".to_string()
            } else {
                "Nothing to publish".to_string()
            },
            disabled: !inputs.has_branch_commits,
        };
    }

    // 6c / 6d / 6e. Diverged / behind / ahead.
    if upstream.ahead > 0 && upstream.behind > 0 {
        return PrimaryAction {
            kind: PrimaryActionKind::Sync,
            label: "Sync".to_string(),
            title: format!("Pull {}, push {}", upstream.behind, upstream.ahead),
            disabled: false,
        };
    }
    if upstream.behind > 0 {
        return PrimaryAction {
            kind: PrimaryActionKind::Pull,
            label: "Pull".to_string(),
            title: format!(
                "Pull {} {}",
                upstream.behind,
                plural_commits(upstream.behind)
            ),
            disabled: false,
        };
    }
    if upstream.ahead > 0 {
        return PrimaryAction {
            kind: PrimaryActionKind::Push,
            label: "Push".to_string(),
            title: format!("Push {} {}", upstream.ahead, plural_commits(upstream.ahead)),
            disabled: false,
        };
    }

    // 6f. Clean + tracked + in sync. The branch is pushed and even with its
    //     upstream — the natural moment to open a PR. Offer Create PR when the
    //     remote is GitHub and there's no open PR yet; otherwise the plain
    //     "up to date" frame. Push/Sync/Pull rungs sit ahead of this, so a
    //     branch with unpushed/unpulled commits is steered there first ("push
    //     before PR"). A merged PR or the default branch suppresses the offer
    //     too — a new PR for already-merged work, or from the base branch to
    //     itself, would be invalid.
    if inputs.is_github_remote
        && !inputs.has_open_pr
        && !inputs.pr_merged
        && !inputs.on_default_branch
    {
        return PrimaryAction {
            kind: PrimaryActionKind::CreatePR,
            label: "Create PR".to_string(),
            title: "Create a pull request for this branch".to_string(),
            disabled: false,
        };
    }

    disabled_commit("Nothing to commit. Branch is up to date.")
}

fn disabled_commit(title: &str) -> PrimaryAction {
    PrimaryAction {
        kind: PrimaryActionKind::Commit,
        label: "Commit".to_string(),
        title: title.to_string(),
        disabled: true,
    }
}

fn label_for_primary(kind: PrimaryActionKind) -> &'static str {
    match kind {
        PrimaryActionKind::Commit => "Commit",
        PrimaryActionKind::Stage => "Stage All",
        PrimaryActionKind::Push => "Push",
        PrimaryActionKind::Pull => "Pull",
        PrimaryActionKind::Sync => "Sync",
        PrimaryActionKind::Publish => "Publish Branch",
        PrimaryActionKind::CreatePR => "Create PR",
    }
}

/// Map an in-flight remote op back onto the primary kind that can host it.
/// `Fetch` is dropdown-only and returns `None` — its in-flight state never
/// rewrites the primary button.
fn primary_kind_for_remote(kind: RemoteOpKind) -> Option<PrimaryActionKind> {
    match kind {
        RemoteOpKind::Push => Some(PrimaryActionKind::Push),
        RemoteOpKind::Pull => Some(PrimaryActionKind::Pull),
        RemoteOpKind::Sync => Some(PrimaryActionKind::Sync),
        RemoteOpKind::Publish => Some(PrimaryActionKind::Publish),
        RemoteOpKind::Fetch => None,
    }
}

fn plural_commits(n: u32) -> &'static str {
    if n == 1 { "commit" } else { "commits" }
}
