//! Rewind: per-turn checkpoints + "restore conversation (± files)".
//!
//! Flow (strictly sequenced — each step gates the next):
//! 1. `cancel_and_wait` — the child must be CONFIRMED dead before the session
//!    file is read; SIGINT alone is fire-and-forget and the CLI may still be
//!    flushing its transcript.
//! 2. `fork_truncated` — write a NEW session file cut before the target user
//!    message; the original file is never touched.
//! 3. Optional `CheckpointEngine::restore` (files axis). A GC'd checkpoint
//!    degrades to conversation-only with a notice, never a hard failure.
//! 4. Back on the foreground: truncate the in-memory thread, swap to the new
//!    session id, respawn. On ANY failure before the swap, the original
//!    session (file, blob, id) is untouched and the tab stays resumable.
//!
//! The old transcript blob (`agent_chat:<old-sid>`) is deliberately NOT
//! deleted: it is the recovery path if anything after the fork goes sideways,
//! and it is tiny. (A later sweep can garbage-collect blobs whose session file
//! no longer exists.)

use gpui::prelude::FluentBuilder;
use gpui::{div, px, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window};
use oximux_agents::thread::session_file_fork::{self, ForkError};
use oximux_agents::thread::ThreadEntry;
use oximux_git::checkpoint::{CheckpointEngine, CheckpointError, CheckpointSha};

use super::AgentChatView;

/// State of the open rewind-confirm card (rendered above the composer).
pub struct RewindConfirm {
    /// Ordinal among USER entries (what `truncate_to_user` / the fork take).
    pub ordinal: usize,
    /// Index into `thread.entries` of the target user message.
    pub entry_index: usize,
    /// Snapshot sha when the files axis is offerable (`checkpoint.show`).
    pub sha: Option<String>,
    /// Files `restore` would overwrite, fetched async; `None` while loading.
    pub dirty_count: Option<usize>,
    /// The user's "also restore files" toggle.
    pub include_files: bool,
    /// How many messages will be removed (user-facing count).
    pub messages_removed: usize,
    /// When set, confirming leads to edit-and-resend (prefill) instead of a
    /// plain rewind. Wired in the edit-and-resend slice; constructed `false`
    /// here so the plain-rewind path is unaffected.
    #[allow(dead_code)]
    pub for_edit: bool,
}

/// Outcome of the background half of a rewind.
enum RewindOutcome {
    /// Fork succeeded; files axis (if requested) either applied or degraded.
    Done { new_sid: String, files_degraded: bool },
    /// Nothing was changed on disk (beyond a possible orphan fork file).
    Failed(String),
}

impl AgentChatView {
    /// Ordinal among user entries for `entries[entry_index]`, if it's a user
    /// entry.
    pub(super) fn user_ordinal_at(&self, entry_index: usize) -> Option<usize> {
        if !matches!(self.thread.entries.get(entry_index), Some(ThreadEntry::User { .. })) {
            return None;
        }
        Some(
            self.thread.entries[..entry_index]
                .iter()
                .filter(|e| matches!(e, ThreadEntry::User { .. }))
                .count(),
        )
    }

    /// Open the confirm card for the user message at `entry_index`.
    pub(super) fn open_rewind_confirm(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        if self.rewinding || self.thread.session_id.is_none() {
            return;
        }
        let Some(ordinal) = self.user_ordinal_at(entry_index) else { return };
        let sha = match self.thread.entries.get(entry_index) {
            Some(ThreadEntry::User { checkpoint: Some(cp), .. }) if cp.show => {
                Some(cp.sha.clone())
            }
            _ => None,
        };
        let files_offerable = sha.is_some() && self.checkpoint_engine.is_some();
        self.rewind_confirm = Some(RewindConfirm {
            ordinal,
            entry_index,
            sha: sha.clone(),
            dirty_count: None,
            include_files: false,
            messages_removed: self.thread.entries.len() - entry_index,
            for_edit: false,
        });
        // Blast-radius count for the dialog copy, fetched off-thread.
        if files_offerable
            && let (Some(engine), Some(sha)) = (self.checkpoint_engine.clone(), sha)
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let (tx, rx) = tokio::sync::oneshot::channel::<Option<usize>>();
            handle.spawn(async move {
                let n = engine.worktree_dirty_since(&CheckpointSha(sha)).await.ok();
                let _ = tx.send(n);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(n) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(rc) = &mut this.rewind_confirm {
                            rc.dirty_count = n;
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }

    pub(super) fn cancel_rewind_confirm(&mut self, cx: &mut Context<Self>) {
        if self.rewind_confirm.take().is_some() {
            cx.notify();
        }
    }

    /// Confirm the open card: run the rewind with the chosen axes.
    pub(super) fn confirm_rewind(&mut self, cx: &mut Context<Self>) {
        let Some(rc) = self.rewind_confirm.take() else { return };
        let sha_for_restore = rc.include_files.then_some(rc.sha.clone()).flatten();
        self.perform_rewind(rc.ordinal, rc.entry_index, sha_for_restore, cx);
    }

    /// The strictly-sequenced rewind flow (see module docs). Background half on
    /// the tokio runtime; the state swap happens on the foreground ONLY after
    /// the fork (and optional restore) succeeded.
    pub(super) fn perform_rewind(
        &mut self,
        ordinal: usize,
        entry_index: usize,
        sha_for_restore: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.rewinding {
            return;
        }
        let Some(old_sid) = self.thread.session_id.clone() else { return };
        let Some(expected_text) = (match self.thread.entries.get(entry_index) {
            Some(ThreadEntry::User { text, .. }) => Some(text.clone()),
            _ => None,
        }) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.thread.last_error = Some("Rewind unavailable: no async runtime".into());
            cx.notify();
            return;
        };

        self.rewinding = true;
        // Mark the kill as intentional BEFORE taking the connection: the old
        // drain task's forwarder observes the killed child's stdout EOF
        // independently of `cancel_and_wait`'s reap, and its `on_disconnect`
        // runs on the foreground racing `finish_rewind`. With `interrupted`
        // set, `on_disconnect` takes its resumable-idle branch (no
        // `disconnected`, no error banner) instead of stranding the tab.
        // `finish_rewind` clears it (respawn on success; explicit on failure).
        self.interrupted = true;
        self.sync_composer(cx);
        cx.notify();

        // Step 1 input: the connection is MOVED into the background task so
        // `cancel_and_wait` can block there. From here the view has no live
        // connection until `finish_rewind` respawns (or restores `interrupted`).
        let conn = self.connection.take();
        let engine = self.checkpoint_engine.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<RewindOutcome>();
        handle.spawn(async move {
            let outcome = run_rewind_background(conn, engine, old_sid, ordinal, expected_text,
                sha_for_restore).await;
            let _ = tx.send(outcome);
        });
        cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or_else(|_| {
                RewindOutcome::Failed("rewind task dropped".into())
            });
            let _ = this.update(cx, |this, cx| this.finish_rewind(ordinal, outcome, cx));
        })
        .detach();
    }

    /// Foreground completion: swap state on success, restore resumability on
    /// failure. The original session file/blob are intact in every branch.
    fn finish_rewind(&mut self, ordinal: usize, outcome: RewindOutcome, cx: &mut Context<Self>) {
        self.rewinding = false;
        match outcome {
            RewindOutcome::Done { new_sid, files_degraded } => {
                self.thread.truncate_to_user(ordinal);
                // Entry indices shift when the tail is dropped — clear any jump
                // highlight so it can't tint the wrong bubble after the rewind.
                self.flash_entry = None;
                self.flash_frames = 0;
                self.pre_turn_checkpoint = None;
                self.thread.session_id = Some(new_sid);
                // Respawn resumes the FORKED session. On failure it sets
                // `disconnected` + an error banner itself; the old session
                // blob/file are still on disk for recovery.
                self.respawn(cx);
                if files_degraded {
                    self.thread.last_error = Some(
                        "Checkpoint expired (git gc) — conversation rewound, files left as-is."
                            .into(),
                    );
                }
                // Edit-and-resend: send the edited message into the forked
                // session, but only if the respawn actually connected.
                if let Some((text, images)) = self.rewind_then_send.take()
                    && !self.disconnected
                {
                    self.send_text(text, images, cx);
                }
            }
            RewindOutcome::Failed(msg) => {
                // Nothing was swapped. The child was killed by cancel_and_wait,
                // so mark resumable-idle: the next send respawns the ORIGINAL
                // session via `--resume`. Drop any queued edit-and-resend text.
                self.rewind_then_send = None;
                self.interrupted = true;
                self.thread.turn_active = false;
                self.thread.last_error = Some(format!("Rewind failed: {msg}"));
            }
        }
        self.sync_composer(cx);
        cx.notify();
    }

    /// The confirm card, rendered above the composer while open.
    pub(super) fn render_rewind_confirm(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let rc = self.rewind_confirm.as_ref()?;
        let t = &self.theme;
        let files_offerable = rc.sha.is_some();
        let include_files = rc.include_files;
        let file_note: SharedString = match (include_files, rc.dirty_count) {
            (true, Some(n)) => format!(
                "{n} file{} in this repo will be reset to the snapshot — ALL uncommitted \
                 changes since then are discarded, including edits made outside this chat. \
                 Other repos and submodule contents are not covered.",
                if n == 1 { "" } else { "s" }
            )
            .into(),
            (true, None) => "Counting files this will overwrite…".into(),
            (false, _) => "Files on disk are left as they are.".into(),
        };
        let title: SharedString = format!(
            "Rewind: remove {} message{} after this point?",
            rc.messages_removed,
            if rc.messages_removed == 1 { "" } else { "s" }
        )
        .into();

        // Soft error wash for the leading rewind badge — signals a destructive,
        // history-removing action without shouting.
        let badge_soft = gpui::Hsla { a: 0.15, ..t.status_error };

        Some(
            // Center a width-capped card so the confirm reads as a contained
            // dialog above the composer, not a full-bleed strip.
            div().flex().w_full().justify_center().child(
            div()
                .flex()
                .flex_col()
                .w_full()
                .max_w(px(560.0))
                .gap(px(8.0))
                .pl(px(11.0))
                .pr(px(10.0))
                .py(px(10.0))
                .rounded(px(9.0))
                .bg(t.bg_panel_alt)
                .border_1()
                .border_color(t.border_inactive)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .flex_shrink_0()
                                .size(px(24.0))
                                .rounded(px(6.0))
                                .bg(badge_soft)
                                .text_sm()
                                .text_color(t.status_error)
                                .child("↺"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(t.fg_base)
                                .child(title),
                        ),
                )
                .when(files_offerable, |el| {
                    el.child(
                        div()
                            .id("rewind-files-toggle")
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(if include_files { t.fg_base } else { t.fg_muted })
                            .child(if include_files { "☑" } else { "☐" })
                            .child("Also restore files to the snapshot")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(rc) = &mut this.rewind_confirm {
                                    rc.include_files = !rc.include_files;
                                    cx.notify();
                                }
                            })),
                    )
                })
                .child(div().text_xs().text_color(t.fg_muted).child(file_note))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .justify_end()
                        .child(
                            div()
                                .id("rewind-cancel")
                                .px(px(10.0))
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .text_xs()
                                .text_color(t.fg_muted)
                                .border_1()
                                .border_color(t.border_inactive)
                                .hover(|s| s.bg(t.hover_overlay).text_color(t.fg_base))
                                .child("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_rewind_confirm(cx)
                                })),
                        )
                        .child(
                            div()
                                .id("rewind-go")
                                .px(px(10.0))
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .bg(t.status_error.opacity(0.15))
                                .text_color(t.status_error)
                                .hover(|s| s.bg(t.status_error.opacity(0.28)))
                                .child("Rewind")
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_rewind(cx))),
                        ),
                ),
            ),
        )
    }
}

/// Steps 1–3 of the flow, off the UI thread. Returns without mutating any view
/// state — the caller owns the swap.
async fn run_rewind_background(
    conn: Option<Box<dyn oximux_agents::thread::AgentConnection>>,
    engine: Option<std::sync::Arc<CheckpointEngine>>,
    old_sid: String,
    ordinal: usize,
    expected_text: String,
    sha_for_restore: Option<String>,
) -> RewindOutcome {
    // Step 1: confirmed-dead child. `cancel_and_wait` blocks — hop to a
    // blocking-ok thread so the tokio worker isn't stalled.
    if let Some(conn) = conn {
        let waited = tokio::task::spawn_blocking(move || {
            let r = conn.cancel_and_wait();
            conn.shutdown(); // idempotent reap, keeps the no-zombie invariant
            r
        })
        .await;
        match waited {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return RewindOutcome::Failed(format!("stopping agent: {e}")),
            Err(e) => return RewindOutcome::Failed(format!("stopping agent: {e}")),
        }
    }

    // Step 2: fork-truncate the session file.
    let Some(root) = session_file_fork::default_projects_root() else {
        return RewindOutcome::Failed("cannot locate ~/.claude/projects".into());
    };
    let fork = tokio::task::spawn_blocking(move || {
        session_file_fork::fork_truncated(&root, &old_sid, ordinal, &expected_text)
    })
    .await;
    let new_sid = match fork {
        Ok(Ok(sid)) => sid,
        Ok(Err(e @ ForkError::OrdinalMismatch { .. })) => {
            return RewindOutcome::Failed(format!("{e} (transcript out of sync)"));
        }
        Ok(Err(e)) => return RewindOutcome::Failed(e.to_string()),
        Err(e) => return RewindOutcome::Failed(format!("fork task: {e}")),
    };

    // Step 3: optional files restore; GC'd checkpoint degrades, not fails.
    let mut files_degraded = false;
    if let Some(sha) = sha_for_restore {
        match engine {
            Some(engine) => match engine.restore(&CheckpointSha(sha)).await {
                Ok(()) => {}
                Err(CheckpointError::ObjectMissing) => files_degraded = true,
                Err(e) => return RewindOutcome::Failed(format!("restoring files: {e}")),
            },
            None => files_degraded = true,
        }
    }

    RewindOutcome::Done { new_sid, files_degraded }
}
