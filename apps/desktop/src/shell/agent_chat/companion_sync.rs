//! Fold companion-terminal turns back into the chat transcript.
//!
//! A chat tab's companion terminal (⌃⇧V) resumes the SAME session id
//! interactively, so turns typed there land only in the CLI's session log
//! (`~/.claude/projects/<slug>/<sid>.jsonl`) — nothing arrives on the chat
//! view's connection, and without this the chat silently renders a stale
//! transcript until the tab is reopened from history. The hook is the switch
//! back from Terminal to Chat view ([`AgentChatView::set_view_mode`]): re-fold
//! the log off the UI thread and append the suffix the thread hasn't seen.

use std::path::PathBuf;

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
    /// Every backend with a companion resumes the session IN PLACE (Claude
    /// appends to the per-project session jsonl; Codex `resume` appends to the
    /// same rollout file — verified on codex 0.145; Pi `--session <id>`
    /// appends to the same rollout — verified on pi 0.80; OpenCode `--session
    /// <id>` continues the same SQLite session rows), so re-folding the store
    /// captures terminal-typed turns. ACP presets other than opencode never
    /// spawn a companion (`interactive_resume: None`) — skipped. Also skipped
    /// while a chat turn is in flight so an import can't interleave a
    /// streaming reply.
    ///
    /// A non-empty fold also RESPAWNS the chat's live connection: the old one
    /// never learned the terminal's turns, so its next send would answer
    /// without them — and, on parent-linked stores (Claude, Pi), fork the
    /// session tree and orphan them from context permanently.
    ///
    /// Known limitation: a turn still streaming IN THE TERMINAL at toggle time
    /// imports its reply as-written-so-far; the missing remainder arrives on a
    /// later reopen, not on the next toggle (the anchor has moved past it).
    pub(super) fn sync_from_companion_terminal(&mut self, cx: &mut Context<Self>) {
        if self.companion_session.is_none() || self.thread.turn_active {
            return;
        }
        let Some(session_id) = self.thread.session_id.clone() else {
            return;
        };
        let Some(home) = dirs::home_dir() else {
            return;
        };
        // Where the companion's turns land, per backend. Claude's path is
        // deterministic; Codex/Pi rollouts embed the id in the filename and
        // are found by a bounded walk of their session trees (off the UI
        // thread); OpenCode's SQLite store is keyed by the session id itself.
        enum CompanionLog {
            Claude(PathBuf),
            Codex { codex_dir: PathBuf, thread_id: String },
            Pi { home: PathBuf, session_id: String },
            Omp { home: PathBuf, session_id: String },
            OpenCode { home: PathBuf, session_id: String },
        }
        let source = match self.backend.transport {
            Transport::StreamJson => CompanionLog::Claude(
                oximux_agents::session_log::project_log_dir(&home.join(".claude"), &self.cwd)
                    .join(format!("{session_id}.jsonl")),
            ),
            Transport::AppServer => CompanionLog::Codex {
                codex_dir: home.join(".codex"),
                thread_id: session_id,
            },
            Transport::Rpc => CompanionLog::Pi { home, session_id },
            // omp's store, NOT the pi home — the two dialects' rollouts live
            // in different roots even though the file format is shared.
            Transport::OmpRpc => CompanionLog::Omp { home, session_id },
            Transport::Acp => {
                // Only opencode has an interactive-resume companion AND a
                // readable store importer keyed by the agent-supplied id.
                let is_opencode = self
                    .backend
                    .acp_command
                    .as_deref()
                    .and_then(|cmd| {
                        oximux_settings::ACP_PRESETS.iter().find(|p| p.command == cmd)
                    })
                    .is_some_and(|p| p.id == "opencode");
                if !is_opencode {
                    return;
                }
                CompanionLog::OpenCode { home, session_id }
            }
        };
        let known = self.known_user_turns();
        cx.spawn(async move |this, cx| {
            let tail = cx
                .background_spawn(async move {
                    let folded = match source {
                        CompanionLog::Claude(path) => {
                            oximux_agents::thread::transcript_from_jsonl(&path)
                                .unwrap_or_default()
                        }
                        CompanionLog::Codex { codex_dir, thread_id } => {
                            oximux_agents::thread::locate_rollout(&codex_dir, &thread_id)
                                .and_then(|p| {
                                    oximux_agents::thread::import_codex_rollout(&p).ok()
                                })
                                .map(|import| import.entries)
                                .unwrap_or_default()
                        }
                        CompanionLog::Pi { home, session_id } => {
                            oximux_agents::session_log::import_transcript_pi::locate_pi_session(
                                &home,
                                &session_id,
                            )
                            .map(|p| {
                                oximux_agents::session_log::import_transcript_pi::pi_transcript(
                                    &p,
                                )
                            })
                            .unwrap_or_default()
                        }
                        CompanionLog::Omp { home, session_id } => {
                            oximux_agents::session_log::import_transcript_pi::locate_omp_session(
                                &home,
                                &session_id,
                            )
                            .map(|p| {
                                oximux_agents::session_log::import_transcript_pi::pi_transcript(
                                    &p,
                                )
                            })
                            .unwrap_or_default()
                        }
                        CompanionLog::OpenCode { home, session_id } => {
                            oximux_agents::session_log::import_transcript_opencode::opencode_transcript(
                                &home,
                                &session_id,
                            )
                        }
                    };
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
                this.follow_bottom();
                // The chat's live connection loaded the session at spawn and
                // never re-reads the log — after the terminal advanced it, the
                // connection's in-memory context is missing those turns, and
                // for parent-linked stores (Claude, Pi) its next send would
                // fork the session tree, permanently orphaning them from
                // context. Respawn (a fresh resume re-reads the full log) so
                // the next send parents on the current leaf. A dormant chat
                // has no connection yet and already resumes lazily on first
                // send — nothing to refresh.
                if this.connection.is_some() {
                    this.respawn(cx);
                }
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
