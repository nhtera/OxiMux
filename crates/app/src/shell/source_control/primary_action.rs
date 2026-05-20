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
}

/// Resolve the primary split-button action. Priority ladder mirrors the
/// design-doc state machine documented in phase-04.
pub fn resolve_primary_action(inputs: &PrimaryActionInputs) -> PrimaryAction {
    // 1. Commit in flight — lock the primary regardless of other state.
    if inputs.is_committing {
        return disabled_commit("Commit in progress…");
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

    // 6b. Branch never published.
    if !upstream.has_upstream {
        return PrimaryAction {
            kind: PrimaryActionKind::Publish,
            label: "Publish Branch".to_string(),
            title: "Publish this branch to origin".to_string(),
            disabled: false,
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

    // 6f. Clean + tracked + in sync.
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
