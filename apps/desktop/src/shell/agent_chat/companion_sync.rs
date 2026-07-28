//! Fold companion-terminal turns back into the chat transcript.
//!
//! A chat tab's companion terminal (⌃⇧V) resumes the SAME session id
//! interactively, so turns typed there land only in the CLI's session log
//! (`~/.claude/projects/<slug>/<sid>.jsonl`) — nothing arrives on the chat
//! view's connection, and without this the chat silently renders a stale
//! transcript until the tab is reopened from history. The hook is the switch
//! back from Terminal to Chat view ([`AgentChatView::set_view_mode`]): re-fold
//! the log off the UI thread and append the suffix the thread hasn't seen.

use gpui::{AppContext as _, Context, Window};

use oximux_agents::thread::{ThreadEntry, Transport};
use oximux_agents::SharedBackend;
use oximux_core::AgentSessionId;
use oximux_pty::TerminalSessionId;

use crate::shell::context_env::SurfaceIds;
use crate::shell::terminal_view::TerminalView;

use super::{AgentChatView, ChatViewMode};

impl AgentChatView {
    /// Mount a freshly-spawned companion terminal (the host did the async
    /// `start_session`) and switch to Terminal view. Builds the `TerminalView`
    /// from this chat's own theme/density/typography, observes it for repaint,
    /// and remembers the session id for reaping on close. A fresh mount is by
    /// definition current — clears the staleness mark.
    pub fn attach_terminal(
        &mut self,
        session: AgentSessionId,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let terminal = cx.new(|cx| {
            TerminalView::mount(backend, term_id, ids, theme, density, typography, window, cx)
        });
        self._terminal_observer = Some(cx.observe(&terminal, |_this, _tv, cx| cx.notify()));
        self.terminal = Some(terminal);
        self.companion_session = Some(session);
        self.chat_advanced_since_companion = false;
        self.view_mode = ChatViewMode::Terminal;
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Record that the chat sent a prompt while a companion terminal exists.
    /// The running interactive CLI loaded the session at spawn time and never
    /// re-reads the log, so from this moment its context is missing turns —
    /// the next switch to Terminal view must respawn it rather than show it.
    pub(super) fn note_chat_prompt_sent(&mut self) {
        if self.companion_session.is_some() {
            self.chat_advanced_since_companion = true;
        }
    }

    /// Whether the companion terminal's CLI is missing chat-sent turns and
    /// must be respawned (fresh `--resume` re-reads the log) instead of shown.
    pub fn companion_terminal_stale(&self) -> bool {
        self.chat_advanced_since_companion
    }

    /// Detach the companion terminal from this view so a fresh one can be
    /// spawned. The caller reaps the CLI's daemon session — this only drops
    /// the view-side state (the `TerminalView` drop releases its subscriber).
    pub fn drop_companion_terminal(&mut self, cx: &mut Context<Self>) {
        self.terminal = None;
        self.companion_session = None;
        self._terminal_observer = None;
        self.chat_advanced_since_companion = false;
        cx.notify();
    }

    /// Switch between chat and terminal view. Terminal requires the companion to
    /// already exist (the host spawns it first via [`Self::attach_terminal`]); a
    /// request for Terminal with no companion is a no-op. Focuses the newly-active
    /// surface — and on the way OUT of the terminal, folds any turns typed there
    /// back into the chat ([`Self::sync_from_companion_terminal`]).
    pub fn set_view_mode(
        &mut self,
        mode: ChatViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if mode == ChatViewMode::Terminal && self.terminal.is_none() {
            return;
        }
        if self.view_mode == mode {
            return;
        }
        let leaving_terminal = self.view_mode == ChatViewMode::Terminal;
        self.view_mode = mode;
        if leaving_terminal {
            self.sync_from_companion_terminal(cx);
        }
        self.focus_active_surface(window, cx);
        cx.notify();
    }

    /// Fold turns typed in the companion terminal back into the chat, on the
    /// switch from Terminal to Chat view.
    ///
    /// Appends the log suffix past the thread's current turns, anchored on the
    /// count of user prompts (see
    /// [`oximux_agents::thread::tail_beyond_known_turns`]).
    ///
    /// Claude stream-json chats only: that's the log format the importer
    /// reads, and the only backend whose companion resumes in-place today
    /// (Codex rolls out its own file; ACP has no companion). Skipped while a
    /// chat turn is in flight so an import can't interleave a streaming reply.
    ///
    /// Known limitation: a turn still streaming IN THE TERMINAL at toggle time
    /// imports its reply as-written-so-far; the missing remainder arrives on a
    /// later reopen, not on the next toggle (the anchor has moved past it).
    pub(super) fn sync_from_companion_terminal(&mut self, cx: &mut Context<Self>) {
        if self.companion_session.is_none()
            || self.thread.turn_active
            || self.backend.transport != Transport::StreamJson
        {
            return;
        }
        let Some(session_id) = self.thread.session_id.clone() else {
            return;
        };
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let log_path =
            oximux_agents::session_log::project_log_dir(&home.join(".claude"), &self.cwd)
                .join(format!("{session_id}.jsonl"));
        let known = self.known_user_turns();
        cx.spawn(async move |this, cx| {
            let tail = cx
                .background_spawn(async move {
                    let folded = oximux_agents::thread::transcript_from_jsonl(&log_path)
                        .unwrap_or_default();
                    oximux_agents::thread::tail_beyond_known_turns(folded, known)
                })
                .await;
            if tail.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                // Re-check the anchor: a prompt sent (or a turn started) while
                // the fold ran would misalign the tail — drop it; the next
                // toggle re-syncs from the fresh state.
                if this.thread.turn_active || this.known_user_turns() != known {
                    return;
                }
                this.thread.append_imported(tail);
                this.stick_to_bottom = true;
                this.list_scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    /// How many user prompts the thread currently holds — the alignment anchor
    /// between this chat's transcript and the session log on disk.
    fn known_user_turns(&self) -> usize {
        self.thread
            .entries
            .iter()
            .filter(|e| matches!(e, ThreadEntry::User { .. }))
            .count()
    }
}
