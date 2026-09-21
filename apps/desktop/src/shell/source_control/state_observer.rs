//! The status-poller observer: the loop that turns each `PollState` tick into
//! panel state.
//!
//! Split out of `mod.rs`, which sits right on the file-size lint's 1500-line
//! warn boundary and has been trimmed for it once already (Phase 8 moved
//! `set_poller` and `refresh_after_branch_change` into `picker_wiring.rs`).
//! This is the natural next seam: one method, one concern — everything that
//! happens *because a poll arrived*, and nothing that happens because the user
//! clicked something.
//!
//! What a tick drives, in order: the commit graph when HEAD has moved (see
//! [`SourceControlPanel::refresh_graph_if_head_moved`]), the cached
//! `git_state`, the commit area's staged snapshot, the resolved rebase base,
//! the in-progress-op banner, and — throttled — the forge PR/CI status.
//!
//! [`SourceControlPanel::refresh_graph_if_head_moved`]:
//!     crate::shell::source_control::SourceControlPanel::refresh_graph_if_head_moved

use super::*;

impl SourceControlPanel {
    pub(super) fn start_state_observer(
        mut rx: watch::Receiver<PollState>,
        repo: Repository,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let state = rx.borrow_and_update().clone();
                // Decide whether to refresh the upstream-rewrite check
                // BEFORE the panel-update borrow. The check only matters
                // when local and upstream have actually diverged — pure
                // ahead-only or behind-only states are never lease
                // candidates, so we skip the (cached-but-still-locking)
                // backend call.
                let should_check_lease = matches!(
                    &state,
                    PollState::Ready(s)
                        if s.upstream.is_some() && s.ahead > 0 && s.behind > 0
                );
                // The Create-PR rung only matters when the branch is published
                // and fully in sync (the terminal "up to date" state). Gate the
                // network `gh` round-trips on that — throttled to ~30s below.
                let should_check_pr = matches!(
                    &state,
                    PollState::Ready(s)
                        if s.upstream.is_some() && s.ahead == 0 && s.behind == 0
                );
                // The branch the (potential) PR/CI refresh would belong to —
                // used to invalidate the throttle when the user switches to a
                // different in-sync branch (its PR/CI differ).
                let pr_branch = match &state {
                    PollState::Ready(s) => s.branch.clone(),
                    _ => None,
                };
                // Refresh the cached in-progress git op before the
                // panel update so render sees the new value on the
                // same tick. Stat-only — microsecond cost on APFS,
                // tolerable on a poll tick (vs per-render, which
                // would burn it on every keystroke).
                let op = repo.current_operation();
                if this
                    .update(cx, |panel, cx| {
                        if let PollState::Ready(ref s) = state {
                            // Before the snapshot is replaced, while the
                            // previous HEAD is still on hand.
                            panel.refresh_graph_if_head_moved(s.head_oid.as_deref(), cx);
                            panel.git_state = Some(s.clone());
                            // Push the staged-filtered file list into
                            // the commit area so the sparkles button
                            // can gate on staged-count and feed the
                            // heuristic without re-shelling out to
                            // git on click. Equality-guarded inside
                            // the setter so identical snapshots
                            // don't fire spurious notifies.
                            let staged: Vec<oximux_core::FileStatus> = s
                                .files
                                .iter()
                                .filter(|f| f.is_staged())
                                .cloned()
                                .collect();
                            // Mirror the resolved rebase base in too, so the
                            // Rebase dropdown row dispatches onto the same ref
                            // its label shows. Computed after `git_state` is
                            // set above so `resolve_rebase_base` sees fresh data.
                            let rebase_base = panel.resolve_rebase_base();
                            let commit_area = panel.commit_area.clone();
                            commit_area.update(cx, |area, cx| {
                                area.set_staged_snapshot(staged, cx);
                                area.set_rebase_base(rebase_base);
                            });
                            // Feed the "Committed on Branch" section.
                            let branch_commits = panel.branch_commits.clone();
                            let bc_files = s.branch_committed.clone();
                            let bc_range = s.branch_range.clone();
                            branch_commits.update(cx, |p, cx| {
                                p.set_state(bc_files, bc_range, cx)
                            });
                        }
                        panel.poll_state = state;
                        panel.current_op = op;
                        if !should_check_lease {
                            // Reset stale lease state immediately when we
                            // leave the diverged window — otherwise the
                            // dropdown would keep showing Force Push on a
                            // freshly-pulled branch.
                            panel.force_push_with_lease = false;
                        }
                        if !should_check_pr || panel.pr_status_checked_branch != pr_branch {
                            // Left the in-sync window (new commits / a pull), or
                            // switched to a different in-sync branch: force a
                            // fresh PR + CI check on the next tick so stale data
                            // from the previous branch/state isn't shown.
                            panel.pr_status_checked_at = None;
                            panel.pr_status_checked_branch = pr_branch.clone();
                        }
                        // A just-created PR sets this flag; invalidate the
                        // throttle so the refresh below fires this tick and the
                        // button stops offering Create PR immediately.
                        let pr_dirty = panel
                            .commit_area
                            .read(cx)
                            .pr_status_dirty;
                        if pr_dirty {
                            panel.pr_status_checked_at = None;
                            panel
                                .commit_area
                                .update(cx, |area, _| area.pr_status_dirty = false);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                if should_check_lease {
                    refresh_force_push_with_lease(&repo, &this, cx).await;
                }
                if should_check_pr {
                    let due = this
                        .update(cx, |panel, _cx| {
                            panel.pr_status_checked_at.is_none_or(|t| {
                                t.elapsed() >= std::time::Duration::from_secs(30)
                            })
                        })
                        .unwrap_or(false);
                    if due {
                        refresh_pr_status(&repo, &this, cx).await;
                    }
                }
            }
        })
    }

}
