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
use crate::shell::forge::{ForgeProvider, GithubForge};

/// Run `gh pr create --fill` for the current branch. No-op if another op is
/// already in flight (the shared `in_flight` latch). On success the PR opens
/// in the browser and the status row clears; on failure the status row shows
/// `gh`'s message (commonly "a pull request already exists").
pub fn run_create_pr(area: &mut CommitArea, cx: &mut Context<CommitArea>) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = CommitStatus::CreatingPr;
    let workdir = area.repo.workdir().to_path_buf();
    let (tx, rx) = oneshot::channel::<Result<String, String>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let r = GithubForge
                    .create_pr(&workdir)
                    .await
                    .map_err(|e| e.to_string());
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
                    open_in_browser(&url);
                    tracing::info!(target: "oximux_app::source_control", url = %url, "pull request created");
                }
                Err(err) => {
                    area.status = CommitStatus::Failed("create PR".to_string(), err);
                }
            }
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}

/// Open `url` in the default browser. macOS-only app, so `open <url>` is the
/// portable launcher. Best-effort: a spawn failure is logged, not surfaced —
/// the PR already exists; the URL is also in the tracing log.
fn open_in_browser(url: &str) {
    if url.is_empty() {
        return;
    }
    if let Err(err) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!(target: "oximux_app::source_control", ?err, "failed to open PR url in browser");
    }
}
