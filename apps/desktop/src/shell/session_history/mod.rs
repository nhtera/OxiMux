//! Session-history picker modal (`⌘⇧H`).
//!
//! A centered overlay — same shape as the command palette — listing past
//! Claude Code and Codex sessions newest-first. Type to fuzzy-filter, or narrow
//! to one agent family with the header chips (`All | Claude | Codex | OpenCode`,
//! cycled by `Tab`). `↵` imports the highlighted session on its configured
//! surface — a chat tab when the agent's resolved open mode is Chat, else a
//! terminal resume (see [`SessionHistoryModal::import_selected`]); `⇧↵` forks,
//! `⌘↵` force-opens as chat. The index is built off the main thread on open (a
//! small head/tail read per log) and the chosen session is relaunched by
//! dispatching [`ResumeAgentSession`] / [`OpenChatSession`], handled in
//! `WorkspaceRoot` so it can resolve the active project's cwd when a Codex
//! entry doesn't record one.
//!
//! All non-GPUI logic — row labels, fuzzy filtering, resume/fork mapping —
//! lives in [`picker`] so it stays unit-testable; this file is the thin view.

mod fulltext;
mod fulltext_rows;
pub mod jump;
pub mod picker;

pub use fulltext::{HistoryContext, HistoryScope};

use gpui::{
    AnyElement, Animation, AnimationExt, App, AppContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    hsla, prelude::FluentBuilder, px,
};
use gpui_component::input::{
    Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use gpui_component::{Icon, IconName};
use oximux_settings::{Density, Theme, Typography};

use oximux_agents::session_log::{
    import_provider_index::load_import_provider_preview,
    now_unix_ms,
    session_index::{SessionEntry, SessionIndex, SessionScope},
    session_preview::{PreviewMessage, PreviewRole, load_session_preview},
};

use crate::actions::{
    CycleSessionTypeFilter, OpenChatSession, OpenHistoryEntryAsChat, ResumeAgentSession,
    ToggleSessionHistoryScope,
};
use crate::shell::agent_ui::agent_presentation::adapter_icon_path;
use crate::shell::session_history::picker::{AGENT_TYPE_FILTERS, AgentTypeFilter, LaunchKind};
use crate::ui::FloatingSurface;

/// Key context carried by the modal's root while it's open, so the
/// `Cmd+Enter` binding below only participates in dispatch when the modal is
/// focused (mirrors the terminal's context-scoped shadow bindings).
const SESSION_HISTORY_KEY_CONTEXT: &str = "SessionHistoryModal";

/// The same context narrowed to the modal's focused search `Input`. The
/// input binds `Tab`, `Cmd+Enter` and `Ctrl+A` itself, and GPUI ranks a
/// binding by the depth its context matches at — so a plain
/// `SessionHistoryModal` binding loses to the input's. Matching at the
/// input's own depth ties it, and the later registration (ours, after
/// `gpui_component::init`) wins the tie.
const SESSION_HISTORY_INPUT_KEY_CONTEXT: &str = "SessionHistoryModal > Input";

/// Install the modal-scoped bindings: `Cmd+Enter` → [`OpenHistoryEntryAsChat`],
/// `Tab` → [`CycleSessionTypeFilter`], `Ctrl+A` → [`ToggleSessionHistoryScope`].
/// Context-scoped so they never shadow those chords elsewhere. Called once at
/// boot alongside the global keymap. They must be keymap actions (not
/// `on_key_down` cases): macOS delivers Cmd-modified keys via
/// `performKeyEquivalent:`, GPUI consumes `Tab` for focus-navigation, and the
/// focused search input claims all three before element key listeners run.
/// Each is bound twice — for the modal root and for the search input inside it.
pub fn register_session_history_key_bindings(cx: &mut App) {
    let mut bindings = Vec::new();
    for context in [SESSION_HISTORY_KEY_CONTEXT, SESSION_HISTORY_INPUT_KEY_CONTEXT] {
        bindings.push(gpui::KeyBinding::new(
            "secondary-enter",
            OpenHistoryEntryAsChat,
            Some(context),
        ));
        bindings.push(gpui::KeyBinding::new("tab", CycleSessionTypeFilter, Some(context)));
        bindings.push(gpui::KeyBinding::new("ctrl-a", ToggleSessionHistoryScope, Some(context)));
    }
    cx.bind_keys(bindings);
}

const MODAL_WIDTH: f32 = 940.0;
const LIST_WIDTH: f32 = 380.0;
const MODAL_TOP_OFFSET_PX: f32 = 96.0;
const HEADER_HEIGHT: f32 = 44.0;
const FOOTER_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 46.0;
const LIST_MAX_HEIGHT: f32 = ROW_HEIGHT * 10.0;
const SCRIM_ALPHA: f32 = 0.20;
/// Opening turns pulled into the preview pane for the highlighted session.
const PREVIEW_MAX_MESSAGES: usize = 8;

/// Emitted when the modal closes, so `WorkspaceRoot` can reclaim keyboard
/// focus (the modal grabs focus on open).
pub enum SessionHistoryEvent {
    Closed,
    /// Focus the open tab already running a session (full-text hit).
    JumpToTab { project_id: String, uid: u64 },
    /// Open Settings › Agents at the session-search card.
    OpenSessionSearchSettings,
}

pub struct SessionHistoryModal {
    open: bool,
    query: String,
    selected_idx: usize,
    entries: Vec<SessionEntry>,
    loading: bool,
    /// Captured at open time so row ages render consistently for the frame.
    now_ms: i64,
    /// Active project's launch dirs (root + worktrees), the active worktree,
    /// and the open agent tabs — captured at open.
    context: HistoryContext,
    /// Which sessions are listed (⌃A cycles the offered scopes). Without an
    /// active project only `All` is offered.
    scope: HistoryScope,
    /// Transcript search (session search on, query ≥ 2 chars).
    fulltext: fulltext::FullText,
    /// Which agent-type segment the list is narrowed to (chips + `Tab`).
    type_filter: AgentTypeFilter,
    /// Entry index the preview pane currently shows (or is loading). `None`
    /// before the first load / when nothing is selected.
    preview_idx: Option<usize>,
    /// Opening turns of the highlighted session, loaded lazily off-thread.
    preview_msgs: Vec<PreviewMessage>,
    preview_loading: bool,
    /// Bumped per preview load so a slow read can't clobber a newer selection.
    preview_gen: u64,
    _preview_task: Option<Task<()>>,
    /// The search field — a real text input, so paste, IME composition and
    /// caret movement work. Created lazily on first `open` (the constructor
    /// runs without a `&mut Window`, which `InputState` needs). `query`
    /// mirrors its value via `_query_sub`.
    query_input: Option<Entity<InputState>>,
    _query_sub: Option<Subscription>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    _load_task: Option<Task<()>>,
}

impl SessionHistoryModal {
    pub fn new(theme: Theme, density: Density, typography: Typography, cx: &mut Context<Self>) -> Self {
        Self {
            open: false,
            query: String::new(),
            selected_idx: 0,
            entries: Vec::new(),
            loading: false,
            now_ms: 0,
            context: HistoryContext::default(),
            scope: HistoryScope::All,
            fulltext: fulltext::FullText::default(),
            type_filter: AgentTypeFilter::All,
            preview_idx: None,
            preview_msgs: Vec::new(),
            preview_loading: false,
            preview_gen: 0,
            _preview_task: None,
            query_input: None,
            _query_sub: None,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _load_task: None,
        }
    }

    /// Open the modal, focus its query field, and scan past sessions.
    ///
    /// `context.project_paths` are the active project's launch dirs (root +
    /// worktrees); the default view lists only those, mirroring Claude Code's
    /// same-repo `/resume`. An empty list (no active project) opens the all
    /// view.
    pub fn open(&mut self, context: HistoryContext, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.query.clear();
        self.selected_idx = 0;
        self.now_ms = now_unix_ms();
        self.scope = if context.project_paths.is_empty() { HistoryScope::All } else { HistoryScope::Project };
        self.context = context;
        self.fulltext.reset(cx);
        self.type_filter = AgentTypeFilter::All;
        let input = self.ensure_query_input(window, cx);
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.focus_query(window, cx);
        self.rescan(cx);
        cx.notify();
    }

    /// The search input, created on first use and mirrored into `query` on
    /// every edit (typing, paste, IME commit, delete).
    fn ensure_query_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.query_input {
            return input.clone();
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search past sessions…"));
        let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.query = input.read(cx).value().to_string();
                this.selected_idx = 0;
                this.schedule_fulltext(cx);
                this.refresh_preview(cx);
                cx.notify();
            }
        });
        self.query_input = Some(input.clone());
        self._query_sub = Some(sub);
        input
    }

    /// What should hold focus while the modal is open: the search input, or
    /// the modal root before the input exists.
    fn query_focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.query_input {
            Some(input) => input.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }

    /// Put keyboard focus in the search input (falls back to the modal root
    /// before the input exists). Typing and paste must land in the field;
    /// nav keys still reach the modal through its capture-phase handlers.
    fn focus_query(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.query_input {
            Some(input) => {
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// `Enter` in any of its forms: plain ↵ imports on the session's
    /// configured surface (chat when its open mode is Chat + chat-capable,
    /// else a terminal resume); ⇧↵ forks into a terminal.
    fn confirm(&mut self, shift: bool, window: &mut Window, cx: &mut Context<Self>) {
        if shift {
            self.launch_selected(LaunchKind::Fork, window, cx);
        } else {
            self.import_selected(self.selected_idx, window, cx);
        }
    }

    /// Set the agent-type segment (chip click), reset the selection, and
    /// refresh the preview against the new list. No re-scan — the index already
    /// holds every adapter; the filter is applied in `filtered()`.
    fn set_type_filter(&mut self, filter: AgentTypeFilter, cx: &mut Context<Self>) {
        if self.type_filter == filter {
            return;
        }
        self.type_filter = filter;
        self.selected_idx = 0;
        self.schedule_fulltext(cx);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Advance to the next agent-type segment (`Tab`).
    fn cycle_type_filter(&mut self, cx: &mut Context<Self>) {
        self.set_type_filter(self.type_filter.next(), cx);
    }

    /// Advance to the next offered scope (⌃A): worktree → project → all when
    /// session search is on, project ↔ all otherwise. No-op without an active
    /// project — that view is already all-projects.
    fn cycle_scope(&mut self, cx: &mut Context<Self>) {
        let next = self.scope.next(&self.offered_scopes());
        self.set_scope(next, cx);
    }

    fn set_scope(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.selected_idx = 0;
        self.rescan(cx);
        self.schedule_fulltext(cx);
        cx.notify();
    }

    /// (Re)build the index for the current scope on the background executor,
    /// then publish back on the main thread. `SessionIndex::build` is blocking
    /// std::fs, so it never runs on the UI thread.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        self.loading = true;
        // The previous scope's preview is stale; force a reload once the new
        // entries land.
        self.preview_idx = None;
        self.preview_msgs.clear();
        let scope = match self.scope {
            HistoryScope::All => SessionScope::AllProjects,
            HistoryScope::Project => SessionScope::Projects(self.context.project_paths.clone()),
            HistoryScope::Worktree => {
                SessionScope::Projects(self.context.worktree_path.iter().cloned().collect())
            }
        };
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let entries = executor
                .spawn(async move {
                    match dirs::home_dir() {
                        Some(home) => SessionIndex::build(
                            &home.join(".claude"),
                            &home.join(".codex"),
                            &home,
                            &scope,
                        ),
                        None => Vec::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.entries = entries;
                this.loading = false;
                this.refresh_preview(cx);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    /// Load the highlighted session's opening exchange into the preview pane
    /// if it isn't already shown. Reads the log off-thread; a generation guard
    /// drops a stale result when the selection moved on before it finished.
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        // Full-text hits preview their snippet; nothing to load.
        if self.fulltext_active() {
            self.preview_idx = None;
            self.preview_loading = false;
            return;
        }
        let order = self.filtered();
        let Some(&entry_idx) = order.get(self.selected_idx) else {
            self.preview_idx = None;
            self.preview_msgs.clear();
            self.preview_loading = false;
            return;
        };
        if self.preview_idx == Some(entry_idx) {
            return; // already showing (or loading) this session
        }
        let (path, preset_id, session_id) = self
            .entries
            .get(entry_idx)
            .map(|e| (e.path.clone(), e.preset_id.clone(), e.session_id.clone()))
            .unwrap_or_default();
        self.preview_idx = Some(entry_idx);
        self.preview_msgs.clear();
        self.preview_gen = self.preview_gen.wrapping_add(1);
        let generation = self.preview_gen;
        // Nothing to preview only when there's neither a transcript file (Claude/
        // Pi) nor an import-provider store to read (OpenCode/Copilot). Codex rows
        // carry no path and no preset, so they still fall through to empty.
        if path.is_none() && preset_id.is_none() {
            self.preview_loading = false;
            cx.notify();
            return;
        }
        self.preview_loading = true;
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let msgs = executor
                .spawn(async move {
                    // Import-provider rows read their own store (SQLite / Pi JSONL);
                    // native Claude rows read the transcript `.jsonl`.
                    if let Some(preset) = preset_id {
                        match dirs::home_dir() {
                            Some(home) => load_import_provider_preview(
                                &home,
                                &preset,
                                &session_id,
                                path.as_deref(),
                                PREVIEW_MAX_MESSAGES,
                            ),
                            None => Vec::new(),
                        }
                    } else if let Some(path) = path {
                        load_session_preview(std::path::Path::new(&path), PREVIEW_MAX_MESSAGES)
                    } else {
                        Vec::new()
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_gen == generation {
                    this.preview_msgs = msgs;
                    this.preview_loading = false;
                    cx.notify();
                }
            });
        });
        self._preview_task = Some(task);
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        // Guard the emit on a real open→closed transition: `close()` may run on
        // an already-closed modal (overlay teardown), and an unconditional emit
        // would queue a workspace refocus that lands after a later open, stealing
        // keyboard focus. Mirrors the command palette.
        let was_open = self.open;
        self.open = false;
        self.query.clear();
        self.selected_idx = 0;
        if was_open {
            cx.emit(SessionHistoryEvent::Closed);
        }
        cx.notify();
    }

    fn filtered(&self) -> Vec<usize> {
        picker::filter_sessions_typed(&self.query, self.type_filter, &self.entries)
    }

    /// Short scope label for the header: the worktree or project folder name
    /// when scoped, "All projects" in the all view (or with no active project).
    fn scope_label(&self) -> String {
        let path = match self.scope {
            HistoryScope::All => return "All projects".to_string(),
            HistoryScope::Worktree => self.context.worktree_path.as_ref(),
            HistoryScope::Project => self.context.project_paths.first(),
        };
        path
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
            .unwrap_or_else(|| "All projects".to_string())
    }

    fn move_selection(&mut self, delta: isize, row_count: usize, cx: &mut Context<Self>) {
        if row_count == 0 {
            return;
        }
        self.selected_idx =
            crate::shell::project_picker::wrap_index(self.selected_idx, delta, row_count);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Relaunch the selected session (title row or full-text hit): dispatch a
    /// resume/fork action and close. No-op with nothing selected.
    fn launch_selected(&mut self, kind: LaunchKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        // Pi resumes by rollout file path (`pi --session <file>`); OpenCode /
        // Copilot / omp resume by session id — for omp the id MUST stay the
        // full canonical UUID (its resolver prefix-matches and falls back
        // across projects silently). Native rows carry no preset_id.
        let resume_handle = if entry.preset_id.as_deref() == Some("pi") {
            entry.path.clone().unwrap_or_else(|| entry.session_id.clone())
        } else {
            entry.session_id.clone()
        };
        let action = ResumeAgentSession {
            session_id: entry.session_id.clone(),
            adapter: entry.adapter,
            preset_id: entry.preset_id.clone(),
            resume_handle,
            // Empty when the log omits cwd (Codex index): the handler falls
            // back to the active project's directory.
            cwd: entry.cwd.clone().unwrap_or_default(),
            fork: kind == LaunchKind::Fork,
        };
        self.close(cx);
        window.dispatch_action(Box::new(action), cx);
    }

    /// Reopen the selected session as a chat tab: dispatch [`OpenChatSession`]
    /// — which imports the transcript and spawns a resumed chat — then close.
    /// No-op for adapters without a chat runner (ACP presets, terminal-only)
    /// or with nothing selected.
    fn open_as_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !entry_opens_as_chat(&entry) {
            return;
        }
        let action = OpenChatSession {
            session_id: entry.session_id.clone(),
            // Empty when the log omits a transcript path: the handler imports/
            // locates the transcript itself (Codex) or resumes with no
            // pre-rendered history (Claude with no path). For Pi, this is the
            // rollout path the transcript mapper reads.
            path: entry.path.clone().unwrap_or_default(),
            cwd: entry.cwd.clone().unwrap_or_default(),
            adapter: entry.adapter,
            // `Some` for OpenCode/Pi → the handler builds a transcript bridge
            // instead of a live resume.
            preset_id: entry.preset_id.clone(),
        };
        self.close(cx);
        window.dispatch_action(Box::new(action), cx);
    }

    /// Default import for a row (click / plain `↵`): open the session on the
    /// surface its adapter is configured for. Routes to the structured chat view
    /// when the resolved open mode is `Chat` and the adapter is chat-capable —
    /// the same gate the new-agent launcher uses (`open_mode_for` layers a
    /// per-agent override + a preset's Chat default over the global
    /// `default_open_mode`) — otherwise resumes in a terminal. So a user who set
    /// their agent to open as chat gets a chat tab on import; the classic
    /// terminal-default user is unchanged.
    ///
    /// A full-text hit whose session is already open in a tab jumps there
    /// instead — resuming would start a second CLI on the same conversation.
    fn import_selected(&mut self, list_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_idx = list_idx;
        if self.jump_to_live_tab(window, cx) {
            return;
        }
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let id = picker::entry_slug(&entry);
        let open_chat = entry_opens_as_chat(&entry)
            && cx
                .try_global::<oximux_settings::AgentLaunchSettings>()
                .map(|s| s.opens_as_chat(id))
                .unwrap_or(false);
        if open_chat {
            self.open_as_chat(window, cx);
        } else {
            self.launch_selected(LaunchKind::Resume, window, cx);
        }
    }
}

/// Whether a session can reopen as a chat tab. Claude and Codex import into the
/// live chat surface (Claude via `--resume` + JSONL, Codex via `thread/resume` +
/// rollout). OpenCode and Pi open as a transcript-only **bridge** — their store
/// yields a readable transcript, but they have no in-app chat backend, so the
/// tab seeds the history and swaps the composer for Resume-in-terminal. Copilot
/// stays resume-only (no transcript mapper wired); ACP presets / custom terminal
/// resume in a terminal.
pub(crate) fn entry_opens_as_chat(entry: &SessionEntry) -> bool {
    if matches!(
        entry.adapter,
        oximux_core::AgentAdapter::ClaudeCode | oximux_core::AgentAdapter::Codex
    ) {
        return true;
    }
    matches!(entry.preset_id.as_deref(), Some("opencode") | Some("pi") | Some("omp"))
}

impl EventEmitter<SessionHistoryEvent> for SessionHistoryModal {}

impl Focusable for SessionHistoryModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SessionHistoryModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let motion = crate::motion_settings::active(cx);
        let fulltext = self.fulltext_active();
        let order = if fulltext { Vec::new() } else { self.filtered() };
        let row_count = self.row_count();
        let selected = self.selected_idx;
        let entity = cx.entity();
        let show_all = self.scope == HistoryScope::All;
        // Home dir for `~`-abbreviating cwd paths (all-mode rows + preview meta).
        let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
        let selected_entry = order.get(selected).and_then(|&i| self.entries.get(i));
        let rows_cx = fulltext_rows::Ctx {
            theme,
            density,
            typography: &typography,
            entity: entity.clone(),
            now_ms: self.now_ms,
            home: home.as_deref(),
        };

        let mut list = div()
            .id("session-history-list")
            .flex()
            .flex_col()
            .w(px(LIST_WIDTH))
            .flex_shrink_0()
            .px(px(density.pad_overlay))
            .py(px(4.))
            .h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll();

        if fulltext {
            for (i, hit) in self.fulltext.hits.iter().enumerate() {
                let stale = self.fulltext.replacing;
                list = list.child(fulltext_rows::hit_row(i, hit, i == selected, show_all, stale, &rows_cx));
            }
            if self.fulltext.hits.is_empty() && !self.fulltext.loading {
                let msg = match &self.fulltext.error {
                    Some(err) => format!("Search failed: {err}"),
                    None => "No conversations mention that".to_string(),
                };
                list = list.child(hint_row(&msg, theme, &typography));
            }
            list = list.children(fulltext_rows::load_more_row(self, &rows_cx));
        } else if self.loading {
            list = list.child(hint_row("Scanning sessions…", theme, &typography));
        } else if row_count == 0 {
            let seg_msg;
            let msg = if self.type_filter == AgentTypeFilter::OpenCode {
                // OpenCode sessions aren't indexed yet (SQLite store) — the chip
                // exists so the segment is discoverable; indexing is a follow-up.
                "OpenCode session import is coming soon"
            } else if self.entries.is_empty() {
                "No past sessions found"
            } else if self.query.is_empty() && self.type_filter != AgentTypeFilter::All {
                // A specific segment with an empty query but no rows: that adapter
                // simply has no sessions in scope — say so, not "no match".
                seg_msg = format!("No {} sessions", self.type_filter.label());
                seg_msg.as_str()
            } else {
                "No matching sessions"
            };
            list = list.child(hint_row(msg, theme, &typography));
        } else {
            for (i, &entry_idx) in order.iter().enumerate() {
                let entry = &self.entries[entry_idx];
                let title = picker::session_row_title(entry);
                let subtitle =
                    picker::session_row_subtitle(entry, self.now_ms, show_all, home.as_deref());
                let is_selected = i == selected;
                let ent = entity.clone();
                // Each line is a flex-row wrapper holding a min-w-0 text child —
                // the proven single-line-clip structure (a bare nowrap text in a
                // flex-col forces full width and renders nothing). Mirrors the
                // agents dashboard cards.
                let title_line = div().flex().flex_row().w_full().child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(typography.t_body_md))
                        .text_color(theme.fg_base)
                        .child(title),
                );
                let subtitle_line = div().flex().flex_row().w_full().child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(subtitle),
                );
                // Leading agent glyph categorizes each row by adapter (Claude /
                // Codex / …), mirroring the reference import list. Fixed size,
                // never shrinks; the text column takes the rest.
                let icon = Icon::default()
                    .path(adapter_icon_path(picker::entry_slug(entry)))
                    .size(px(15.))
                    .flex_shrink_0()
                    .text_color(theme.fg_muted);
                let text_col = div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.))
                    .flex_1()
                    .min_w_0()
                    .child(title_line)
                    .child(subtitle_line);
                list = list.child(
                    div()
                        .id(("session-row", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px(px(10.))
                        .rounded(px(density.r_xs))
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(theme.selection))
                        .when(!is_selected, |d| d.hover(|s| s.bg(theme.hover_overlay)))
                        .on_mouse_down(
                            MouseButton::Left,
                            move |_e, window, cx| {
                                ent.update(cx, |m, cx| m.import_selected(i, window, cx));
                            },
                        )
                        .child(icon)
                        .child(text_col),
                );
            }
        }

        if !self.fulltext.available
            && cx.has_global::<crate::session_search_service::SessionSearchService>()
            && self.query.trim().chars().count() >= fulltext::MIN_QUERY_CHARS
        {
            list = list.child(fulltext_rows::enable_hint_row(&rows_cx));
        }

        // Preview pane (right): the highlighted session's opening exchange +
        // metadata, mirroring Claude's `/resume` preview.
        let mut preview = div()
            .id("session-history-preview")
            .flex()
            .flex_col()
            .flex_1()
            .h(px(LIST_MAX_HEIGHT))
            .px(px(16.))
            .py(px(12.))
            .gap(px(10.))
            .overflow_y_scroll();
        if fulltext {
            match self.fulltext.hits.get(selected) {
                Some(hit) => {
                    let live = self.selected_live_tab().is_some();
                    preview = preview.child(fulltext_rows::hit_preview(hit, live, &rows_cx));
                }
                None => {
                    preview = preview.child(preview_hint("Select a result to preview", theme, &typography));
                }
            }
        } else if let Some(entry) = selected_entry {
            let title = picker::session_row_title(entry);
            let mut meta = picker::session_row_subtitle(entry, self.now_ms, true, home.as_deref());
            if let Some(n) = entry.message_count {
                meta.push_str(&format!(" · {n} message{}", if n == 1 { "" } else { "s" }));
            }
            preview = preview.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(typography.t_body_lg))
                            .text_color(theme.fg_base)
                            .child(title),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_size(px(typography.t_sub_label))
                            .text_color(theme.fg_subtle)
                            .child(meta),
                    ),
            );
            preview = preview.child(divider(theme));
            if self.preview_loading {
                preview = preview.child(preview_hint("Loading preview…", theme, &typography));
            } else if self.preview_msgs.is_empty() {
                let msg = if entry.path.is_none() {
                    "No transcript preview for this session"
                } else {
                    "No readable messages in this session"
                };
                preview = preview.child(preview_hint(msg, theme, &typography));
            } else {
                // Import-provider rows label the assistant side with the
                // provider name (OpenCode/Copilot/Pi), not the generic "Assistant".
                let assistant_label = entry
                    .preset_id
                    .as_deref()
                    .map(crate::shell::agent_ui::agent_presentation::adapter_display_name)
                    .unwrap_or_else(|| adapter_display(entry.adapter));
                for m in &self.preview_msgs {
                    preview =
                        preview.child(preview_message(m, assistant_label, theme, &typography));
                }
            }
        } else {
            preview = preview.child(preview_hint(
                "Select a session to preview",
                theme,
                &typography,
            ));
        }

        let body = div()
            .flex()
            .flex_row()
            .w_full()
            .h(px(LIST_MAX_HEIGHT))
            .child(list)
            .child(div().w(px(1.)).h_full().bg(theme.border_inactive))
            .child(preview);

        // Borderless so it reads as part of the header row, not a boxed
        // control inside it.
        let query_field = match &self.query_input {
            Some(input) => Input::new(input)
                .appearance(false)
                .text_size(px(typography.t_body_md))
                .into_any_element(),
            None => div().into_any_element(),
        };

        let scope_control = if self.fulltext.available {
            fulltext_rows::scope_chips(self, &rows_cx)
        } else {
            scope_text(&self.scope_label(), theme, &typography)
        };
        let results_bar = fulltext.then(|| fulltext_rows::results_bar(self, &rows_cx));
        let footer = footer_hints(
            self.scope.next(&self.offered_scopes()),
            self.offered_scopes().len() > 1,
            theme,
            &typography,
        );
        let dismiss_entity = entity.clone();
        let card = div()
            .flex()
            .flex_col()
            .w(px(MODAL_WIDTH))
            .floating_chrome(&theme, &density)
            .overflow_hidden()
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_e, _window, cx| cx.stop_propagation())
            .child(header_row(query_field, scope_control, theme, density, &typography))
            .child(type_filter_chips(
                self.type_filter,
                theme,
                density,
                &typography,
                entity.clone(),
            ))
            .child(divider(theme))
            .children(results_bar)
            .child(body)
            .child(divider(theme))
            .child(footer);

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(MODAL_TOP_OFFSET_PX))
            .bg(hsla(0.0, 0.0, 0.0, SCRIM_ALPHA))
            // Carries the modal's key context so the `Cmd+Enter` binding (see
            // `register_session_history_key_bindings`) resolves only while the
            // modal is focused.
            .key_context(SESSION_HISTORY_KEY_CONTEXT)
            .on_mouse_down(MouseButton::Left, move |_e, _window, cx| {
                dismiss_entity.update(cx, |m, cx| m.close(cx));
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &OpenHistoryEntryAsChat, window, cx| {
                this.open_as_chat(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleSessionTypeFilter, _window, cx| {
                this.cycle_type_filter(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSessionHistoryScope, _window, cx| {
                this.cycle_scope(cx);
            }))
            // The focused search `Input` turns Esc / ↵ / ↑ / ↓ into its own
            // actions before raw key listeners on ancestors run, so they are
            // intercepted at the CAPTURE phase (same contract as
            // `BranchPicker`). ⌘↵ never arrives here as `Enter` — the
            // `SESSION_HISTORY_INPUT_KEY_CONTEXT` binding outranks the input's.
            // Known trade-off: capturing Escape preempts the input's IME-unmark
            // path, so Escape mid-composition closes the modal.
            .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
                cx.stop_propagation();
                this.close(cx);
            }))
            .capture_action(cx.listener(|this, ev: &InputEnter, window, cx| {
                cx.stop_propagation();
                if ev.secondary {
                    this.open_as_chat(window, cx);
                } else {
                    this.confirm(ev.shift, window, cx);
                }
            }))
            .capture_action(cx.listener(move |this, _: &MoveUp, _window, cx| {
                cx.stop_propagation();
                let n = this.row_count();
                this.move_selection(-1, n, cx);
            }))
            .capture_action(cx.listener(move |this, _: &MoveDown, _window, cx| {
                cx.stop_propagation();
                let n = this.row_count();
                this.move_selection(1, n, cx);
            }))
            // Fallback for when focus sits on the modal root rather than the
            // input (e.g. before the input exists).
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.close(cx),
                    "up" => this.move_selection(-1, row_count, cx),
                    "down" => this.move_selection(1, row_count, cx),
                    "enter" => this.confirm(event.keystroke.modifiers.shift, window, cx),
                    _ => {}
                }
            }))
            .child(card.with_animation(
                "session-history-enter",
                Animation::new(motion.m_overlay).with_easing(oximux_settings::ease_out_spring()),
                |el, delta| el.opacity(delta).mt(px(6.0 * (1.0 - delta))),
            ))
            .into_any_element()
    }
}

fn header_row(
    query_field: AnyElement,
    scope: AnyElement,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .h(px(HEADER_HEIGHT))
        .child(
            Icon::new(IconName::Search)
                .size(px(14.))
                .text_color(theme.fg_subtle),
        )
        .child(
            div()
                .px(px(6.))
                .py(px(2.))
                .bg(theme.bg_panel_alt)
                .rounded(px(density.r_xs))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child("History"),
        )
        // Current scope — the folder name / "All projects", or the scope
        // chips when session search is on (⌃A cycles either).
        .child(scope)
        .child(div().flex_1().min_w_0().child(query_field))
}

/// The scope as plain text (session search off).
fn scope_text(label: &str, theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .max_w(px(220.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(label.to_string())
        .into_any_element()
}

/// The agent-type segment chips (`All | Claude | Codex | OpenCode`). The active
/// segment is filled; the rest are dim + hover-lit. Clicking a chip narrows the
/// list via [`SessionHistoryModal::set_type_filter`]; `Tab` cycles them.
fn type_filter_chips(
    active: AgentTypeFilter,
    theme: Theme,
    density: Density,
    typography: &Typography,
    entity: gpui::Entity<SessionHistoryModal>,
) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .px(px(12.))
        .py(px(6.));
    for (i, &filter) in AGENT_TYPE_FILTERS.iter().enumerate() {
        let is_active = filter == active;
        let ent = entity.clone();
        row = row.child(
            div()
                .id(("type-chip", i))
                .px(px(10.))
                .py(px(3.))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .text_size(px(typography.t_sub_label))
                .when(is_active, |d| d.bg(theme.selection).text_color(theme.fg_base))
                .when(!is_active, |d| {
                    d.text_color(theme.fg_subtle)
                        .hover(|s| s.bg(theme.hover_overlay))
                })
                .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
                    cx.stop_propagation();
                    // A chip is a child of the modal's `track_focus` root; the
                    // mouse-down blurs the modal's focus handle, which would
                    // otherwise leave search/↑↓/Tab/Esc dead while the modal
                    // stays open. Restore focus — deferred so it lands after the
                    // click's own focus settling (see the focus-in-mousedown
                    // clobber pattern).
                    let handle = ent.update(cx, |m, cx| {
                        m.set_type_filter(filter, cx);
                        match &m.query_input {
                            Some(input) => input.read(cx).focus_handle(cx),
                            None => m.focus_handle.clone(),
                        }
                    });
                    window.defer(cx, move |window, cx| window.focus(&handle, cx));
                })
                .child(filter.label()),
        );
    }
    row
}

/// Friendly name for the assistant side of a previewed transcript.
pub(crate) fn adapter_display(adapter: oximux_core::AgentAdapter) -> &'static str {
    match adapter {
        oximux_core::AgentAdapter::ClaudeCode => "Claude",
        oximux_core::AgentAdapter::Codex => "Codex",
        oximux_core::AgentAdapter::Pi => "Pi",
        oximux_core::AgentAdapter::Omp => "omp",
        oximux_core::AgentAdapter::Custom => "Assistant",
    }
}

/// A dim status line inside the preview pane (loading / empty / no-selection).
fn preview_hint(text: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_subtle)
        .child(text.to_string())
}

/// One previewed turn: a role label over its wrapping text body.
fn preview_message(
    m: &PreviewMessage,
    assistant_label: &str,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    let (label, label_color) = match m.role {
        PreviewRole::User => ("You".to_string(), theme.focus_ring),
        PreviewRole::Assistant => (assistant_label.to_string(), theme.fg_muted),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .w_full()
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(label_color)
                .child(label),
        )
        .child(
            div()
                .w_full()
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(m.text.clone()),
        )
}

fn hint_row(text: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(10.))
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_subtle)
        .child(text.to_string())
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).bg(theme.border_inactive)
}

fn footer_hints(
    next_scope: HistoryScope,
    can_cycle: bool,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    let hint = |label: &str| -> gpui::Div {
        div()
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(label.to_string())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.))
        .h(px(FOOTER_HEIGHT))
        .px(px(12.))
        .child(hint("↑↓ navigate"))
        .child(hint("⇥ filter"))
        .child(hint("↵ open"))
        .child(hint("⇧↵ fork"))
        .child(hint("⌘↵ open as chat"))
        // The scope toggle is meaningless with no active project (always all).
        .when(can_cycle, |d| {
            d.child(hint(match next_scope {
                HistoryScope::Worktree => "⌃A this worktree",
                HistoryScope::Project => "⌃A this project",
                HistoryScope::All => "⌃A all projects",
            }))
        })
        .child(hint("esc dismiss"))
}
