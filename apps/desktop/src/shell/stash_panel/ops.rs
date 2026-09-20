//! Stash mutations fired from the panel: apply, pop, drop, push.
//!
//! # Every op resolves its target by sha first
//!
//! A row is painted with a `stash@{N}` address, but `N` is only that entry's
//! position in a stack shared by every worktree of the repo — and OxiMux is a
//! second writer alongside the user's terminal. By the time a click lands, the
//! index the row was rendered with may address a different stash entirely.
//!
//! So nothing here fires on the rendered address. Each op calls
//! [`Repository::resolve_stash_index`] to turn the immutable sha back into the
//! *current* address, and acts on that. This self-heals rather than refusing:
//! an out-of-band stash push is routine here, not exceptional, and a
//! verify-then-abort guard would toast an error at the user every time one
//! happened. `None` — the stash is genuinely gone — is the only abort.
//!
//! # Drop closes the gap resolve-then-fire leaves open
//!
//! Resolving a sha and then handing git an *index* still races: something can
//! land in the gap, and git will cheerfully drop whatever now sits there. Drop
//! therefore logs its recovery line **before** firing, then compares the sha
//! git says it removed against the one that was intended, and restores the
//! stash on a mismatch. Without that check the failure is silent and
//! unrecoverable — the wrong stash is gone and the toast reports success.
//!
//! # Ops are detached, but serialised
//!
//! Detaching means a second op cannot void the first one's completion
//! handler. It does not stop them overlapping, and two ops that both resolve
//! before either fires reintroduce exactly the drift this module exists to
//! prevent. `StashPanel::op_lock` is held across resolve-and-fire so the
//! window is closed by construction; the sha assertion stays as the backstop
//! for anything outside this process.
//!
//! # Why detached, never slotted
//!
//! Each op detaches its task instead of storing it in a single field. A shared
//! slot means starting a second op drops the first op's completion handler
//! while its git subprocess is still running: the stash disappears from git,
//! stays painted in the panel, and produces no toast, no error and no log line
//! naming its sha. Same reasoning — and the same fix — as
//! `git_panel/selection.rs:285`.

use crate::shell::chrome::toast::ToastKind;
use crate::shell::stash_panel::{StashListState, StashPanel};
use gpui::Context;
use oximux_core::StashRef;
use oximux_git::Repository;
use tokio::sync::oneshot;

/// What a finished op wants said to the user: a toast kind plus its text, or
/// nothing at all when the refreshed list already tells the whole story.
type OpToast = Option<(ToastKind, String)>;

/// Shown when the sha a row was painted with is no longer on the stack.
const STASH_GONE: &str = "That stash no longer exists. The list has been refreshed.";

/// First 7 chars of a sha, for copy that has to fit in a toast.
fn short(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

/// Message to show for a stash that carries none.
fn label_of(message: &str) -> &str {
    if message.trim().is_empty() {
        "(no message)"
    } else {
        message.trim()
    }
}

/// Current address of `sha`, or `None` when the stash is gone.
async fn resolve(repo: &Repository, sha: &str) -> Result<Option<StashRef>, String> {
    repo.resolve_stash_index(sha)
        .await
        .map_err(|e| e.to_string())
}

impl StashPanel {
    /// Apply a stash without removing it from the stack.
    pub fn apply(&mut self, sha: String, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move {
                let Some(stash_ref) = resolve(&repo, &sha).await? else {
                    return Ok(Some((ToastKind::Warning, STASH_GONE.to_string())));
                };
                repo.stash_apply(&stash_ref)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(None)
            },
            "Stash apply",
            cx,
        );
    }

    /// Apply a stash and remove it from the stack.
    pub fn pop(&mut self, sha: String, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move {
                let Some(stash_ref) = resolve(&repo, &sha).await? else {
                    return Ok(Some((ToastKind::Warning, STASH_GONE.to_string())));
                };
                repo.stash_pop(&stash_ref).await.map_err(|e| e.to_string())?;
                Ok(None)
            },
            "Stash pop",
            cx,
        );
    }

    /// Remove a stash without applying it. Call only from the host's confirm
    /// dialog — this is the destructive path and it asks nothing further.
    ///
    /// Safe to call twice on the same sha, which matters because the reason a
    /// repeat happens is not a double-click: the second call resolves the sha
    /// to `None` and degrades to a toast rather than dropping whatever has
    /// since moved into that address. (`ConfirmDialog` itself `take`s its
    /// callback, so one mounted dialog fires at most once.)
    pub fn drop_confirmed(&mut self, sha: String, message: String, cx: &mut Context<Self>) {
        // Snapshot what the panel currently believes is on the stack. If git
        // drops a sha other than the intended one, this is what lets the
        // rollback restore it under its OWN message instead of a synthesized
        // placeholder — `git stash store -m` writes the message literally and
        // there is no second chance to recover it once the entry is gone.
        let known: Vec<(String, String)> = match &self.state {
            StashListState::Ready(entries) => entries
                .iter()
                .map(|e| (e.sha.clone(), e.message.clone()))
                .collect(),
            _ => Vec::new(),
        };
        let toast_label = label_of(&message).to_string();
        self.spawn_op(
            move |repo| async move {
                let Some(stash_ref) = resolve(&repo, &sha).await? else {
                    return Ok(Some((ToastKind::Warning, STASH_GONE.to_string())));
                };
                // Before anything destructive happens, put the recovery
                // command somewhere durable. If the process dies between here
                // and the toast, this log line is the only record of how to
                // get the stash back.
                // Structured fields, not an interpolated command line: a
                // stash message legally contains quotes, `$` and backslashes,
                // so any attempt to render `-m <msg>` here produces a line
                // that looks copy-pasteable and is not. The sha is the part
                // that matters, and `git stash store <sha>` accepts it alone.
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    sha = %sha,
                    message = %toast_label,
                    "dropping stash; recover with: git stash store {}",
                    sha,
                );
                let dropped = repo
                    .stash_drop(&stash_ref)
                    .await
                    .map_err(|e| e.to_string())?;
                if dropped != sha {
                    return Err(rollback(&repo, &known, &dropped, &sha).await);
                }
                Ok(Some((
                    ToastKind::Success,
                    format!("Dropped “{toast_label}” — recover with: git stash store {sha}"),
                )))
            },
            "Stash drop",
            cx,
        );
    }

    /// `git stash push` from the host's push dialog.
    ///
    /// The returned `StashRef` is discarded: the new entry lands at
    /// `stash@{0}` and surfaces through the refresh this op ends with.
    pub fn push(&mut self, msg: Option<String>, include_untracked: bool, cx: &mut Context<Self>) {
        self.spawn_op(
            move |repo| async move {
                repo.stash_push(msg.as_deref(), include_untracked, &[])
                    .await
                    .map(|_| None)
                    .map_err(|e| e.to_string())
            },
            "Stash push",
            cx,
        );
    }

    /// Run `op` on tokio, toast whatever it asks for, then re-read the list.
    ///
    /// The refresh is always forced. The 15 s read-TTL on `stash_list` exists
    /// to absorb an *external* writer's churn while the panel re-renders; our
    /// own mutation is not something to sit on for 15 s.
    fn spawn_op<F, Fut>(&mut self, op: F, label: &'static str, cx: &mut Context<Self>)
    where
        F: FnOnce(Repository) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<OpToast, String>> + Send + 'static,
    {
        let repo = self.repo.clone();
        let lock = self.op_lock.clone();
        let (tx, rx) = oneshot::channel::<Result<OpToast, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    // Held across the whole op, so no other op can resolve an
                    // index this one is about to invalidate. Queued, never
                    // cancelled — both clicks still complete.
                    let _guard = lock.lock().await;
                    let _ = tx.send(op(repo).await);
                });
            }
            Err(_) => {
                tracing::warn!(target: "oximux_app::stash_panel", op = label, "no tokio runtime; op skipped");
                return;
            }
        }
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                match &result {
                    Ok(Err(err)) => crate::shell::toast::toast_op_error(cx, label, err),
                    Ok(Ok(Some((kind, text)))) => {
                        crate::shell::toast::toast(cx, *kind, text.clone())
                    }
                    _ => {}
                }
                // Refresh even when the op failed — the user wants to see the
                // state git is actually in, not the one they clicked on.
                panel.force_refresh(cx);
            });
        })
        // Detached, not slotted: a second op must not void the first one's
        // completion handler mid-subprocess. See the module doc.
        .detach();
    }
}

/// Put back a stash git dropped that we did not mean to drop, and describe
/// what happened in terms the user can act on.
///
/// Always returns an error string: the rollback succeeding does not make the
/// operation a success — the stack has been reordered under the user and the
/// stash they asked to remove is still there.
async fn rollback(
    repo: &Repository,
    known: &[(String, String)],
    dropped: &str,
    intended: &str,
) -> String {
    let label = known
        .iter()
        .find(|(sha, _)| sha == dropped)
        .map(|(_, msg)| label_of(msg).to_string())
        .unwrap_or_else(|| format!("recovered stash {}", short(dropped)));
    match repo.stash_store(dropped, &label).await {
        Ok(()) => format!(
            "git removed {} instead of {} — it has been restored to the TOP of the stack, \
             so the order has changed. Nothing was lost; the stash you picked is still there.",
            short(dropped),
            short(intended),
        ),
        Err(e) => format!(
            "git removed {} instead of {} and restoring it FAILED: {e}. \
             Recover it by hand: git stash store {}",
            short(dropped),
            short(intended),
            dropped,
        ),
    }
}
