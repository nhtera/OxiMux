//! Inline commit composer mounted inside the Source Control panel.
//!
//! Single multi-line `Message` textarea plus a full-width primary action
//! button. The primary's label/icon adapts via the resolved `PrimaryAction`
//! (Commit / Stage Files / Push / Pull / Sync / Publish Branch).
//!
//! Single-flight: an `AtomicBool` guards re-entry so a fast double-click only
//! triggers one commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{
    Anchor, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants, DropdownButton},
    input::{Input, InputEvent, InputState},
    menu::PopupMenuItem,
};
use oximux_core::FileStatus;
use oximux_git::Repository;
use oximux_settings::{CommitMessageAiMode, CommitMessageAiSettings, Density, Theme, Typography};
use oximux_storage::WorktreeSettingsRepo;

/// Conventional-commit prefix shortcuts surfaced as one-click chips
/// above the message textarea. Click prepends the prefix + `: ` to the
/// current message (or leaves a stub `feat: ` when the box is empty),
/// so the user gets the most common formatting for free without
/// memorising the convention. Order mirrors the relative frequency in
/// typical workflow: feat > fix > chore > docs > refactor.
const COMMIT_PREFIXES: &[&str] = &["feat", "fix", "chore", "docs", "refactor"];

/// Soft warning threshold for the subject character counter. Subjects
/// over ~50 characters get harder to read in `git log --oneline`; the
/// counter shifts to `status_warning` at this length to give a visual
/// nudge without blocking submission.
const SUBJECT_WARN_CHARS: usize = 50;

/// Hard target ceiling for commit subjects. The conventional 72-char
/// cap keeps subjects from being truncated in standard git tooling
/// (GitHub PR titles, `git log` columnar output). The counter turns
/// `status_error` past this point — still allowed, but visibly hot.
const SUBJECT_MAX_CHARS: usize = 72;

/// Coalesce per-keystroke commit_draft upserts. Drops two writes for
/// every keystroke under the threshold (typing speed of ~4 keys/second
/// → at most one upsert per ~250 ms typing burst). Tuned so the user
/// pausing for a beat (e.g. to read the diff) commits the in-progress
/// draft to disk before they start typing again.
const DRAFT_DEBOUNCE: Duration = Duration::from_millis(250);

use crate::shell::source_control::ai_generation::AiState;
use crate::shell::source_control::ai_overlay;
use crate::shell::source_control::dropdown_items::{
    self, DropdownActionKind, DropdownEntry, DropdownInputs,
};
use crate::shell::source_control::primary_action::{PrimaryAction, PrimaryActionKind};
use crate::shell::source_control::settings_persistence::load_initial_commit_draft;
use crate::shell::source_control::style as sc_style;

/// Last status surfaced from a commit / remote operation. Mirrors the
/// primary-action lifecycle in the panel header so the user sees the
/// op-in-flight even when the dropdown chevron triggered it (not just the
/// main split button).
///
/// `Failed(label, message)` includes the failing op's verb so the toast
/// reads "Push failed: …" rather than the generic "Commit failed". Reset
/// to `Idle` on the next successful op.
#[derive(Debug, Clone, Default)]
pub enum CommitStatus {
    #[default]
    Idle,
    Committing,
    Pushing,
    Pulling,
    Syncing,
    Fetching,
    /// `git cherry-pick <sha>` in flight. Surfaced by the commit-row
    /// right-click → Cherry-pick item. Reaches `Idle` on success or
    /// `Failed("cherry-pick", …)` on conflict (the operation banner
    /// covers the recovery surface via `current_op`).
    CherryPicking,
    /// `git revert --no-edit <sha>` in flight. Same lifecycle as
    /// `CherryPicking` — conflict drops into the operation banner.
    Reverting,
    /// `git rebase <base>` in flight. Same lifecycle as `CherryPicking` —
    /// a conflicted rebase drops into the operation banner via
    /// `current_op`.
    Rebasing,
    /// `git <op> --abort` in flight (operation-banner recovery). Reaches
    /// `Idle` on success — the next poll clears `current_op` and the banner
    /// disappears.
    Aborting,
    /// `git <op> --continue` in flight (operation-banner recovery). Reaches
    /// `Idle` on success or `Failed("continue", …)` when conflicts remain
    /// unstaged.
    Continuing,
    /// `gh pr create` in flight. Reaches `Idle` on success (the PR opens in
    /// the browser) or `Failed("create PR", …)` — most usefully when a PR for
    /// the branch already exists.
    CreatingPr,
    /// `gh pr merge <method>` in flight. Reaches `Idle` on success (the PR
    /// merges) or `Failed("merge PR", …)` — e.g. not mergeable / checks pending.
    MergingPr,
    Failed(String, String),
}

/// Events the composer emits to its owning panel. The Create-PR flow needs a
/// modal the composer's small render slot can't host, so it asks the panel to
/// open one rather than acting itself.
pub enum CommitAreaEvent {
    /// User chose "Create PR" — the panel should open the compose dialog.
    CreatePrRequested,
}

pub struct CommitArea {
    // `pub(in crate::shell::source_control)` on the fields the sibling
    // `commit_ops` module pokes. Restricting visibility this way (rather
    // than `pub(crate)`) keeps the fields out of the wider app surface
    // while still letting the in-module helper module reach them.
    pub(in crate::shell::source_control) repo: Repository,
    pub message_state: Entity<InputState>,
    pub status: CommitStatus,
    pub(in crate::shell::source_control) in_flight: Arc<AtomicBool>,
    /// Set true after a successful `gh pr create` so the panel's state
    /// observer forces an immediate PR-status refresh (otherwise the cached
    /// `has_open_pr` would stay stale for up to the 30s throttle and the
    /// button would wrongly re-offer Create PR). The observer reads + clears it.
    pub(in crate::shell::source_control) pr_status_dirty: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Drop cancels the in-flight commit observer task.
    pub(in crate::shell::source_control) _commit_task: Option<Task<()>>,

    /// Per-worktree V006 settings repo. `None` in test wiring; the
    /// production path always supplies it. When absent, the textarea
    /// works in-memory but drafts don't survive a restart.
    worktree_settings_repo: Option<WorktreeSettingsRepo>,
    /// Cached `repo.workdir().to_string_lossy().to_string()` — the
    /// debounced task only needs a string key, not a live `Repository`
    /// handle, so capturing it once at construction avoids re-deriving
    /// inside the async closure on every keystroke.
    workspace_id: String,
    /// Currently-pending debounced commit_draft writer. Overwriting
    /// the field drops the prior task (cancelling it before its timer
    /// fires), so a 100-keystroke burst writes exactly once — 250 ms
    /// after the user stops typing.
    _draft_debounce: Option<Task<()>>,
    /// Held for the lifetime of `CommitArea` so the message-input
    /// change observer keeps firing. The observer drives BOTH the
    /// 250 ms debounced draft persistence AND the AI-generation
    /// race-protection cancel — keeping a single InputEvent
    /// subscription means user typing always cancels in-flight AI
    /// generation, even when no settings repo is plugged (test
    /// wiring). `Option` is kept for API stability with the
    /// pre-Phase-14 shape; it is now always `Some` in practice.
    _draft_subscription: Option<Subscription>,

    /// Snapshot of staged files pushed in by the panel's state
    /// observer on each poll tick. Read by the sparkles button to
    /// gate enablement and by the generation task to feed the
    /// heuristic. Empty when nothing is staged.
    pub(in crate::shell::source_control) staged_snapshot: Vec<FileStatus>,

    /// Resolved base ref for the Rebase dropdown row (e.g. `origin/main`),
    /// pushed in by the panel's state observer alongside `staged_snapshot`.
    /// The dropdown LABEL resolves its own base ref per-render from the
    /// panel; this mirror exists only so `rebase_from_base` — dispatched
    /// from inside `CommitArea` where the panel's `base_ref` isn't reachable
    /// — knows which ref to rebase onto. `None` when no base is configured
    /// (the row is disabled in that case, so the op never fires).
    pub(in crate::shell::source_control) rebase_base: Option<String>,

    /// Which forge backs the repo, mirrored in by the panel's PR-status
    /// refresh. Drives the Create-PR button's brand glyph (GitLab repos
    /// get the GitLab mark instead of a GitHub one). `None` until the
    /// first detection lands — Create-PR is gated on a detected forge,
    /// so the default GitHub glyph can only flash on a GitHub repo.
    pub(in crate::shell::source_control) forge_kind: Option<crate::shell::forge::ForgeKind>,

    /// AI generation lifecycle for the sparkles button. `Idle` until
    /// the user clicks; `Generating` while the spawned task is
    /// computing + applying. Cancellation is cooperative through the
    /// stored flag — the task checks it before mutating the textarea.
    pub(in crate::shell::source_control) ai_state: AiState,
}

impl EventEmitter<CommitAreaEvent> for CommitArea {}

impl CommitArea {
    pub fn new(
        repo: Repository,
        worktree_settings_repo: Option<WorktreeSettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_id = repo.workdir().to_string_lossy().to_string();
        // Pre-populate the textarea with the persisted draft, if any.
        // `default_value` writes the rope directly without firing
        // `InputEvent::Change`, so the subscription set up below doesn't
        // immediately re-upsert the very value we just loaded.
        let initial_draft =
            load_initial_commit_draft(worktree_settings_repo.as_ref(), &workspace_id);
        let message_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Message");
            if let Some(draft) = initial_draft {
                state = state.default_value(draft);
            }
            state
        });
        // Subscribe to keystrokes — every `Change` schedules a 250 ms
        // debounced upsert of the draft. Skip the wiring entirely when
        // no settings repo is plugged (test wiring); the textarea
        // still works, the value just doesn't persist.
        // Single Change observer drives both the persistence debounce
        // AND the AI-generation race-protection. Programmatic writes
        // via `InputState::set_value` toggle `emit_events = false` for
        // the duration of the write (see gpui-component
        // `state.rs::set_value`), so every Change we see here is a
        // user-typed keystroke — exactly the signal the race-protect
        // requirement wants. Subscribing once both ways keeps a single
        // entry point for "user typed into the textarea".
        //
        // Subscription is mounted even when no settings repo is
        // plugged (test wiring): the AI race-protect still matters
        // and `schedule_draft_save` is internally a no-op without a
        // repo.
        let _draft_subscription = Some(cx.subscribe_in(
            &message_state,
            window,
            |area, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    // Drop any in-flight generation — the user is
                    // typing now, so the heuristic result would
                    // overwrite their input. Cheap when not
                    // generating (a single enum match).
                    area.ai_state.signal_cancel();
                    let value = input.read(cx).value().to_string();
                    area.schedule_draft_save(value, cx);
                }
            },
        ));
        Self {
            repo,
            message_state,
            status: CommitStatus::Idle,
            in_flight: Arc::new(AtomicBool::new(false)),
            pr_status_dirty: false,
            theme,
            density,
            typography,
            _commit_task: None,
            worktree_settings_repo,
            workspace_id,
            _draft_debounce: None,
            _draft_subscription,
            staged_snapshot: Vec::new(),
            rebase_base: None,
            forge_kind: None,
            ai_state: AiState::Idle,
        }
    }

    /// Schedule a 250 ms-debounced upsert of the commit_draft column.
    /// Replacing `_draft_debounce` drops the previous task (cancelling
    /// it before its timer fires), so a 100-keystroke burst writes
    /// exactly once — 250 ms after the user stops typing.
    ///
    /// Empty strings are coerced to `None` so a manually-cleared
    /// textarea actually clears the column (vs. persisting `""`
    /// forever). Persistence errors are logged but never raised —
    /// same fail-safe policy as `load_initial_base_ref`: a UX nicety,
    /// not a panel invariant.
    pub(in crate::shell::source_control) fn schedule_draft_save(
        &mut self,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.worktree_settings_repo.clone() else {
            return;
        };
        let workspace_id = self.workspace_id.clone();
        let to_write: Option<String> = if value.is_empty() { None } else { Some(value) };
        self._draft_debounce = Some(cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(DRAFT_DEBOUNCE).await;
            if let Err(err) = repo.modify(&workspace_id, |s| s.commit_draft = to_write) {
                tracing::warn!(
                    target: "oximux_app::commit_area",
                    error = %err,
                    workspace_id = %workspace_id,
                    "commit_draft upsert failed; draft will reset to last-persisted value on restart",
                );
            }
        }));
    }

    /// True when the message field currently holds non-whitespace text.
    pub fn has_message(&self, cx: &gpui::App) -> bool {
        !self.message_state.read(cx).value().trim().is_empty()
    }

    /// Focus the message input. Called by Cmd+K routing.
    pub fn focus_subject(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.message_state.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Sibling-callable focus handle for the message input.
    pub fn subject_focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        use gpui::Focusable;
        self.message_state.read(cx).focus_handle(cx)
    }

    /// Submit if the primary action says we can. Caller passes the resolved
    /// `PrimaryAction` so we only run when the rendered button would be
    /// enabled — keeps the keyboard path (Cmd+Enter, future binding) honest.
    ///
    /// `&mut Window` is plumbed through so the auto-clear of the textarea
    /// after a successful commit (`set_value` requires Window) can fire
    /// inside the completion path's `update_in` block.
    pub fn submit(&mut self, action: &PrimaryAction, window: &mut Window, cx: &mut Context<Self>) {
        if action.disabled || action.kind != PrimaryActionKind::Commit {
            return;
        }
        super::commit_ops::run_commit(self, super::commit_ops::CommitFollowup::None, window, cx);
    }

    /// Commit-then-push convenience used by the dropdown's "Commit & Push"
    /// item. Push only fires on a successful commit; partial failure
    /// surfaces via `CommitStatus::Failed("commit"/"push", …)`.
    pub fn commit_and_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::commit_ops::run_commit(self, super::commit_ops::CommitFollowup::Push, window, cx);
    }

    /// Commit-then-sync (pull + push). Same single-flight + status surface
    /// as `commit_and_push`.
    pub fn commit_and_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::commit_ops::run_commit(self, super::commit_ops::CommitFollowup::Sync, window, cx);
    }

    /// Standalone `git push` — used by the dropdown's Push item and the
    /// primary action when `PrimaryActionKind::Push` resolves.
    pub fn push(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Push, cx);
    }

    /// Standalone `git pull --ff-only`.
    pub fn pull(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Pull, cx);
    }

    /// Fast-forward-only pull. Shares the `--ff-only` backend with [`Self::pull`]
    /// — the dropdown surfaces it as its own row (enabled only when the branch
    /// can fast-forward), but the underlying op is identical until a
    /// merge/rebase pull path lands.
    pub fn fast_forward(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Pull, cx);
    }

    /// Standalone `git pull --ff-only && git push`.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Sync, cx);
    }

    /// `git fetch --all --prune`. Low-risk — never mutates the working
    /// tree, only updates remote-tracking refs.
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Fetch, cx);
    }

    /// Standalone `git push --force-with-lease`. The lease guard aborts the
    /// push (rather than overwriting) if the remote moved since the last
    /// fetch, so a teammate's surprise upstream commits surface as a
    /// rejection in the status row instead of being silently discarded.
    pub fn force_push(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::ForcePush, cx);
    }

    /// Commit-then-force-push convenience used by the dropdown's
    /// "Commit & Force Push" item when the local branch has rewritten
    /// history a plain push would reject. Force-push only fires on a
    /// successful commit; partial failure surfaces via
    /// `CommitStatus::Failed("commit"/"force push", …)`.
    pub fn commit_and_force_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::commit_ops::run_commit(
            self,
            super::commit_ops::CommitFollowup::ForcePush,
            window,
            cx,
        );
    }

    /// Rebase the current branch onto the configured base ref
    /// (`git rebase <base>`). Reads the base from `rebase_base` — the panel's
    /// state observer mirrors the resolved value in. A conflicted rebase
    /// leaves the worktree in rebase state and surfaces through the
    /// operation banner (same lifecycle as cherry-pick / revert).
    ///
    /// The dropdown disables the Rebase row when no base is configured, so
    /// `rebase_base` is normally `Some` here; the `None` guard is defensive
    /// (e.g. a click racing a base-ref clear) and surfaces a clear reason
    /// rather than a raw git error.
    pub fn rebase_from_base(&mut self, cx: &mut Context<Self>) {
        let Some(base) = self.rebase_base.clone() else {
            self.status =
                CommitStatus::Failed("rebase".to_string(), "No base ref configured".to_string());
            cx.notify();
            return;
        };
        super::commit_ops::run_commit_verb(self, super::commit_ops::CommitVerb::Rebase(base), cx);
    }

    /// Publish the current branch by pushing it with `-u origin <branch>`.
    /// Currently routed through the `Publish` remote verb in `commit_ops`,
    /// which uses the existing `Repository::publish_branch` backend.
    pub fn publish(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Publish, cx);
    }

    /// Request the Create-PR dialog. Emits a `CreatePrRequested` event the
    /// owning panel observes to open the compose dialog (which then runs the
    /// forge `create_pr`). The composer doesn't own the dialog itself — it
    /// renders in a small slot, so the panel mounts the modal instead.
    pub fn create_pr(&mut self, cx: &mut Context<Self>) {
        cx.emit(CommitAreaEvent::CreatePrRequested);
    }

    /// Merge the current branch's open PR with the chosen method via the forge.
    /// On success the PR closes and the status row clears; the panel's next poll
    /// flips the PR/CI surface (forced via `pr_status_dirty`).
    pub fn merge_pr(
        &mut self,
        method: crate::shell::forge::MergeMethod,
        cx: &mut Context<Self>,
    ) {
        super::pr_ops::run_merge_pr(self, method, cx);
    }

    /// Abort the in-progress git operation (from the operation banner's
    /// Abort button). Discards the partial merge/rebase/cherry-pick/revert
    /// and returns the worktree to its pre-op state; the next poll clears
    /// the banner.
    pub fn abort_operation(&mut self, op: oximux_core::GitOperation, cx: &mut Context<Self>) {
        super::commit_ops::run_op_recovery(
            self,
            super::commit_ops::OperationRecovery::Abort(op),
            cx,
        );
    }

    /// Continue the in-progress sequencer operation after the user staged
    /// their conflict resolutions (from the operation banner's Continue
    /// button). Only wired for ops where `supports_continue()` is true.
    pub fn continue_operation(&mut self, op: oximux_core::GitOperation, cx: &mut Context<Self>) {
        super::commit_ops::run_op_recovery(
            self,
            super::commit_ops::OperationRecovery::Continue(op),
            cx,
        );
    }

    /// Apply a completed op result to the status surface. Called from
    /// the commit-ops completion task; `pub(super)` so the helper
    /// module can reach it without re-exposing the fields.
    ///
    /// `window` is `Some` only on the commit-completion path
    /// (`run_commit`'s `spawn_in`-rooted oneshot), `None` on
    /// remote-only paths (`run_remote`: push, pull, sync, fetch,
    /// publish). When `Some` AND the op succeeded:
    ///
    /// 1. the textarea is cleared via `InputState::set_value("", window, cx)`,
    /// 2. the SQLite-persisted draft is explicitly cleared via
    ///    `schedule_draft_save(String::new(), cx)`.
    ///
    /// Step 2 is mandatory: `InputState::set_value` toggles
    /// `emit_events = false` for the duration of the write (see
    /// gpui-component `state.rs::set_value`), so the
    /// `InputEvent::Change` observer set up in `new` does NOT fire
    /// from a programmatic clear. Without the explicit
    /// `schedule_draft_save` the in-memory textarea would empty but
    /// the SQLite `commit_draft` column would still hold the
    /// just-committed message — the next panel mount would ghost
    /// the committed text back into the composer.
    pub(super) fn apply_result(
        &mut self,
        result: Result<&'static str, (&'static str, String)>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_label) => {
                self.status = CommitStatus::Idle;
                if let Some(window) = window {
                    self.message_state
                        .update(cx, |s, cx| s.set_value("", window, cx));
                    // Mirror the in-memory clear to disk; the Change
                    // observer is suppressed during set_value.
                    self.schedule_draft_save(String::new(), cx);
                }
            }
            Err((label, error)) => {
                // Transient cross-surface alert in addition to the inline
                // status row, so a failure that scrolls off or happens while
                // the panel is unfocused still registers.
                crate::shell::toast::toast(
                    cx,
                    crate::shell::toast::ToastKind::Error,
                    format!("{label} failed"),
                );
                self.status = CommitStatus::Failed(label.to_string(), error);
            }
        }
    }

    pub fn render(
        &self,
        action: &PrimaryAction,
        dropdown_inputs: DropdownInputs,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let status_row = render_status_row(theme, typography, &self.status);
        let action_for_click = action.clone();
        let submit_label = action.label.clone();
        let submit_title = action.title.clone();
        let submit_disabled = action.disabled;
        let primary_icon = primary_icon_for(action.kind, self.forge_kind);

        // Relative wrapper anchors both the sparkles button (top-right)
        // and the AI overlay (full-area scrim during generation).
        // Textarea sits on `bg_base` (darker than the surrounding
        // `bg_panel`) so it reads as an inset field rather than a
        // floating chip.
        //
        // Height is enforced on BOTH the wrapper (`.h(COMMIT_H)`) and the
        // inner Input (`.h_full()`); without the wrapper bound, the multi-
        // line Input would expand to the available column slack and the
        // composer would balloon to fill the panel.
        //
        // Sparkles enable rules:
        //  - HIDDEN entirely when settings have `mode = "off"` —
        //    disabled-with-tooltip would imply "you could click
        //    this" when actually nothing's wired.
        //  - disabled while committing (`in_flight`),
        //  - disabled while a generation is already running,
        //  - disabled when nothing is staged (empty snapshot),
        //  - disabled when the textarea already has user content
        //    (don't overwrite). Future: Cmd+click bypass for
        //    force-overwrite.
        let ai_off = self.ai_disabled_by_settings(cx);
        let staged_count = self.staged_snapshot.len();
        let has_user_text = !self.message_state.read(cx).value().trim().is_empty();
        let generating = self.ai_state.is_generating();
        let committing = self.in_flight.load(Ordering::Relaxed);
        let sparkles_disabled =
            committing || generating || staged_count == 0 || has_user_text;
        let sparkles_tooltip = if generating {
            "Generating commit message…"
        } else if committing {
            "Commit in flight"
        } else if staged_count == 0 {
            "Stage at least one file"
        } else if has_user_text {
            "Clear the message first"
        } else {
            "Generate commit message"
        };
        // Agent-mode sublabel: when the user has configured an agent
        // (claude / codex / custom) in `commit_message_ai.toml`, show
        // its name as a tiny muted chip beneath the sparkles button so
        // the active backend is visible at a glance — no need to
        // hover the tooltip to discover which model the click will run.
        // Heuristic mode → no sublabel (the local generator is the
        // default; surfacing "heuristic" everywhere reads as noise).
        // Off mode → entire button is hidden by `ai_off`.
        let agent_sublabel = cx
            .try_global::<CommitMessageAiSettings>()
            .and_then(|s| match s.mode {
                CommitMessageAiMode::Agent => {
                    let raw = s.agent.agent_id.trim();
                    if raw.is_empty() {
                        None
                    } else {
                        let mut chars = raw.chars();
                        let first = chars.next().map(|c| c.to_uppercase().to_string());
                        Some(format!(
                            "{}{}",
                            first.unwrap_or_default(),
                            chars.as_str()
                        ))
                    }
                }
                _ => None,
            });
        let sparkles_button = (!ai_off).then(|| {
            div()
                .absolute()
                .top(px(6.0))
                .right(px(6.0))
                .flex()
                .flex_col()
                .items_end()
                .gap(px(1.0))
                .child(
                    Button::new("sc-ai-message")
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::default()
                                .path("icons/sparkles.svg")
                                .size(px(sc_style::ICON)),
                        )
                        .tooltip(sparkles_tooltip)
                        .disabled(sparkles_disabled)
                        .on_click(cx.listener(|area, _: &ClickEvent, window, cx| {
                            area.start_ai_generation(window, cx);
                        })),
                )
                .when_some(agent_sublabel, |col, name| {
                    col.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.fg_subtle)
                            .child(name),
                    )
                })
        });
        let textarea = div()
            .relative()
            .w_full()
            .h(px(sc_style::COMMIT_H))
            .flex_shrink_0()
            .border_1()
            .border_color(theme.border_inactive)
            .rounded(px(density.r_xs))
            .bg(theme.bg_base)
            .text_size(px(sc_style::BODY_TEXT))
            .child(Input::new(&self.message_state).h_full())
            .children(sparkles_button)
            .children(ai_overlay::render_ai_overlay(
                generating,
                theme,
                cx.listener(|area, _: &ClickEvent, _window, cx| {
                    area.cancel_ai_generation(cx);
                }),
            ));

        let mut inner_button = Button::new("source-control-primary-inner")
            .label(submit_label)
            .tooltip(submit_title)
            .on_click(cx.listener(move |area, _: &ClickEvent, window, cx| {
                area.submit(&action_for_click, window, cx);
                cx.notify();
            }))
            .flex_1();
        if let Some(icon) = primary_icon {
            inner_button = inner_button.icon(icon);
        }

        // Chevron opens a dropdown built from `dropdown_items::resolve`.
        // Each row carries its own label / tooltip / disabled state; the
        // render layer just walks the resolved Vec and emits one
        // `PopupMenuItem` per `Item` + a separator line per `Separator`.
        let action_view = cx.entity();
        // Resolve once up front so the menu builder closure doesn't need to
        // re-resolve on each open (and so the resolution sees the same
        // inputs as the rest of this render call).
        let resolved_entries = dropdown_items::resolve(&dropdown_inputs);
        // DropdownButton handles the shared borders + unified variant so the
        // chevron half can't render brighter than the disabled main half.
        // `.small()` brings the chunky default-size pill down to ~32px tall
        // so it sits proportional to the textarea above.
        //
        // Variant switch, keyed on the resolved action kind so weight tracks
        // meaning:
        //   - Disabled → `.secondary()`. A `.primary()` button keeps its bright
        //     fill even when disabled, reading as "active"; the dimmed
        //     secondary matches the gray disabled-Commit state users expect.
        //   - Commit (enabled) → `.primary()`. Solid fill is reserved for the
        //     single true affirmative of the flow, so it still pops.
        //   - Every other verb (Stage / Push / Pull / Sync / Publish / Create
        //     PR) → `.outline()`. These are frequent, non-terminal steps; a
        //     solid-white box per verb made the panel shout. Quiet outline
        //     keeps them present without dominating.
        let mut primary = DropdownButton::new("sc-commit-split")
            .button(inner_button)
            .small()
            .disabled(submit_disabled)
            .w_full();
        primary = if submit_disabled {
            primary.secondary()
        } else if action.kind == PrimaryActionKind::Commit {
            primary.primary()
        } else {
            primary.outline()
        };
        // Captured for the dropdown rows so enabled verbs read at full weight
        // and disabled verbs recede with their reason as a quiet hint.
        let menu_theme = theme;
        let menu_label_weight = typography.w_medium;
        let primary =
            primary.dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, _cx| {
                build_commit_menu(
                    menu,
                    window,
                    &action_view,
                    &resolved_entries,
                    menu_theme,
                    menu_label_weight,
                )
            });

        // Conventional-prefix shortcut row + subject character counter
        // share one slim strip above the textarea. Chips stay on the
        // left (frequent action), counter pins to the right via
        // `ml_auto` (informational). The strip stays mounted regardless
        // of message contents so the chips and counter never re-flow
        // as the user types.
        let subject_chars = self
            .message_state
            .read(cx)
            .value()
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .count();
        let counter_color = if subject_chars > SUBJECT_MAX_CHARS {
            theme.status_error
        } else if subject_chars > SUBJECT_WARN_CHARS {
            theme.status_warning
        } else {
            theme.fg_subtle
        };
        let mut prefix_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(sc_style::ICON_CLUSTER_GAP))
            .w_full();
        for (idx, prefix) in COMMIT_PREFIXES.iter().enumerate() {
            let prefix_static: &'static str = prefix;
            prefix_row = prefix_row.child(
                Button::new(("sc-commit-prefix", idx))
                    .ghost()
                    .xsmall()
                    .label(format!("{prefix_static}:"))
                    .tooltip(format!(
                        "Prepend `{prefix_static}: ` to the commit message"
                    ))
                    .on_click(cx.listener(move |area, _: &ClickEvent, window, cx| {
                        area.prepend_prefix(prefix_static, window, cx);
                    })),
            );
        }
        let meta_row = prefix_row.child(
            div()
                .ml_auto()
                .text_size(px(9.0))
                .text_color(counter_color)
                .child(format!("{subject_chars} / {SUBJECT_MAX_CHARS}")),
        );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .px(px(sc_style::PAD_H))
            .pt(px(sc_style::PAD_V))
            .pb(px(sc_style::PAD_V_TIGHT))
            .gap(px(sc_style::PAD_V_TIGHT))
            .child(meta_row)
            .child(textarea)
            .child(primary)
            .children(status_row)
    }

    /// Prepend a conventional-commit prefix (`feat`, `fix`, …) to the
    /// current message contents. No-op if the message already starts
    /// with the same prefix (avoids stacking `feat: feat: …`). Called
    /// from the meta-row chip click handlers; kept as a method rather
    /// than an inline closure so the click site stays terse.
    pub(in crate::shell::source_control) fn prepend_prefix(
        &mut self,
        prefix: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.message_state.read(cx).value().to_string();
        let stamp = format!("{prefix}: ");
        if current.starts_with(&stamp) {
            return;
        }
        let new_val = if current.is_empty() {
            stamp
        } else {
            format!("{stamp}{current}")
        };
        self.message_state
            .update(cx, |s, cx| s.set_value(&new_val, window, cx));
    }
}

/// Chevron dropdown items: every entry comes from the resolved
/// `Vec<DropdownEntry>`. Per-row label, tooltip, and disabled state are
/// owned by `dropdown_items::resolve`; this function only wires each
/// `DropdownActionKind` to the matching `CommitArea` method and turns
/// `Separator` into a divider line.
///
/// Every actionable row wires to a real backend op on `CommitArea`. The
/// two review verbs (Create PR / Push before PR) are intentionally inert
/// in v1 — their dispatch arms no-op until the hosted-review adapter lands.
fn build_commit_menu(
    mut menu: gpui_component::menu::PopupMenu,
    window: &mut Window,
    view: &Entity<CommitArea>,
    entries: &[DropdownEntry],
    theme: Theme,
    label_weight: FontWeight,
) -> gpui_component::menu::PopupMenu {
    menu = menu.min_w(px(224.0));
    for entry in entries {
        match entry {
            DropdownEntry::Separator => {
                menu = menu.separator();
            }
            DropdownEntry::Item {
                kind,
                label,
                title,
                disabled,
            } => {
                menu = menu.item(build_menu_item(
                    window,
                    view,
                    *kind,
                    label,
                    title,
                    *disabled,
                    theme,
                    label_weight,
                ));
            }
        }
    }
    menu
}

/// Map one resolved entry into a styled `PopupMenuItem`.
///
/// Hierarchy (modeled on VS Code / the reference UX git menus):
/// - **Enabled** verbs carry a medium weight and the menu's default bright
///   foreground (no explicit color, so hover still recolors them) — the
///   actionable rows are the visual focus. Counts ride inline in the label
///   (`Push (1)`, `Sync (↓0 ↑1)`) from the resolver.
/// - **Disabled** verbs recede to `fg_muted`; the "why not" rides a hover
///   **tooltip** rather than crowding every row. The two review verbs (Create
///   PR / Push before PR) are the exception — their path-to-enable isn't
///   obvious from the label, so the reason also shows as an inline `fg_subtle`
///   sub-line.
fn build_menu_item(
    window: &mut Window,
    view: &Entity<CommitArea>,
    kind: DropdownActionKind,
    label: &str,
    title: &str,
    disabled: bool,
    theme: Theme,
    label_weight: FontWeight,
) -> PopupMenuItem {
    let label = label.to_string();
    let title = title.to_string();
    // Only the review verbs surface their reason inline; every other row keeps
    // the menu clean and explains itself on hover.
    let show_inline_hint = disabled
        && !title.is_empty()
        && matches!(
            kind,
            DropdownActionKind::CreatePr | DropdownActionKind::PushBeforePr
        );
    let row_id = SharedString::from(format!("sc-menu-{kind:?}"));
    let item = PopupMenuItem::element(move |_window, _cx| {
        let tip = title.clone();
        let row = if !disabled {
            div().font_weight(label_weight).child(label.clone())
        } else if show_inline_hint {
            div()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(div().text_color(theme.fg_muted).child(label.clone()))
                .child(
                    div()
                        .text_size(px(sc_style::GRAPH_META_TEXT))
                        .text_color(theme.fg_subtle)
                        .child(title.clone()),
                )
        } else {
            div().text_color(theme.fg_muted).child(label.clone())
        };
        // Every row explains its current state on hover — enabled verbs
        // describe what they'll do, disabled verbs say why they can't. The
        // tooltip API needs a stable element id; the row is otherwise inert
        // (no click handler / stop_propagation) so the menu's own row click
        // still dispatches.
        row.id(row_id.clone()).tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
    })
    .disabled(disabled);
    // Disabled rows still carry the click slot for the menu's dispatcher; the
    // gate is `PopupMenuItem.disabled` (the row skips its on_click) and the
    // dispatch table no-ops on PR kinds so a wired click never escapes.
    let view_for_click = view.clone();
    item.on_click(window.listener_for(&view_for_click, move |area, _, window, cx| {
        dispatch_dropdown(area, kind, window, cx);
        cx.notify();
    }))
}

/// Single dispatch table — kind → `CommitArea` method. Keeping the match
/// here means the resolver and the action surface only meet at this one
/// site; adding a new dropdown verb is "add a kind + add a stub method +
/// add an arm here".
///
/// `window` is plumbed through so commit-family verbs (Commit,
/// CommitPush, CommitSync) can hand it to `submit` / `commit_and_push`
/// / `commit_and_sync`, which in turn root their completion task in
/// `cx.spawn_in(window, …)` so the success arm can auto-clear the
/// textarea via `set_value`. Remote-only verbs ignore `window`.
fn dispatch_dropdown(
    area: &mut CommitArea,
    kind: DropdownActionKind,
    window: &mut Window,
    cx: &mut Context<CommitArea>,
) {
    match kind {
        // Compound commit-with-followup verbs need a synthesised
        // `PrimaryAction { kind: Commit, disabled: false }` to satisfy
        // `submit`'s gate. The resolver already gated the row so we
        // know the user can commit; if it's disabled the menu blocked
        // the click before we got here.
        DropdownActionKind::Commit => {
            let synthetic = PrimaryAction {
                kind: PrimaryActionKind::Commit,
                label: "Commit".to_string(),
                title: String::new(),
                disabled: false,
            };
            area.submit(&synthetic, window, cx);
        }
        DropdownActionKind::CommitPush => area.commit_and_push(window, cx),
        DropdownActionKind::CommitForcePush => area.commit_and_force_push(window, cx),
        DropdownActionKind::CommitSync => area.commit_and_sync(window, cx),
        DropdownActionKind::Push => area.push(cx),
        DropdownActionKind::ForcePush => area.force_push(cx),
        DropdownActionKind::Pull => area.pull(cx),
        DropdownActionKind::FastForward => area.fast_forward(cx),
        DropdownActionKind::Sync => area.sync(cx),
        DropdownActionKind::Rebase => area.rebase_from_base(cx),
        DropdownActionKind::Fetch => area.fetch(cx),
        DropdownActionKind::Publish => area.publish(cx),
        DropdownActionKind::CreatePr => area.create_pr(cx),
        DropdownActionKind::MergePr(method) => area.merge_pr(method, cx),
        // Compound push-then-PR not wired; the row ships disabled.
        DropdownActionKind::PushBeforePr => {}
    }
}

fn primary_icon_for(
    kind: PrimaryActionKind,
    forge_kind: Option<crate::shell::forge::ForgeKind>,
) -> Option<Icon> {
    match kind {
        PrimaryActionKind::Commit => Some(IconName::Check.into()),
        PrimaryActionKind::Stage => Some(IconName::Plus.into()),
        PrimaryActionKind::Push | PrimaryActionKind::Publish => Some(IconName::ArrowUp.into()),
        PrimaryActionKind::Pull => Some(IconName::ArrowDown.into()),
        PrimaryActionKind::Sync => Some(IconName::ChevronsUpDown.into()),
        // Brand glyph matches the detected forge — a GitLab repo must not
        // wear the GitHub mark. Unknown forge defaults to GitHub purely as
        // a render fallback: Create-PR is disabled until detection lands.
        PrimaryActionKind::CreatePR => Some(match forge_kind {
            Some(crate::shell::forge::ForgeKind::Gitlab) => {
                Icon::default().path("icons/gitlab.svg")
            }
            _ => IconName::Github.into(),
        }),
    }
}

/// Single-line status indicator beneath the primary action. Returns
/// `None` while idle so the empty row doesn't reserve its line-height
/// as dead space between the composer and the file list — the row only
/// appears when there's something to say.
fn render_status_row(
    theme: Theme,
    _typography: &Typography,
    status: &CommitStatus,
) -> Option<impl IntoElement> {
    let (color, msg) = match status {
        CommitStatus::Idle => return None,
        CommitStatus::Committing => (theme.fg_muted, "Committing…".to_string()),
        CommitStatus::Pushing => (theme.fg_muted, "Pushing…".to_string()),
        CommitStatus::Pulling => (theme.fg_muted, "Pulling…".to_string()),
        CommitStatus::Syncing => (theme.fg_muted, "Syncing…".to_string()),
        CommitStatus::Fetching => (theme.fg_muted, "Fetching…".to_string()),
        CommitStatus::CherryPicking => (theme.fg_muted, "Cherry-picking…".to_string()),
        CommitStatus::Reverting => (theme.fg_muted, "Reverting…".to_string()),
        CommitStatus::Rebasing => (theme.fg_muted, "Rebasing…".to_string()),
        CommitStatus::Aborting => (theme.fg_muted, "Aborting…".to_string()),
        CommitStatus::Continuing => (theme.fg_muted, "Continuing…".to_string()),
        CommitStatus::CreatingPr => (theme.fg_muted, "Creating pull request…".to_string()),
        CommitStatus::MergingPr => (theme.fg_muted, "Merging pull request…".to_string()),
        CommitStatus::Failed(label, error) => (
            theme.status_error,
            // Title-case the verb so "Push failed: …" reads naturally;
            // labels are always short ASCII so the manual capitalize is fine.
            format!("{} failed: {error}", title_case_first(label)),
        ),
    };
    Some(
        div()
            .text_size(px(sc_style::GRAPH_META_TEXT))
            .text_color(color)
            .child(msg),
    )
}

fn title_case_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// `RemoteVerb` moved to `commit_ops.rs` — the helper module owns the
// op-execution machinery and the verb enum it dispatches on.
//
// `load_initial_commit_draft` lives in `settings_persistence.rs` next
// to its base-ref counterpart so the per-worktree load helpers stay in
// one place and one integration test file can exercise both.
