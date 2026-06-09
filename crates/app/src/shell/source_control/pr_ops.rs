//! Create-Pull-Request execution path for the Source Control panel.
//!
//! Mirrors the two-runtime bridge in [`super::commit_ops`]: the `gh pr create`
//! shellout runs on the live tokio runtime via `Handle::try_current().spawn`,
//! hands the result back through a `oneshot`, and a `cx.spawn` task on the gpui
//! executor applies it to the [`CommitArea`] status surface. On success the PR
//! opens in the default browser.
//!
//! Kept separate from `commit_ops` because the PR result carries a URL (not a
//! static verb label) and the success arm has a side effect (open browser)
//! that the remote-op completion path does not.

use std::sync::atomic::Ordering;

use gpui::Context;
use tokio::sync::oneshot;

use super::commit_area::{CommitArea, CommitStatus};
use crate::shell::forge::{CreatePrOptions, Forge, ForgeProvider, MergeMethod};

/// Create a PR for the current branch with the supplied options (empty title
/// falls back to commit-derived fill). No-op if another op is already in flight
/// (the shared `in_flight` latch). On success the PR opens in the browser and
/// the status row clears; on failure the status row shows the forge's message
/// (commonly "a pull request already exists").
pub fn run_create_pr(area: &mut CommitArea, opts: CreatePrOptions, cx: &mut Context<CommitArea>) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = CommitStatus::CreatingPr;
    let workdir = area.repo.workdir().to_path_buf();
    let (tx, rx) = oneshot::channel::<Result<String, String>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let r = match Forge::detect(&workdir).await {
                    Some(forge) => forge
                        .create_pr(&workdir, opts)
                        .await
                        .map_err(|e| e.to_string()),
                    None => Err("origin is not a supported forge (GitHub or GitLab)".to_string()),
                };
                let _ = tx.send(r);
            });
        }
        Err(_) => {
            area.status =
                CommitStatus::Failed("create PR".to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    let task = cx.spawn(async move |this, cx| {
        let Ok(result) = rx.await else {
            return;
        };
        let _ = this.update(cx, |area, cx| {
            area.in_flight.store(false, Ordering::SeqCst);
            match result {
                Ok(url) => {
                    area.status = CommitStatus::Idle;
                    // Force the panel's next observer tick to re-check PR
                    // status so the button stops offering Create PR right away
                    // instead of after the 30s throttle.
                    area.pr_status_dirty = true;
                    crate::shell::open_url::open_url(&url);
                    crate::shell::toast::toast(
                        cx,
                        crate::shell::toast::ToastKind::Success,
                        "Pull request opened",
                    );
                    tracing::info!(target: "oximux_app::source_control", url = %url, "pull request created");
                }
                Err(err) => {
                    crate::shell::toast::toast(
                        cx,
                        crate::shell::toast::ToastKind::Error,
                        "PR creation failed",
                    );
                    area.status = CommitStatus::Failed("create PR".to_string(), err);
                }
            }
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}

/// Merge the current branch's open PR with `method`. No-op if another op is in
/// flight (shared `in_flight` latch). On success the status row clears and the
/// panel's next poll re-checks PR/CI (forced via `pr_status_dirty`); on failure
/// the status row shows the forge's message (e.g. "not mergeable").
pub fn run_merge_pr(area: &mut CommitArea, method: MergeMethod, cx: &mut Context<CommitArea>) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = CommitStatus::MergingPr;
    let workdir = area.repo.workdir().to_path_buf();
    let (tx, rx) = oneshot::channel::<Result<(), String>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let r = match Forge::detect(&workdir).await {
                    Some(forge) => forge
                        .merge_pr(&workdir, method)
                        .await
                        .map_err(|e| e.to_string()),
                    None => Err("origin is not a supported forge (GitHub or GitLab)".to_string()),
                };
                let _ = tx.send(r);
            });
        }
        Err(_) => {
            area.status =
                CommitStatus::Failed("merge PR".to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    let task = cx.spawn(async move |this, cx| {
        let Ok(result) = rx.await else {
            return;
        };
        let _ = this.update(cx, |area, cx| {
            area.in_flight.store(false, Ordering::SeqCst);
            match result {
                Ok(()) => {
                    area.status = CommitStatus::Idle;
                    // Force the next observer tick to re-check PR status so the
                    // merged PR's surface (checks, merge rows) clears promptly.
                    area.pr_status_dirty = true;
                    tracing::info!(target: "oximux_app::source_control", "pull request merged");
                }
                Err(err) => {
                    area.status = CommitStatus::Failed("merge PR".to_string(), err);
                }
            }
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}

