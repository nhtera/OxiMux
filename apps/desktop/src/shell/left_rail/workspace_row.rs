//! Pure plan helpers for one persisted Workspace's rail row.
//!
//! `build_workspace_row_plan`, `build_workspace_card_plan` and
//! `status_dot_color` are pure functions — no GPUI runtime, no IO — unit-tested
//! without a window. The painter that consumes the plan lives in
//! `workspace_card.rs`; the older single-line row painter that used to sit
//! here had no caller left and was retired.

use gpui::{Hsla, SharedString};
use oximux_core::{AgentStatus, WorkPhase, Workspace};
use oximux_git::AheadBehind;
use oximux_settings::Theme;

use crate::shell::agent_presentation::{AgentVerb, agent_verb};
use crate::shell::left_rail::worktree_stats::WorktreeStats;
use crate::shell::pane_group::TabColor;

/// Side length of the colored status circle. Shared with the card painter
/// (`workspace_card.rs`) so the dot size cannot drift between the two.
pub(crate) const STATUS_DOT_SIZE: f32 = 8.0;
/// Folder icon size (matches the workspace header icon). Shared with the card.
pub(crate) const FOLDER_ICON_SIZE: f32 = 14.0;
/// Ellipsis trailing-button width. Shared with the card painter.
pub(crate) const TRAILING_BTN_SIZE: f32 = 18.0;

/// Working-tree diff line counts for one worktree (staged + unstaged vs HEAD).
/// `None` when the diff count is unavailable or still being computed — the
/// card degrades gracefully by omitting the chip rather than blocking render.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffCounts {
    pub added: u32,
    pub removed: u32,
}

/// Sum a numstat `path → (added, removed)` map into a single `DiffCounts`
/// total. Pure + testable; used by the background diff-count refresh to
/// collapse a per-file numstat result into the worktree-level chip value.
pub(crate) fn sum_numstat(
    map: &std::collections::HashMap<std::path::PathBuf, (u32, u32)>,
) -> DiffCounts {
    let (added, removed) = map
        .values()
        .fold((0u32, 0u32), |(a, r), &(na, nr)| (a + na, r + nr));
    DiffCounts { added, removed }
}

/// Line total above which a perfectly symmetric diff stops being plausible
/// as ordinary editing. Hand edits do produce `added == removed` (a renamed
/// symbol, a reflowed block), but they do so at small magnitudes; thousands
/// of lines replaced one-for-one across several files is the signature of a
/// whole-file rewrite — line-ending or encoding renormalisation — not of work.
const RENORMALIZATION_SUSPECT_LINES: u32 = 1_000;

/// Does this numstat result look like renormalisation rather than editing?
///
/// Exists because of an unreproduced defect: a freshly launched app painted
/// `+35361 −35361` on a worktree whose `git status --porcelain` was empty, and
/// cleared itself seconds later. The counts came back from a real
/// `git diff --numstat -z HEAD`, so the bug is in what git was asked or what it
/// saw — not in the cache that renders them. Until it reproduces there is
/// nothing to fix, so this only *names* the shape in the log; it never
/// suppresses a count. Suppressing would hide the recurrence we are waiting for.
pub(crate) fn looks_like_renormalization(files: usize, counts: &DiffCounts) -> bool {
    counts.added == counts.removed
        && counts.added >= RENORMALIZATION_SUSPECT_LINES
        && files >= 2
}

/// Visual tokens for one Workspace row. Pure, testable.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRowPlan {
    pub name: String,
    pub slug: String,
    pub dot_color: Hsla,
    pub bg: Hsla,
    pub fg: Hsla,
    pub fg_sub: Hsla,
    /// `true` for the project's primary (main) worktree — renders a
    /// `primary` badge and suppresses the rename/archive/delete menu
    /// (the main worktree is removed by removing the project, not here).
    pub is_primary: bool,
    /// `true` when this row represents a non-git folder project — renders
    /// a `Folder` badge (instead of `primary`/branch).
    pub is_folder: bool,
    /// `true` when this is the selected/active workspace — the row is
    /// painted as an inset rounded card rather than a flat highlight.
    pub is_active: bool,
}

/// Extended plan for the rich two-line workspace card.
///
/// Adds branch chip, agent-verb line, and diff counts to `WorkspaceRowPlan`.
/// Built by `build_workspace_card_plan`; painted by `workspace_card.rs`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceCardPlan {
    /// Resolved base row tokens (dot color, bg/fg, flags).
    pub row: WorkspaceRowPlan,
    /// Git branch name for the branch chip. `None` for folder projects or
    /// when the workspace has no branch field.
    pub branch: Option<String>,
    /// Agent state verb + color. `None` only when the workspace has never
    /// started a session and is not live — card omits the verb line.
    pub agent_verb: Option<AgentVerb>,
    /// Display name of the agent running in this worktree (tracked session or
    /// a hand-launched one detected from its terminal title), shown before the
    /// verb on the status line (e.g. "Claude Code · Running"). `None` when no
    /// agent is identifiable — the line shows the verb alone.
    pub agent_name: Option<SharedString>,
    /// The live agent's title — its captured prompt, optionally trailed by the
    /// current activity. When present it leads the status line in place of the
    /// `name · verb` summary (the dot still carries the status), matching the
    /// reference cockpit's prompt-as-title rows. `Some` only for a single-agent
    /// workspace with a live, prompted agent; `None` otherwise (multi-agent
    /// workspaces use the disclosure, dormant ones show the verb).
    pub agent_title: Option<SharedString>,
    /// Working-tree diff counts. `None` when not yet fetched or unavailable;
    /// card omits the `+A −B` chip gracefully.
    pub diff: Option<DiffCounts>,
    /// Tracked files with a diff against HEAD. `None` when not yet measured;
    /// `Some(0)` is a clean tree. The card shows no chip for either, but the
    /// two are distinct here so a test can tell "unknown" from "clean".
    pub dirty_files: Option<u32>,
    /// HEAD against the worktree's base. `None` when unmeasured **or when no
    /// base resolved** — never a zero pair standing in for "unknown".
    pub ahead_behind: Option<AheadBehind>,
    /// GitHub issue/PR reference (e.g. `"#42"`) this workspace was created
    /// from, shown as a small badge. `None` for manually-created workspaces.
    pub linked_issue: Option<String>,
    /// Optional identifier hue, resolved from the workspace's stored swatch
    /// slug. Painted as a thin left-edge accent on the row. `None` = default.
    pub tint: Option<TabColor>,
    /// `true` when this workspace is pinned — the card shows a small pin glyph
    /// and the row floats to the top of its group in every sort mode.
    pub pinned: bool,
    /// The worktree's progress line — what the agent working here says it is
    /// doing — **when it is the line that should lead**. `None` when unset, and
    /// also `None` while a live agent has a prompt of its own: see
    /// [`build_workspace_card_plan`] for why the prompt outranks it.
    pub comment: Option<SharedString>,
    /// The declared work phase, shown as a chip beside the branch. `None` when
    /// unset **or unrecognised** — a phase written by a newer build renders as
    /// no chip rather than as an error or a wrong label.
    pub phase: Option<WorkPhase>,
}

/// Resolve the status-dot color for a workspace given its latest agent
/// session status (or `None` when no sessions have ever started) and
/// whether the workspace currently has a live (open) agent tab.
///
/// Delegates to `agent_verb` so the dot color and the card's verb line
/// are always derived from the same mapping.
pub fn status_dot_color(status: Option<&AgentStatus>, is_live: bool, theme: Theme) -> Hsla {
    agent_verb(status, is_live, theme).color
}

/// Compute the visual plan for one Workspace row. `is_live` is `true`
/// when the workspace has an open agent tab (drives the green idle dot).
#[allow(clippy::too_many_arguments)]
pub fn build_workspace_row_plan(
    workspace: &Workspace,
    is_active: bool,
    is_primary: bool,
    is_folder: bool,
    is_live: bool,
    latest_status: Option<&AgentStatus>,
    theme: Theme,
) -> WorkspaceRowPlan {
    let dot_color = status_dot_color(latest_status, is_live, theme);
    // Active rows become an inset card raised one tier above the rail
    // (`bg_overlay` — `bg_panel_alt` sits BELOW `bg_rail` and would read
    // pressed-in); inactive rows sit flat on the rail surface and only
    // lift on hover.
    let (bg, fg) = if is_active {
        (theme.bg_overlay, theme.fg_base)
    } else {
        (theme.bg_rail, theme.fg_base)
    };
    WorkspaceRowPlan {
        name: workspace.name.clone(),
        slug: workspace.slug.clone(),
        dot_color,
        bg,
        fg,
        fg_sub: theme.fg_subtle,
        is_primary,
        is_folder,
        is_active,
    }
}

/// Compute the rich card plan for one Workspace. Extends `build_workspace_row_plan`
/// with branch, agent verb, and diff counts.
///
/// `diff` is pushed down from the cached concurrent diff-count fetch;
/// `None` means the count is not yet available — the card renders without
/// the `+A −B` chip rather than blocking.
#[allow(clippy::too_many_arguments)]
pub fn build_workspace_card_plan(
    workspace: &Workspace,
    is_active: bool,
    is_primary: bool,
    is_folder: bool,
    is_live: bool,
    latest_status: Option<&AgentStatus>,
    agent_name: Option<SharedString>,
    agent_title: Option<SharedString>,
    stats: Option<&WorktreeStats>,
    theme: Theme,
) -> WorkspaceCardPlan {
    let mut row = build_workspace_row_plan(
        workspace,
        is_active,
        is_primary,
        is_folder,
        is_live,
        latest_status,
        theme,
    );

    // Where this checkout's HEAD is *now*, measured by the refresh round.
    // `workspaces.branch` is written once — at create, adopt or rename — and
    // the synthesized primary row carries the project's *default* branch;
    // neither follows a `git checkout` run in a terminal, so the chip went on
    // naming a branch the worktree had left, and a restart did not fix it
    // because the stale name is what the database holds. The stored name stays
    // the fallback: for the tick before the first measurement lands, and for a
    // detached HEAD, which has no branch to name.
    let live_branch = stats.and_then(|s| s.head_branch.clone());

    // Folder projects carry no branch; linked worktrees always have one.
    let branch = if is_folder {
        None
    } else {
        live_branch
            .clone()
            .or_else(|| (!workspace.branch.is_empty()).then(|| workspace.branch.clone()))
    };

    // The synthesized primary row has no `workspaces` row behind it: its title
    // IS its branch (`rail_data::workspaces_with_primary_for` seeds name, slug
    // and branch alike from the project's default branch). So the title has to
    // follow HEAD too, or the row keeps announcing a branch the checkout left
    // while the chip beside it says otherwise. A real row keeps the name the
    // user gave it — that is a label they chose, not a claim about HEAD.
    if is_primary
        && !is_folder
        && workspace.id.starts_with("primary:")
        && let Some(live) = live_branch.as_ref()
    {
        row.name = live.clone();
    }

    // Produce an agent verb for every workspace that has had any interaction
    // (live or status-bearing). When neither is true, omit the verb line so
    // the card stays compact for dormant workspaces.
    let verb = agent_verb(latest_status, is_live, theme);
    let agent_verb_opt = if latest_status.is_some() || is_live {
        Some(verb)
    } else {
        None
    };

    // Line-2 precedence between the two things that can describe this worktree.
    //
    // A live agent's prompt wins. It is what is happening *now*, and it cannot
    // lie. The progress line is a snapshot with no timestamp — last write wins,
    // no history — so a comment written half an hour ago ("rebasing onto main")
    // keeps asserting itself while the agent has long since moved on to
    // something else. Letting stale prose displace live truth is worse than
    // showing no prose at all.
    //
    // The comment leads exactly where the prompt cannot: a **dormant** worktree,
    // where there is no live agent and no captured prompt, and the authored line
    // is the only thing that can say what state the work was left in. That is
    // also the case this feature exists for.
    let comment = (!workspace.comment.is_empty())
        .then(|| SharedString::from(workspace.comment.clone()))
        .filter(|_| !(is_live && agent_title.is_some()));

    WorkspaceCardPlan {
        row,
        branch,
        agent_verb: agent_verb_opt,
        agent_name,
        agent_title,
        diff: stats.map(|s| s.diff.clone()),
        dirty_files: stats.map(|s| s.dirty_files),
        ahead_behind: stats.and_then(|s| s.ahead_behind.clone()),
        linked_issue: workspace.linked_issue.clone(),
        tint: workspace.tint.as_deref().and_then(TabColor::from_slug),
        pinned: workspace.pinned,
        comment,
        // `parse` yields `None` for an unrecognised value, which is exactly the
        // documented degrade: no chip, never a guess.
        phase: WorkPhase::parse(&workspace.phase),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str, slug: &str) -> Workspace {
        Workspace {
            id: format!("id-{slug}"),
            project_id: "proj".to_string(),
            // Not a branch OxiMux minted: a synthesized row or a
            // fixture. `false` is the reading that never deletes.
            branch_minted: false,
            name: name.to_string(),
            slug: slug.to_string(),
            branch: format!("oximux/{slug}"),
            worktree_path: format!("/tmp/{slug}"),
            status: "active".to_string(),
            created_at: "2026-05-21T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    // ── status_dot_color (delegates to agent_verb) ────────────────────────────

    #[test]
    fn dot_color_none_not_live_uses_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(status_dot_color(None, false, t), t.fg_subtle);
    }

    #[test]
    fn dot_color_none_live_uses_status_ok() {
        // An open (live) session with no concrete status reads green —
        // distinguishes a workspace with an open agent tab from a dormant one.
        let t = Theme::charcoal();
        assert_eq!(status_dot_color(None, true, t), t.status_ok);
    }

    #[test]
    fn dot_color_idle_not_live_uses_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Idle), false, t),
            t.fg_subtle
        );
    }

    #[test]
    fn dot_color_idle_live_uses_status_ok() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Idle), true, t),
            t.status_ok
        );
    }

    #[test]
    fn dot_color_running_wins_over_live_flag() {
        // A concrete status always overrides the live flag.
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Running), true, t),
            t.status_info
        );
    }

    #[test]
    fn dot_color_running_uses_status_info() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Running), false, t),
            t.status_info
        );
    }

    #[test]
    fn dot_color_waiting_uses_status_warn() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::WaitingForInput), false, t),
            t.status_warn
        );
    }

    #[test]
    fn dot_color_needs_approval_uses_status_warn() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::NeedsApproval("permission".into())), false, t),
            t.status_warn
        );
    }

    #[test]
    fn dot_color_done_clean_uses_status_ok() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: Some(0) }), false, t),
            t.status_ok
        );
    }

    #[test]
    fn dot_color_done_with_nonzero_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: Some(1) }), false, t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_done_with_unknown_code_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Done { code: None }), false, t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_failed_uses_status_error() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Failed("boom".into())), false, t),
            t.status_error
        );
    }

    #[test]
    fn dot_color_interrupted_uses_status_muted() {
        let t = Theme::charcoal();
        assert_eq!(
            status_dot_color(Some(&AgentStatus::Interrupted), false, t),
            t.status_muted
        );
    }

    // ── WorkspaceRowPlan ──────────────────────────────────────────────────────

    #[test]
    fn row_plan_active_uses_overlay_bg() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, true, false, false, false, None, t);
        assert_eq!(plan.bg, t.bg_overlay);
        assert_eq!(plan.fg, t.fg_base);
        assert!(plan.is_active);
    }

    #[test]
    fn row_plan_inactive_uses_rail_bg() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, false, false, false, false, None, t);
        assert_eq!(plan.bg, t.bg_rail);
        assert!(!plan.is_active);
    }

    #[test]
    fn row_plan_carries_name_and_slug() {
        let t = Theme::charcoal();
        let w = ws("Fix Login", "fix-login");
        let plan = build_workspace_row_plan(&w, false, false, false, false, None, t);
        assert_eq!(plan.name, "Fix Login");
        assert_eq!(plan.slug, "fix-login");
    }

    #[test]
    fn row_plan_dot_color_reflects_latest_status() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan =
            build_workspace_row_plan(&w, false, false, false, false, Some(&AgentStatus::Running), t);
        assert_eq!(plan.dot_color, t.status_info);
    }

    #[test]
    fn row_plan_live_with_no_status_is_green() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_row_plan(&w, false, false, false, true, None, t);
        assert_eq!(plan.dot_color, t.status_ok);
    }

    #[test]
    fn row_plan_carries_primary_and_folder_flags() {
        let t = Theme::charcoal();
        let w = ws("main", "main");
        let primary = build_workspace_row_plan(&w, false, true, false, false, None, t);
        let folder = build_workspace_row_plan(&w, false, true, true, false, None, t);
        let linked = build_workspace_row_plan(&w, false, false, false, false, None, t);
        assert!(primary.is_primary && !primary.is_folder);
        assert!(folder.is_primary && folder.is_folder);
        assert!(!linked.is_primary && !linked.is_folder);
    }

    // ── WorkspaceCardPlan ─────────────────────────────────────────────────────

    fn ws_with_branch(name: &str, slug: &str, branch: &str) -> Workspace {
        Workspace {
            id: format!("id-{slug}"),
            project_id: "proj".to_string(),
            // Not a branch OxiMux minted: a synthesized row or a
            // fixture. `false` is the reading that never deletes.
            branch_minted: false,
            name: name.to_string(),
            slug: slug.to_string(),
            branch: branch.to_string(),
            worktree_path: format!("/tmp/{slug}"),
            status: "active".to_string(),
            created_at: "2026-05-21T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    #[test]
    fn card_plan_carries_branch_when_present() {
        let t = Theme::charcoal();
        let w = ws_with_branch("Feat", "feat", "oximux/feat");
        let plan = build_workspace_card_plan(&w, false, false, false, false, None, None, None, None, t);
        assert_eq!(plan.branch, Some("oximux/feat".to_string()));
    }

    #[test]
    fn card_plan_branch_absent_for_folder_project() {
        let t = Theme::charcoal();
        let mut w = ws("Folder", "folder");
        w.branch = String::new();
        let plan = build_workspace_card_plan(&w, false, true, true, false, None, None, None, None, t);
        assert!(plan.branch.is_none());
    }

    #[test]
    fn card_plan_branch_absent_when_empty() {
        let t = Theme::charcoal();
        let w = ws_with_branch("Plain", "plain", "");
        let plan = build_workspace_card_plan(&w, false, false, false, false, None, None, None, None, t);
        assert!(plan.branch.is_none());
    }

    /// The defect this pairing exists to close: someone runs `git checkout` in
    /// a terminal and the card goes on naming the branch the row was written
    /// with. The measured HEAD outranks the stored name.
    #[test]
    fn card_plan_branch_follows_live_head_over_the_stored_name() {
        let t = Theme::charcoal();
        let w = ws_with_branch("Feat", "feat", "oximux/feat");
        let plan = build_workspace_card_plan(
            &w, false, false, false, false, None, None, None, Some(&stats_on("main")), t,
        );
        assert_eq!(plan.branch, Some("main".to_string()));
    }

    /// A detached HEAD has no branch to name, so the measurement says nothing
    /// and the stored name — the last branch this row was known to be on — is
    /// what the chip keeps. Blanking it would be a worse answer than a stale
    /// one, and inventing a sha is not a branch.
    #[test]
    fn card_plan_branch_keeps_stored_name_when_head_is_detached() {
        let t = Theme::charcoal();
        let w = ws_with_branch("Feat", "feat", "oximux/feat");
        let measured_detached = stats(0, 0, 0, None);
        let plan = build_workspace_card_plan(
            &w, false, false, false, false, None, None, None, Some(&measured_detached), t,
        );
        assert_eq!(plan.branch, Some("oximux/feat".to_string()));
    }

    /// The synthesized primary row's title has no source but its branch, so it
    /// follows HEAD with the chip.
    #[test]
    fn primary_row_title_follows_live_head() {
        let t = Theme::charcoal();
        let mut w = ws_with_branch("develop", "develop", "develop");
        w.id = "primary:proj".to_string();
        let plan = build_workspace_card_plan(
            &w, false, true, false, false, None, None, None, Some(&stats_on("main")), t,
        );
        assert_eq!(plan.row.name, "main");
        assert_eq!(plan.branch, Some("main".to_string()));
    }

    /// A real row's name is a label its user chose. The chip tells the truth
    /// about HEAD; the title is left alone.
    #[test]
    fn real_row_title_survives_a_branch_switch() {
        let t = Theme::charcoal();
        let w = ws_with_branch("Auth rework", "auth", "oximux/auth");
        let plan = build_workspace_card_plan(
            &w, false, false, false, false, None, None, None, Some(&stats_on("main")), t,
        );
        assert_eq!(plan.row.name, "Auth rework");
        assert_eq!(plan.branch, Some("main".to_string()));
    }

    /// A folder project is not a checkout: no chip, and its title is the
    /// project name whatever a stray measurement might carry.
    #[test]
    fn folder_project_ignores_a_measured_head() {
        let t = Theme::charcoal();
        let mut w = ws_with_branch("Notes", "notes", "");
        w.id = "primary:proj".to_string();
        let plan = build_workspace_card_plan(
            &w, false, true, true, false, None, None, None, Some(&stats_on("main")), t,
        );
        assert!(plan.branch.is_none());
        assert_eq!(plan.row.name, "Notes");
    }

    #[test]
    fn card_plan_agent_verb_none_when_dormant() {
        // No status, not live → no verb line (dormant workspace).
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(&w, false, false, false, false, None, None, None, None, t);
        assert!(plan.agent_verb.is_none());
    }

    #[test]
    fn card_plan_agent_verb_present_when_live() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(&w, false, false, false, true, None, None, None, None, t);
        let verb = plan.agent_verb.expect("live workspace must have verb");
        assert_eq!(verb.label, "Ready");
        assert_eq!(verb.color, t.status_ok);
    }

    #[test]
    fn card_plan_agent_verb_present_when_status_set() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(
            &w,
            false,
            false,
            false,
            false,
            Some(&AgentStatus::Running),
            None,
            None,
            None,
            t,
        );
        let verb = plan.agent_verb.expect("status-bearing workspace must have verb");
        assert_eq!(verb.label, "Running");
        assert_eq!(verb.color, t.status_info);
    }

    #[test]
    fn card_plan_carries_agent_name() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(
            &w,
            false,
            false,
            false,
            true,
            Some(&AgentStatus::Running),
            Some("Claude Code".into()),
            None,
            None,
            t,
        );
        assert_eq!(plan.agent_name.as_deref(), Some("Claude Code"));
    }

    #[test]
    fn card_plan_agent_name_absent_by_default() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(&w, false, false, false, true, None, None, None, None, t);
        assert!(plan.agent_name.is_none());
    }

    #[test]
    fn card_plan_carries_agent_title() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(
            &w,
            false,
            false,
            false,
            true,
            Some(&AgentStatus::Running),
            Some("Claude Code".into()),
            Some("add a readme section".into()),
            None,
            t,
        );
        assert_eq!(plan.agent_title.as_deref(), Some("add a readme section"));
    }

    #[test]
    fn card_plan_agent_title_absent_by_default() {
        let t = Theme::charcoal();
        let w = ws("X", "x");
        let plan = build_workspace_card_plan(&w, false, false, false, true, None, None, None, None, t);
        assert!(plan.agent_title.is_none());
    }

    fn stats(added: u32, removed: u32, files: u32, ab: Option<(u32, u32)>) -> WorktreeStats {
        WorktreeStats {
            diff: DiffCounts { added, removed },
            dirty_files: files,
            ahead_behind: ab.map(|(ahead, behind)| AheadBehind {
                base: "origin/main".into(),
                ahead,
                behind,
            }),
            head_branch: None,
        }
    }

    /// The same fixture with a measured HEAD — the live branch the refresh
    /// round found in the checkout.
    fn stats_on(branch: &str) -> WorktreeStats {
        WorktreeStats {
            head_branch: Some(branch.to_string()),
            ..stats(0, 0, 0, None)
        }
    }

    fn card_with(stats: Option<&WorktreeStats>) -> WorkspaceCardPlan {
        let w = ws("X", "x");
        build_workspace_card_plan(&w, false, false, false, false, None, None, None, stats, Theme::charcoal())
    }

    #[test]
    fn card_plan_carries_every_stat_when_supplied() {
        let plan = card_with(Some(&stats(10, 3, 2, Some((2, 5)))));
        assert_eq!(plan.diff, Some(DiffCounts { added: 10, removed: 3 }));
        assert_eq!(plan.dirty_files, Some(2));
        let ab = plan.ahead_behind.expect("a resolved base must reach the card");
        assert_eq!((ab.base.as_str(), ab.ahead, ab.behind), ("origin/main", 2, 5));
    }

    #[test]
    fn card_plan_stats_absent_when_not_measured() {
        let plan = card_with(None);
        assert!(plan.diff.is_none());
        assert!(plan.dirty_files.is_none());
        assert!(plan.ahead_behind.is_none());
    }

    /// The property the chips rely on: a measured clean tree and a
    /// never-measured one are different plans, even though both paint nothing.
    #[test]
    fn zero_stats_are_present_not_absent() {
        let plan = card_with(Some(&stats(0, 0, 0, Some((0, 0)))));
        assert_eq!(plan.diff, Some(DiffCounts { added: 0, removed: 0 }));
        assert_eq!(plan.dirty_files, Some(0));
        assert_eq!(plan.ahead_behind.as_ref().map(|a| (a.ahead, a.behind)), Some((0, 0)));
        assert_ne!(plan, card_with(None));
    }

    #[test]
    fn an_unresolved_base_is_absent_even_when_the_diff_is_known() {
        let plan = card_with(Some(&stats(1, 1, 1, None)));
        assert!(plan.diff.is_some());
        assert!(plan.ahead_behind.is_none(), "no base must not become ↑0 ↓0");
    }

    #[test]
    fn card_plan_active_variant_uses_overlay_bg() {
        let t = Theme::charcoal();
        let w = ws("Active", "active");
        let plan = build_workspace_card_plan(&w, true, false, false, false, None, None, None, None, t);
        assert_eq!(plan.row.bg, t.bg_overlay);
        assert!(plan.row.is_active);
    }

    #[test]
    fn card_plan_primary_variant_sets_flag() {
        let t = Theme::charcoal();
        let w = ws("main", "main");
        let plan = build_workspace_card_plan(&w, false, true, false, false, None, None, None, None, t);
        assert!(plan.row.is_primary);
        assert!(!plan.row.is_folder);
    }

    // ── sum_numstat ───────────────────────────────────────────────────────────

    #[test]
    fn sum_numstat_empty_map_is_zero() {
        let map = std::collections::HashMap::new();
        assert_eq!(sum_numstat(&map), DiffCounts { added: 0, removed: 0 });
    }

    #[test]
    fn sum_numstat_single_file() {
        let mut map = std::collections::HashMap::new();
        map.insert(std::path::PathBuf::from("a.rs"), (5u32, 2u32));
        assert_eq!(sum_numstat(&map), DiffCounts { added: 5, removed: 2 });
    }

    #[test]
    fn sum_numstat_multi_file_totals() {
        let mut map = std::collections::HashMap::new();
        map.insert(std::path::PathBuf::from("a.rs"), (5u32, 2u32));
        map.insert(std::path::PathBuf::from("b.rs"), (10u32, 1u32));
        map.insert(std::path::PathBuf::from("c.rs"), (0u32, 7u32));
        assert_eq!(sum_numstat(&map), DiffCounts { added: 15, removed: 10 });
    }

    // ── looks_like_renormalization ────────────────────────────────────────────

    /// The shape actually observed on 2026-08-21 against a clean tree.
    #[test]
    fn renormalization_flags_the_observed_shape() {
        assert!(looks_like_renormalization(
            59,
            &DiffCounts { added: 35361, removed: 35361 }
        ));
    }

    /// A big lopsided diff is someone deleting a vendored directory — real work.
    #[test]
    fn renormalization_ignores_asymmetric_totals() {
        assert!(!looks_like_renormalization(
            59,
            &DiffCounts { added: 35361, removed: 35360 }
        ));
    }

    /// Symmetric but small is an ordinary rename-a-symbol edit.
    #[test]
    fn renormalization_ignores_small_symmetric_edits() {
        assert!(!looks_like_renormalization(
            8,
            &DiffCounts { added: 999, removed: 999 }
        ));
    }

    /// One file rewritten wholesale is plausible on its own — a generated file,
    /// a lockfile. It takes several at once to look mechanical.
    #[test]
    fn renormalization_ignores_a_single_file() {
        assert!(!looks_like_renormalization(
            1,
            &DiffCounts { added: 5000, removed: 5000 }
        ));
    }

    /// A clean tree must never trip the heuristic.
    #[test]
    fn renormalization_ignores_a_clean_tree() {
        assert!(!looks_like_renormalization(
            0,
            &DiffCounts { added: 0, removed: 0 }
        ));
    }

    // ── progress: comment + phase ────────────────────────────────────────────

    fn ws_with_progress(comment: &str, phase: &str) -> Workspace {
        let mut w = ws_with_branch("W", "w", "oximux/w");
        w.comment = comment.to_string();
        w.phase = phase.to_string();
        w
    }

    fn card(w: &Workspace) -> WorkspaceCardPlan {
        build_workspace_card_plan(w, false, false, false, false, None, None, None, None, Theme::charcoal())
    }

    #[test]
    fn a_worktree_that_has_said_nothing_carries_no_progress() {
        let plan = card(&ws_with_progress("", ""));
        assert!(plan.comment.is_none(), "an empty comment must not render as an empty line");
        assert!(plan.phase.is_none());
    }

    #[test]
    fn a_progress_line_and_phase_reach_the_card() {
        let plan = card(&ws_with_progress("rebasing onto main", "in-progress"));
        assert_eq!(plan.comment.as_deref(), Some("rebasing onto main"));
        assert_eq!(plan.phase, Some(WorkPhase::InProgress));
    }

    /// The forward-compat contract, at the surface that shows it: a phase this
    /// build does not know renders as *no chip*. Anything else — a default
    /// phase, a raw string chip — would have an older desktop state something
    /// false about a worktree a newer one is managing.
    #[test]
    fn an_unrecognised_phase_shows_no_chip_rather_than_guessing() {
        let plan = card(&ws_with_progress("", "shipped"));
        assert!(plan.phase.is_none(), "an unknown phase must not become a label");
    }

    /// The comment is kept even when the phase is unreadable — one unknown
    /// field must not suppress the other, which is the whole point of carrying
    /// them independently.
    #[test]
    fn an_unrecognised_phase_does_not_suppress_the_comment() {
        let plan = card(&ws_with_progress("still working", "shipped"));
        assert_eq!(plan.comment.as_deref(), Some("still working"));
        assert!(plan.phase.is_none());
    }

    /// The synthesized primary row has no `workspaces` row behind it, so it can
    /// never carry progress. Pinned here because the card would happily render
    /// a comment for it if one ever appeared.
    #[test]
    fn every_phase_in_the_vocabulary_reaches_the_card() {
        for phase in WorkPhase::ALL {
            let plan = card(&ws_with_progress("x", phase.as_str()));
            assert_eq!(plan.phase, Some(phase), "`{}` must survive to the card", phase.as_str());
        }
    }

    // ── line-2 precedence: live truth vs authored snapshot ───────────────────

    fn card_live(w: &Workspace, is_live: bool, title: Option<&str>) -> WorkspaceCardPlan {
        build_workspace_card_plan(
            w,
            false,
            false,
            false,
            is_live,
            None,
            None,
            title.map(SharedString::from),
            None,
            Theme::charcoal(),
        )
    }

    /// A live agent's prompt outranks the stored progress line. The comment has
    /// no timestamp, so an old one keeps asserting itself long after the agent
    /// moved on — stale prose displacing live truth is worse than no prose.
    #[test]
    fn a_live_agents_prompt_outranks_a_stored_progress_line() {
        let w = ws_with_progress("rebasing onto main", "in-progress");
        let plan = card_live(&w, true, Some("fix the parser"));
        assert!(
            plan.comment.is_none(),
            "a live prompt must take the line, or a stale comment overwrites live truth"
        );
        assert_eq!(plan.agent_title.as_deref(), Some("fix the parser"));
        // The phase chip is unaffected — it is a chip on line 1, and a declared
        // phase stays useful next to a live prompt.
        assert_eq!(plan.phase, Some(WorkPhase::InProgress));
    }

    /// The case the feature exists for: nothing is running, so there is no
    /// prompt and no activity, and the authored line is the only thing that can
    /// say what state the work was left in.
    #[test]
    fn a_dormant_worktree_leads_with_its_progress_line() {
        let w = ws_with_progress("3 conflicts left, see notes", "in-review");
        let plan = card_live(&w, false, None);
        assert_eq!(plan.comment.as_deref(), Some("3 conflicts left, see notes"));
    }

    /// Live but with no prompt captured — there is nothing to outrank, so the
    /// comment still leads rather than the row falling back to a bare verb.
    #[test]
    fn a_live_agent_without_a_prompt_does_not_suppress_the_comment() {
        let w = ws_with_progress("running the suite", "in-progress");
        let plan = card_live(&w, true, None);
        assert_eq!(plan.comment.as_deref(), Some("running the suite"));
    }

    /// Suppression is display-only and must not be mistaken for a delete: the
    /// same worktree read again once its agent stops shows the line again.
    #[test]
    fn suppressing_the_comment_while_live_does_not_discard_it() {
        let w = ws_with_progress("waiting on review", "in-review");
        assert!(card_live(&w, true, Some("do the thing")).comment.is_none());
        assert_eq!(
            card_live(&w, false, None).comment.as_deref(),
            Some("waiting on review"),
            "the comment must return once the agent stops"
        );
    }
}
