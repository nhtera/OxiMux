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
        if self.rewinding || self.thread.session_id.is_none() || !self.backend_supports_rewind() {
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

    /// "Fork from here": branch the conversation at `entry_index` into a NEW
    /// chat tab, leaving THIS tab and its session fully intact. Reuses the same
    /// truncate-fork as rewind (keep everything before the target user message)
    /// but hands the result to the host to open as a separate tab instead of
    /// replacing this one — so the original thread stays put while the fork
    /// explores a different direction.
    ///
    /// Idle-only: the on-disk session file is read directly (no child cancel),
    /// so the CLI must have flushed the last turn. The original file is never
    /// modified; the truncated copy lands under a fresh session id.
    pub(super) fn request_fork(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        // Fork-to-new-tab reads the on-disk `~/.claude` session log directly, so
        // it's client-side (Claude) only. A server-side backend (Codex) has no
        // such log — the menu hides this entry there (see `render_fork_menu_item`).
        if self.rewinding
            || self.thread.turn_active
            || !self.backend_supports_rewind()
            || self.connection.as_ref().is_some_and(|c| c.rewind_is_server_side())
        {
            return;
        }
        let Some(old_sid) = self.thread.session_id.clone() else { return };
        let Some(ordinal) = self.user_ordinal_at(entry_index) else { return };
        let Some(expected_text) = (match self.thread.entries.get(entry_index) {
            Some(ThreadEntry::User { text, .. }) => Some(text.clone()),
            _ => None,
        }) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.thread.last_error = Some("Fork unavailable: no async runtime".into());
            cx.notify();
            return;
        };

        // The forked tab's in-memory transcript matches the file cut: everything
        // before the target user message.
        let entries = self.thread.entries[..entry_index].to_vec();
        let cwd = self.cwd.clone();
        let model = self.model.clone();
        let slash_commands = self.thread.slash_commands.clone();
        // The fork resumes the same underlying session, so it inherits what that
        // session advertised — no empty popover until its first turn re-inits.
        let session_meta = self.thread.session_meta.clone();
        let thinking_level = self.thinking_level;

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        handle.spawn(async move {
            let res = match session_file_fork::default_projects_root() {
                Some(root) => tokio::task::spawn_blocking(move || {
                    session_file_fork::fork_truncated(&root, &old_sid, ordinal, &expected_text)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(format!("fork task: {e}"))),
                None => Err("cannot locate ~/.claude/projects".into()),
            };
            let _ = tx.send(res);
        });
        cx.spawn(async move |this, cx| {
            let res = rx.await.unwrap_or_else(|_| Err("fork task dropped".into()));
            let _ = this.update(cx, |this, cx| match res {
                Ok(session_id) => {
                    cx.emit(super::AgentChatEvent::ForkReady {
                        cwd,
                        model,
                        session_id,
                        entries,
                        slash_commands,
                        session_meta,
                        thinking_level,
                    });
                }
                Err(msg) => {
                    this.thread.last_error = Some(format!("Fork failed: {msg}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Start a rewind on behalf of a remote client.
    ///
    /// Every check here exists because the caller is not the desktop user: the
    /// phone folded its own transcript from an event stream and can be behind,
    /// so its `ordinal` is a *claim* about a conversation, validated against
    /// this view's actual thread before anything destructive happens. The
    /// desktop's own path skips this — it derives the ordinal from the entry
    /// the user clicked, which cannot be stale.
    ///
    /// Returns once the rewind is accepted, not once it completes; the
    /// truncation reaches subscribers as a `Rewound` event either way, so
    /// there is nothing for the caller to learn by waiting.
    pub fn remote_rewind(
        &mut self,
        ordinal: usize,
        include_files: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), oximux_remote_host::RewindError> {
        use oximux_remote_host::RewindError;

        // A restored session the desktop hasn't rendered yet is dormant: no
        // connection, so `backend_supports_rewind` below would read as
        // Unsupported even for a backend that supports it. A phone rewind is
        // an explicit user action, so connect first (retry-capable, like the
        // catalog open). No-op on an already-live chat.
        self.ensure_connected(true, cx);
        // The files axis is refused rather than silently downgraded to a
        // conversation-only rewind: it overwrites the working tree, discarding
        // uncommitted work belonging to whoever is sitting at the desktop. A
        // client that asked for it and got a quiet partial result would report
        // success for something that did not happen.
        if include_files {
            return Err(RewindError::FilesUnsupported);
        }
        if self.rewinding {
            return Err(RewindError::Busy);
        }
        if self.thread.session_id.is_none() || !self.backend_supports_rewind() {
            return Err(RewindError::Unsupported);
        }
        // The ordinal must name a user message *in this thread*. A stale phone
        // asking to rewind to a turn that no longer exists must be refused, not
        // clamped to the nearest one — truncating at a point the user did not
        // choose destroys turns they meant to keep.
        let entry_index = self
            .thread
            .user_entry_index(ordinal)
            .ok_or(RewindError::OrdinalMismatch)?;

        // Any open confirm card is for a different target and would be stale
        // once this lands.
        self.rewind_confirm = None;
        self.perform_rewind(ordinal, entry_index, None, cx);
        Ok(())
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

    /// Regenerate the assistant reply at `assistant_entry_idx`: rewind to the
    /// user turn that produced it and re-send that same prompt so a fresh reply
    /// streams in. Reuses the rewind machinery (session-file fork + respawn-
    /// then-send) verbatim, so the resumed CLI session matches the truncated
    /// transcript — never a second truncation path. Anything after the target
    /// turn is dropped (rewind semantics); on the last reply that's nothing.
    /// Conversation-only (no files axis): a re-roll shouldn't revert the repo.
    pub(super) fn regenerate(&mut self, assistant_entry_idx: usize, cx: &mut Context<Self>) {
        if self.thread.turn_active
            || self.rewinding
            || self.thread.session_id.is_none()
            || !self.backend_supports_rewind()
        {
            return;
        }
        // Only the LAST turn's reply is regenerable. Regenerating an earlier reply
        // would fork the session and drop every later turn — a destructive,
        // unconfirmed action. The UI hides the affordance off-tail; this guards
        // the method itself so no caller can trip it.
        if self
            .thread
            .entries
            .iter()
            .skip(assistant_entry_idx + 1)
            .any(|e| matches!(e, ThreadEntry::User { .. }))
        {
            return;
        }
        // The prompt that produced this reply is the nearest preceding user turn.
        let end = assistant_entry_idx.min(self.thread.entries.len());
        let Some(user_idx) =
            self.thread.entries[..end].iter().rposition(|e| matches!(e, ThreadEntry::User { .. }))
        else {
            return; // no user prompt precedes this reply
        };
        let Some(ordinal) = self.user_ordinal_at(user_idx) else { return };
        let (text, images) = match self.thread.entries.get(user_idx) {
            Some(ThreadEntry::User { text, images, .. }) => (text.clone(), images.clone()),
            _ => return,
        };
        // The rewind lands, respawns on the forked session, then re-sends this
        // (same prompt, unchanged) — `finish_rewind` drains `rewind_then_send`.
        self.rewind_then_send = Some((text, images));
        self.perform_rewind(ordinal, user_idx, None, cx);
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

        // Whether the backend rewinds server-side (Codex `thread/fork` on the live
        // connection) vs the client's kill-then-file-fork (Claude) — decided
        // before the connection is taken.
        let server_side = self
            .connection
            .as_ref()
            .is_some_and(|c| c.rewind_is_server_side());
        // The full user-turn count, so a server-side fork can fail closed when its
        // ledger doesn't cover the whole transcript (a restored session).
        let total_user_msgs =
            self.thread.entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count();
        // Step 1 input: the connection is MOVED into the background task so
        // `cancel_and_wait` can block there. From here the view has no live
        // connection until `finish_rewind` respawns (or restores `interrupted`).
        let conn = self.connection.take();
        let engine = self.checkpoint_engine.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<RewindOutcome>();
        handle.spawn(async move {
            let outcome = run_rewind_background(conn, engine, old_sid, ordinal, expected_text,
                sha_for_restore, server_side, total_user_msgs).await;
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
                // `truncate_to_user` bumps the thread's revision, which also
                // covers the session-id swap below — the two land together.
                self.thread.truncate_to_user(ordinal);
                // Tell remote subscribers to truncate too. A rewind mutates the
                // thread directly rather than through the event stream, so
                // without this a subscribed phone keeps the dropped tail and
                // then appends the replacement turns after it — a transcript
                // showing a conversation that never happened.
                //
                // Ingested here, after the fork succeeded, so a failed rewind
                // (the `Failed` arm below) leaves every subscriber's transcript
                // exactly as the desktop's own.
                if let Some(binding) = &self.remote {
                    binding.ingest(oximux_agents::thread::ThreadEvent::Rewound { ordinal });
                }
                // Entry indices shift when the tail is dropped — clear the jump
                // highlight so it can't tint the wrong bubble. The rail/list read
                // live entries each render, so their hover state needs no reset.
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
#[allow(clippy::too_many_arguments)]
async fn run_rewind_background(
    conn: Option<std::sync::Arc<dyn oximux_agents::thread::AgentConnection>>,
    engine: Option<std::sync::Arc<CheckpointEngine>>,
    old_sid: String,
    ordinal: usize,
    expected_text: String,
    sha_for_restore: Option<String>,
    server_side: bool,
    total_user_msgs: usize,
) -> RewindOutcome {
    // Server-side rewind (Codex): fork the thread on the LIVE connection FIRST
    // (the process must still be alive to answer `thread/fork`), THEN stop it —
    // the respawn resumes the forked thread id. No session-file fork, no
    // checkpoint restore (conversation-only). The original thread is untouched.
    if server_side {
        let Some(conn) = conn else {
            return RewindOutcome::Failed("no connection to rewind".into());
        };
        let forked = tokio::task::spawn_blocking(move || {
            let res = conn.fork_conversation(ordinal, total_user_msgs);
            // Whether the fork succeeded or not, the process is stopped here —
            // `finish_rewind` respawns (resuming the fork on success, or the
            // original session on failure).
            let _ = conn.cancel_and_wait();
            conn.shutdown();
            res
        })
        .await;
        return match forked {
            Ok(Ok(new_sid)) => RewindOutcome::Done { new_sid, files_degraded: false },
            Ok(Err(e)) => RewindOutcome::Failed(e.to_string()),
            Err(e) => RewindOutcome::Failed(format!("rewind task: {e}")),
        };
    }

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
