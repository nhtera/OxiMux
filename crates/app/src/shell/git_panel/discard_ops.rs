//! Discard plumbing for `GitPanel` — single-path and per-area variants.
//!
//! Phase 01 shipped the single-path flow (`discard_path` →
//! `confirmed_discard_path`) and the `DiscardRequest` modal contract.
//! Slice C extends it with section-area variants (`discard_area` →
//! `confirmed_discard_area`) and reshapes `DiscardRequest` to carry
//! either scope.
//!
//! Living in its own submodule keeps `mod.rs` under the 800-LOC fail
//! cap as Phase 03 piles on the area branches + the discard-all
//! sequence (`unstage_paths`-then-`discard_paths` for Staged) and the
//! `delete_untracked_paths` dispatch for pure-untracked rows.
//!
//! ## Confirm-modal contract
//!
//! 1. User clicks Discard (row hover, chord, or section header).
//! 2. `GitPanel::discard_path` / `discard_area` builds a
//!    [`DiscardRequest`] and stores it in `pending_discard`, then
//!    emits [`DiscardRequested`].
//! 3. Shell host (`workspace_root::mount_discard_dialog`) observes the
//!    event, reads `pending_discard()`, mounts a `ConfirmDialog`.
//! 4. On confirm: host dispatches on `request.scope` → calls
//!    [`GitPanel::confirmed_discard_path`] or
//!    [`GitPanel::confirmed_discard_area`].
//! 5. On cancel / Escape: host calls
//!    [`GitPanel::clear_pending_discard`].

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::discard_confirm::{
    self, DiscardAllArea, DiscardCopy, DiscardKind,
};
use gpui::{Context, EventEmitter, SharedString};
use oximux_git::Repository;
use std::path::{Path, PathBuf};
use tokio::sync::oneshot;

/// What the discard targets — a single row or a whole section.
///
/// Drives the modal's `on_confirm` dispatch in `workspace_root.rs`:
/// `Single` → [`GitPanel::confirmed_discard_path`]; `Area` →
/// [`GitPanel::confirmed_discard_area`] (which runs the
/// unstage-first-for-staged sequence and uses `git clean` for the
/// Untracked area).
#[derive(Debug, Clone, Copy)]
pub enum DiscardScope {
    /// Hover-row discard (Phase 01) — exactly one path. `kind`
    /// determines the copy flavour (Delete / Restore / Discard).
    Single { kind: DiscardKind },
    /// Section-header "Discard all" / "Delete all" (Slice C). The
    /// area determines copy + the sequence (`git restore` vs
    /// `git clean`, with an optional `git restore --staged` first
    /// pass for Staged).
    Area { area: DiscardAllArea },
}

/// Information the shell host needs to render a discard confirm dialog.
///
/// Built by `discard_path` (single) or `discard_area` (per-section).
/// The host passes `copy` + `expected` straight into a `ConfirmPrompt`,
/// and dispatches `on_confirm` on `scope`.
///
/// `paths` is always non-empty: one entry for `Single`, the full
/// section list for `Area`.
#[derive(Debug, Clone)]
pub struct DiscardRequest {
    pub paths: Vec<PathBuf>,
    pub scope: DiscardScope,
    pub copy: DiscardCopy,
    pub expected: SharedString,
}

/// Event emitted whenever `discard_path` / `discard_area` accepts a
/// new request. The shell host subscribes to GitPanel and pulls the
/// live request out of `pending_discard()` to build the confirm
/// dialog. We could ship the `DiscardRequest` on the event itself,
/// but routing through the field keeps the panel a single source of
/// truth — re-subscribers see the same state.
#[derive(Debug, Clone, Copy)]
pub struct DiscardRequested;

impl EventEmitter<DiscardRequested> for GitPanel {}

impl GitPanel {
    /// Open a confirm dialog for discarding `path`. Pulls the matching
    /// `FileStatus` out of the current `git_state` so the modal copy
    /// (Delete / Restore / Discard) matches what the user is looking
    /// at. If `path` is no longer in `git_state` (stale UI state), the
    /// fallback copy reads as a generic "Discard changes to ...".
    ///
    /// This method does NOT mutate the working tree — it only sets
    /// `pending_discard` and notifies. The shell host observes the
    /// field, mounts a `ConfirmDialog`, and calls
    /// [`confirmed_discard_path`] when the user confirms.
    ///
    /// Early-returns when another discard for the same path is already
    /// in flight or when any request is currently pending. This
    /// prevents the revert keybind from queueing a second confirm
    /// dialog over an unresolved one (the hover button blocks clicks
    /// via `.disabled(true)` already; the keybind has no equivalent
    /// gate at the action handler).
    ///
    /// [`confirmed_discard_path`]: Self::confirmed_discard_path
    pub fn discard_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.pending_discard.is_some() || self.in_flight_discards.contains(&path) {
            return;
        }
        let request = self.build_discard_request(path);
        self.pending_discard = Some(request);
        cx.emit(DiscardRequested);
        cx.notify();
    }

    fn build_discard_request(&self, path: PathBuf) -> DiscardRequest {
        let file_status = self
            .git_state
            .as_ref()
            .and_then(|s| s.files.iter().find(|f| f.path == path).cloned());

        match file_status {
            Some(f) => {
                let (kind, copy) = discard_confirm::copy_for(&f);
                let expected = discard_confirm::expected_for(&f);
                DiscardRequest {
                    paths: vec![path],
                    scope: DiscardScope::Single { kind },
                    copy,
                    expected,
                }
            }
            None => {
                // Stale state — the poller hasn't reported this path
                // (yet). Surface the generic Discard copy with the
                // path basename so the user can decide.
                let display = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                DiscardRequest {
                    paths: vec![path],
                    scope: DiscardScope::Single {
                        kind: DiscardKind::Discard,
                    },
                    copy: DiscardCopy {
                        title: format!("Discard changes to \"{display}\"?").into(),
                        body: "This will revert all changes to this file. This cannot be undone."
                            .into(),
                        confirm_label: "Discard".into(),
                    },
                    expected: display.into(),
                }
            }
        }
    }

    /// Open the type-to-confirm dialog for "Discard all" / "Delete all"
    /// on a section. `paths` is the full list of paths in `area` at
    /// the moment the user clicked. No-op when `paths` is empty (the
    /// section button should already be hidden in that case).
    ///
    /// Like [`discard_path`](Self::discard_path), this only sets
    /// `pending_discard` — the actual mutation runs in
    /// [`confirmed_discard_area`](Self::confirmed_discard_area) after
    /// the user types the expected string and clicks Discard / Delete.
    pub fn discard_area(
        &mut self,
        area: DiscardAllArea,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_discard.is_some() || paths.is_empty() {
            return;
        }
        let copy = discard_confirm::copy_for_area(area, paths.len());
        let expected = discard_confirm::expected_for_area(area, paths.len());
        self.pending_discard = Some(DiscardRequest {
            paths,
            scope: DiscardScope::Area { area },
            copy,
            expected,
        });
        cx.emit(DiscardRequested);
        cx.notify();
    }

    /// Current pending discard request, if any. The shell host observes
    /// this via `cx.observe(&panel)` to mount / dismiss the modal.
    pub fn pending_discard(&self) -> Option<&DiscardRequest> {
        self.pending_discard.as_ref()
    }

    /// Drop the pending request without mutating the working tree.
    /// Called by the shell on Escape / cancel / click-outside.
    pub fn clear_pending_discard(&mut self, cx: &mut Context<Self>) {
        if self.pending_discard.take().is_some() {
            cx.notify();
        }
    }

    /// Paths whose `confirmed_discard_path` / `confirmed_discard_area`
    /// op is in flight on the tokio runtime. The row renderer reads
    /// this set to swap the revert icon for a spinner.
    pub fn in_flight_discards(&self) -> &std::collections::HashSet<PathBuf> {
        &self.in_flight_discards
    }

    /// Actually run the discard for a single `path`. Tracks the path
    /// in `in_flight_discards` for spinner rendering and clears it
    /// after the op completes (success or failure). The host calls
    /// this from the ConfirmDialog `on_confirm` callback.
    ///
    /// Dispatches on whether the path is pure-untracked: untracked
    /// paths go through `delete_untracked_paths` (`git clean -f --`)
    /// because `git restore` errors on paths git doesn't know about.
    /// Slice C fix — Phase 01 always called `discard_paths` so a
    /// single-row Delete on an untracked file silently failed.
    ///
    /// Pauses editor autosave for the duration of the op so a buffer
    /// open on the same path can't race the worktree write and
    /// immediately re-save the user's old content over the checked-out
    /// version (today both calls are no-op stubs; the semantics light
    /// up automatically when the editor side ships an autosave pump).
    pub fn confirmed_discard_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.pending_discard = None;
        self.in_flight_discards.insert(path.clone());
        cx.notify();

        oximux_editor::pause_autosave(&path);

        let is_untracked = self.is_pure_untracked(&path);
        let repo = self.repo.clone();
        let op_path = path.clone();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let refs = &[op_path.as_path()];
                    let r = if is_untracked {
                        repo.delete_untracked_paths(refs).await
                    } else {
                        repo.discard_paths(refs).await
                    };
                    let _ = tx.send(r.map_err(|e| e.to_string()));
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    "no tokio runtime; confirmed_discard_path skipped (test wiring)"
                );
                self.in_flight_discards.remove(&path);
                oximux_editor::resume_autosave(&path);
                cx.notify();
                return;
            }
        }

        // Detach the result-handling task so concurrent discards
        // don't cancel each other.
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                panel.in_flight_discards.remove(&path);
                oximux_editor::resume_autosave(&path);
                if let Ok(Err(err)) = result {
                    tracing::warn!(
                        target: "oximux_app::git_panel",
                        error = %err,
                        "single-path discard failed"
                    );
                    crate::shell::toast::toast_op_error(
                        cx,
                        &format!("Discard {}", path.display()),
                        &err,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run the area-discard sequence. Tracks every path in
    /// `in_flight_discards` (so per-row spinners fire), pauses
    /// editor autosave, and on completion logs + clears the in-flight
    /// markers.
    ///
    /// Sequence by area:
    /// - `Staged`: `git restore --staged --` first, then `git restore --`
    ///   so the worktree picks up the HEAD content (otherwise the
    ///   second call has nothing to revert TO, because the index
    ///   still holds the staged version).
    /// - `Unstaged`: `git restore --` only.
    /// - `Untracked`: `git clean -f --` only (`git restore` can't
    ///   touch paths git doesn't know about).
    ///
    /// Fail-fast on the staged-area unstage step; the worktree
    /// restore won't run if unstage errored.
    pub fn confirmed_discard_area(
        &mut self,
        area: DiscardAllArea,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.pending_discard = None;
        for p in &paths {
            self.in_flight_discards.insert(p.clone());
            oximux_editor::pause_autosave(p);
        }
        cx.notify();

        let repo = self.repo.clone();
        let op_paths = paths.clone();
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let refs: Vec<&Path> = op_paths.iter().map(|p| p.as_path()).collect();
                    let r = run_area_discard_sequence(&repo, area, &refs).await;
                    let _ = tx.send(r.map_err(|e| e.to_string()));
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    area = ?area,
                    "no tokio runtime; confirmed_discard_area skipped (test wiring)"
                );
                for p in &paths {
                    self.in_flight_discards.remove(p);
                    oximux_editor::resume_autosave(p);
                }
                cx.notify();
                return;
            }
        }

        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                for p in &paths {
                    panel.in_flight_discards.remove(p);
                    oximux_editor::resume_autosave(p);
                }
                if let Ok(Err(err)) = result {
                    tracing::warn!(
                        target: "oximux_app::git_panel",
                        area = ?area,
                        error = %err,
                        "confirmed_discard_area failed"
                    );
                    crate::shell::toast::toast_op_error(cx, "Discard changes", &err);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// True if `path` is in the current poll snapshot AND its index +
    /// worktree both report `Untracked`. Used to dispatch single-row
    /// confirmed_discard_path between `git restore` and `git clean`.
    /// Returns `false` for stale / missing paths — falls back to the
    /// `git restore` path, which is harmless for "the path no longer
    /// exists" (git errors, we log, modal already closed).
    fn is_pure_untracked(&self, path: &Path) -> bool {
        use oximux_core::{IndexStatus, WorktreeStatus};
        self.git_state
            .as_ref()
            .and_then(|s| s.files.iter().find(|f| f.path == path))
            .map(|f| {
                matches!(f.index, IndexStatus::Untracked)
                    && matches!(f.worktree, WorktreeStatus::Untracked)
            })
            .unwrap_or(false)
    }
}

/// Per-area discard sequence: dispatches the right backend(s) for
/// the area. Lives at module scope so it's testable in isolation and
/// the spawned future doesn't need access to `self`.
///
/// - `Staged` is the only multi-step path: `unstage_paths` first
///   (fail-fast on error), then `discard_paths`.
/// - `Unstaged` runs `discard_paths` directly.
/// - `Untracked` runs `delete_untracked_paths` (`git clean -f --`).
async fn run_area_discard_sequence(
    repo: &Repository,
    area: DiscardAllArea,
    paths: &[&Path],
) -> oximux_git::Result<()> {
    match area {
        DiscardAllArea::Staged => {
            repo.unstage_paths(paths).await?;
            repo.discard_paths(paths).await
        }
        DiscardAllArea::Unstaged => repo.discard_paths(paths).await,
        DiscardAllArea::Untracked => repo.delete_untracked_paths(paths).await,
    }
}
