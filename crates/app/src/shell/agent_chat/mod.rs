//! Agent Chat view — a dedicated tab that renders a Claude Code session as a
//! structured chat thread (user/assistant bubbles, streaming text, collapsible
//! thinking, tool-call lines) instead of a raw terminal.
//!
//! It owns a [`ChatThread`] (the gpui-free conversation model from
//! `oximux-agents`) plus a live [`AgentConnection`] to a headless `claude`
//! subprocess. Decoded events arrive on a background channel; a foreground task
//! folds each into the thread and repaints. The raw-PTY terminal agent path is
//! untouched — this is an additive second surface.
//!
//! Fail-closed: if the subprocess dies (stdout EOF) while a permission is
//! pending, the drain task rejects it rather than leaving a dangling prompt.

mod bubble;
mod composer;
mod composer_history;
mod diff_card;
mod image_attach;
mod pending_edit;
mod plan_panel;
mod question_card;
mod rewind_menu;
mod session_picker;
mod slash_command_catalog;
mod slash_palette;
mod tool_bodies;
mod tool_card;
mod tool_grouping;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext, ClipboardItem, Context, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, Image, ImageSource, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement, Render, ScrollHandle,
    SharedString,
    StatefulInteractiveElement, Styled, StyledImage as _, Subscription, Task, Transformation,
    WeakEntity, Window, div, img, percentage, px, relative,
};
use gpui_component::Icon;
use gpui_component::input::Enter as InputEnter;
use gpui_component::scroll::Scrollbar;

/// Max width of the reading column (transcript + composer). Wider windows keep
/// the conversation centered in a comfortable measure rather than stretching
/// text edge-to-edge — the calm, focused feel of a dedicated chat surface.
pub(super) const CONTENT_MAX_W: f32 = 720.0;

/// How many frames to keep re-pinning the transcript to the bottom after a
/// content change (see [`AgentChatView::follow_frames`]). ~10 frames (≈160ms at
/// 60fps) comfortably outlasts the async markdown parse/layout of a normal reply
/// so the follow catches the message's settled height, then stops (no idle spin).
const FOLLOW_FRAMES: u8 = 10;

/// Claude model aliases offered in the in-chat model picker. The CLI accepts
/// these short aliases directly as `--model`. (There is no model *list* in
/// settings — only a single default — so the selectable set is fixed here.)
pub(super) const CLAUDE_MODELS: &[&str] = &["opus", "sonnet", "haiku"];

/// Permission modes offered in the in-chat mode picker, as `(wire, label)`. The
/// wire value is passed to `--permission-mode`; the label is what the user sees.
/// The CLI also accepts `auto`/`dontAsk`, but this is the canonical, well-
/// understood set (matching what other agent front-ends expose):
/// - **default** — prompt before each tool.
/// - **acceptEdits** — auto-approve file edits; still prompt for other tools.
/// - **plan** — read-only planning; no tools execute.
/// - **bypassPermissions** — never prompt (skip all approvals).
pub(super) const CLAUDE_PERMISSION_MODES: &[(&str, &str)] = &[
    ("default", "Ask each time"),
    ("acceptEdits", "Accept edits"),
    ("plan", "Plan mode"),
    ("bypassPermissions", "Bypass all"),
];

/// The wire value treated as the baseline (no `--permission-mode` flag).
pub(super) const DEFAULT_PERMISSION_MODE: &str = "default";

/// Reasoning-effort levels offered in the in-chat effort picker, as
/// `(wire, label)`. The wire value is passed to `--effort`. These are the levels
/// the CLI accepts (`low`/`medium`/`high`/`xhigh`/`max`).
pub(super) const CLAUDE_EFFORTS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
];

/// The effort shown as the current selection when none has been chosen — the
/// CLI's own default. Purely a display label; when the field is `None` no
/// `--effort` flag is passed, so the CLI applies whatever it's configured for.
pub(super) const DEFAULT_EFFORT: &str = "high";

/// Decoded user-attached image thumbnails, memoized by `(entry index, image
/// index)`. Interior-mutable so the immutable `render` path can fill it lazily.
type ImageCache = RefCell<HashMap<(usize, usize), Option<Arc<Image>>>>;

/// Events the chat view raises for its host (the pane group) to act on.
pub enum AgentChatEvent {
    /// The user picked a different model; the host persists it in the tab kind
    /// so the choice survives relaunch (the view already respawned on it).
    ModelChanged(String),
    /// The user chose a past session in the in-chat browser — open it as a new
    /// chat tab. The pane group handles this: activate the tab if the session is
    /// already open, else import the transcript (preferring an OxiMux-native
    /// persisted blob over the raw `.jsonl`) and open a resumed chat.
    OpenSessionAsChat {
        session_id: String,
        /// Session log path for JSONL import (external sessions).
        path: Option<String>,
        /// Directory to root the resumed subprocess in.
        cwd: std::path::PathBuf,
    },
}

/// How assistant thinking blocks are shown across the whole chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ThinkingLevel {
    /// Never render thinking blocks.
    Hidden,
    /// Auto-expand the thought currently streaming; collapse it once the reply
    /// text starts. Past thoughts stay collapsed but remain individually
    /// toggleable. The default — a live "thinking…" peek without clutter.
    #[default]
    Auto,
    /// Always expand every thinking block.
    Expanded,
}

impl ThinkingLevel {
    fn next(self) -> Self {
        match self {
            Self::Hidden => Self::Auto,
            Self::Auto => Self::Expanded,
            Self::Expanded => Self::Hidden,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Hidden => "Thinking: off",
            Self::Auto => "Thinking: auto",
            Self::Expanded => "Thinking: shown",
        }
    }
}

use composer::{ComposerEvent, ComposerView};
use question_card::{QuestionCard, QuestionCardEvent};
use tool_grouping::{plan_tool_grouping, EntryDisplay};
use oximux_agents::thread::{
    AgentConnection, AssistantMessage, ChatImage, ChatThread, ClaudeStreamJsonConnection,
    PermissionDecision, QuestionAnswers, QuestionRequest, ThreadEntry, ThreadEvent, ToolCallStatus,
    TurnUsage,
};
use oximux_settings::{Density, Theme, Typography};

pub struct AgentChatView {
    /// The conversation model. Owned directly (not a nested entity) — the view
    /// is its sole mutator, on the foreground thread.
    thread: ChatThread,
    /// The live agent connection. `None` if the subprocess failed to spawn (a
    /// read-only error state) or after teardown.
    connection: Option<Box<dyn AgentConnection>>,
    /// The bottom composer (status line + input + Send button), isolated into
    /// its own view so typing repaints only it, never the transcript. It reports
    /// submissions back via [`ComposerEvent`].
    composer: Entity<ComposerView>,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,
    /// Whether the transcript auto-follows the bottom. True by default and while
    /// the user stays at the end; set false when they scroll up to read history
    /// (so streaming doesn't yank them down), re-armed when they scroll back to
    /// the bottom or send a new message. `render` re-pins every frame while true,
    /// which keeps the newest row glued even as its height settles a frame after
    /// it arrives (markdown/diff measuring) — a single per-event scroll lands
    /// short in that case.
    stick_to_bottom: bool,
    /// Extra render frames to force after a content change while following. The
    /// markdown renderer parses/lays out ASYNCHRONOUSLY, so the frame a message
    /// arrives its final height isn't known yet — `scroll_to_bottom` pins to a
    /// too-short `content_size` and the newest (tallest) reply tucks under the
    /// composer. The async layout completes on the nested text entity and does
    /// NOT re-run this view's `render`, so the pin never corrects (a tab-switch
    /// re-triggers the same race, which is why it looked permanent). Counting a
    /// few frames down here — each forcing a re-render that re-pins to the now-
    /// settled `content_size` — lets the follow catch the true bottom. The count
    /// is re-armed each frame the scrollable height keeps growing (see
    /// [`Self::last_max_offset`]), so a slow or large async layout is followed to
    /// completion rather than cut off after a fixed number of frames.
    follow_frames: u8,
    /// The transcript's scrollable extent (`max_offset().y`) as of the last
    /// render, used to detect that the async layout is still settling: while this
    /// keeps growing the follow re-arms; once it holds steady the follow winds
    /// down and the frame loop stops (no idle repaint).
    last_max_offset: f32,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Launch context, retained so [`Self::respawn`] can re-spawn the subprocess
    /// (Stop→next-send resume) in the same directory with the same model.
    cwd: PathBuf,
    model: Option<String>,
    /// The active permission mode's wire value (`acceptEdits`, `plan`, …), or
    /// `None`/`"default"` for the CLI default. Like `--model` it's fixed at
    /// spawn, so a live switch respawns via `--resume`. Intentionally *not*
    /// persisted across relaunch: a session should reopen in the safe default
    /// rather than silently inheriting a prior "bypass all".
    permission_mode: Option<String>,
    /// The chosen reasoning-effort level (`low`/`medium`/`high`/`xhigh`/`max`),
    /// or `None` for the CLI's own default. Like `--model` it's fixed at spawn,
    /// so a live switch respawns via `--resume`.
    effort: Option<String>,
    /// Set once the event channel closes (process exit / EOF). Disables sending.
    disconnected: bool,
    /// True after the user pressed Stop: the turn was interrupted and the child
    /// exited, but the session is **resumable** — the next send transparently
    /// respawns with `--resume`. Distinct from `disconnected` (an unexpected
    /// crash, which stays unavailable), so an intentional Stop shows no error.
    interrupted: bool,
    /// Assistant entry indices the user manually EXPANDED (per-entry override,
    /// meaningful in `ThinkingLevel::Auto`).
    expanded_thinking: HashSet<usize>,
    /// Assistant entry indices the user manually COLLAPSED — overrides Auto's
    /// stream auto-expand so a manual collapse registers on the first click even
    /// mid-stream.
    collapsed_thinking: HashSet<usize>,
    /// Chat-wide thinking display level (persisted). Cycled from a pill above
    /// the composer.
    thinking_level: ThinkingLevel,
    /// Tool-call ids whose card disclosure (raw input + result) is expanded.
    expanded_tool_calls: HashSet<String>,
    /// Run-start entry indices of long tool-card runs the user has expanded
    /// (collapsed runs show first-3 + "N more" + last-2 otherwise).
    expanded_tool_runs: HashSet<usize>,
    /// Decoded thumbnails for user-attached images, keyed by (entry index, image
    /// index). Base64→decode happens once per attachment and is cached here so
    /// the transcript doesn't re-decode every streaming repaint. `RefCell` because
    /// `render` borrows the view immutably. Append-only entries keep the keys
    /// stable; `None` marks an image whose base64 failed to decode.
    image_cache: ImageCache,
    /// When `Some((entry, image))`, a full-size lightbox is open on the `image`-th
    /// attachment of the `entry`-th transcript entry. The ‹ › pager walks only
    /// that one message's images (a per-message group); the backdrop / ✕ clears
    /// it.
    preview: Option<(usize, usize)>,
    /// Foreground event-drain task. Dropping it only cancels the *foreground*
    /// half at its next await point — it does NOT stop the forwarder/reader OS
    /// threads or reap the subprocess. Subprocess + thread teardown is owned by
    /// `Drop::shutdown()` (which kills the child → stdout EOF → both threads
    /// unwind). Keep that the single cleanup owner across future refactors.
    _drain_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    /// Interactive AskUserQuestion cards for tool calls awaiting answers, keyed
    /// by tool-call id. Each is its own entity so its text inputs repaint without
    /// rebuilding the transcript; reconciled from the thread each render.
    question_cards: HashMap<String, Entity<QuestionCard>>,
    /// Event subscriptions for the cards above, kept alive alongside each card.
    question_card_subs: HashMap<String, Subscription>,
    /// Git checkpoint engine for this chat's `cwd`, or `None` when the dir isn't
    /// a git repo (or git is too old). Shared into background tasks via `Arc`.
    checkpoint_engine: Option<Arc<oximux_git::checkpoint::CheckpointEngine>>,
    /// The checkpoint taken when the CURRENT (in-flight) turn was sent, held so
    /// the turn-end compare can decide whether the rewind "files" affordance
    /// should light up. Cleared at each new send and after a rewind.
    pre_turn_checkpoint: Option<(usize, oximux_git::checkpoint::CheckpointSha)>,
    /// Open rewind-confirm card, rendered above the composer.
    rewind_confirm: Option<rewind_menu::RewindConfirm>,
    /// True while a rewind's background half (stop → fork → restore) runs; gates
    /// the composer and prevents overlapping rewinds.
    rewinding: bool,
    /// A message to send once the in-flight rewind lands (edit-and-resend). Set
    /// before the rewind starts, consumed on success, dropped on failure.
    rewind_then_send: Option<(String, Vec<ChatImage>)>,
    /// Active staged edit-and-resend, if any. Nothing is destroyed until send —
    /// Escape/cancel is a true no-op that restores the prior draft.
    pending_edit: Option<pending_edit::PendingEdit>,
    /// Open "Sessions" browser overlay (an inline child entity), toggled from
    /// the composer's history button. `None` when closed.
    session_picker: Option<Entity<session_picker::SessionPickerView>>,
    /// Subscription to the open session picker's events; dropped with it.
    session_picker_sub: Option<Subscription>,
}

impl AgentChatView {
    /// Construct a chat view and spawn its headless `claude` subprocess in
    /// `cwd`. A spawn failure degrades to a read-only error state rather than
    /// panicking, so the tab still opens and explains what went wrong.
    pub fn new(
        cwd: PathBuf,
        model: Option<String>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::assemble(cwd, model, ChatThread::new(), theme, density, typography, window, cx)
    }

    /// Rebuild a chat view on session restore: seed the thread from the
    /// persisted transcript and spawn the subprocess with `--resume
    /// <session_id>` (via [`ChatThread::rehydrated`]'s captured id) so the
    /// continued conversation keeps its context. The visible history paints
    /// immediately from `entries` — it does not wait on the resumed process.
    ///
    /// LIVE-VERIFY: `claude -p --resume` in stream-json mode is expected to load
    /// the session server-side and wait for input (not replay prior turns to
    /// stdout). If it *does* replay, the drain would append duplicate entries
    /// atop the rehydrated ones — watch for doubled bubbles on the first restore
    /// eyeball; the fix would be to drop the rehydrated seed and render purely
    /// from the replay.
    #[allow(clippy::too_many_arguments)]
    pub fn new_resumed(
        cwd: PathBuf,
        model: Option<String>,
        session_id: Option<String>,
        entries: Vec<ThreadEntry>,
        slash_commands: Vec<String>,
        thinking_level: ThinkingLevel,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread = ChatThread::rehydrated(session_id, model.clone(), entries, slash_commands);
        let mut view = Self::assemble(cwd, model, thread, theme, density, typography, window, cx);
        view.thinking_level = thinking_level;
        view
    }

    /// Shared construction for [`new`]/[`new_resumed`]: wire the composer, spawn
    /// the subprocess (resuming when `thread.session_id` is set), and start the
    /// event drain. A spawn failure degrades to a read-only error state so the
    /// tab still opens and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cwd: PathBuf,
        model: Option<String>,
        mut thread: ChatThread,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer =
            cx.new(|cx| ComposerView::new(theme, density, typography.clone(), window, cx));
        // The composer owns its input and repaints itself per keystroke. We only
        // react when it reports a finished submission — so typing never touches
        // this view (and thus never rebuilds the transcript, which is the lag we
        // want to avoid).
        let subscriptions = vec![cx.subscribe_in(
            &composer,
            window,
            |this, _composer, ev: &ComposerEvent, window, cx| match ev {
                ComposerEvent::Submit { text, images } => {
                    // A staged edit reroutes: rewind to the edited message, then
                    // send the edited text into the forked session.
                    if this.pending_edit.is_some() {
                        this.send_pending_edit(text.clone(), images.clone(), cx)
                    } else {
                        this.send_text(text.clone(), images.clone(), cx)
                    }
                }
                ComposerEvent::Stop => this.stop_turn(cx),
                ComposerEvent::ModelPicked(model) => this.change_model(model.clone(), cx),
                ComposerEvent::PermissionModePicked(mode) => {
                    this.change_permission_mode(mode.clone(), cx)
                }
                ComposerEvent::EffortPicked(effort) => this.change_effort(effort.clone(), cx),
                ComposerEvent::BrowseSessions => this.toggle_session_picker(window, cx),
            },
        )];

        // A resumed thread carries the prior session id; a fresh one is `None`
        // (spawn a new session). Either way the subprocess is spawned the same.
        let resume_session_id = thread.session_id.clone();
        let mut connection: Option<Box<dyn AgentConnection>> = None;
        let mut disconnected = false;
        let mut drain_task = None;
        // A fresh/restored session always starts in the default permission mode
        // (see the `permission_mode` field note); a live switch respawns.
        match ClaudeStreamJsonConnection::spawn_resumed(
            &cwd,
            model.as_deref(),
            resume_session_id.as_deref(),
            None,
            None,
        ) {
            Ok((conn, rx)) => {
                connection = Some(Box::new(conn));
                drain_task = Some(Self::spawn_drain(rx, cx));
            }
            Err(e) => {
                thread.last_error = Some(format!("Failed to start agent: {e}"));
                disconnected = true;
            }
        }

        // Seed the composer's bottom-toolbar pickers now, so they're correct on
        // the very first paint — a restored chat that isn't streaming fires no
        // event, so `sync_composer` wouldn't otherwise run until the next turn
        // (and the capability-gated pickers would be missing until then).
        // Permission mode + effort both start unset (the CLI defaults apply).
        let caps = connection
            .as_ref()
            .map(|c| c.capabilities())
            .unwrap_or_default();
        // Seed the palette from the rehydrated list so a restored chat offers it
        // on the first paint — `--resume` stays silent until the first message,
        // so no init would otherwise arrive to populate it.
        let seed_slash = if caps.supports_slash { thread.slash_commands.clone() } else { Vec::new() };
        // Seed ↑/↓ prompt history from the restored transcript's user prompts
        // (oldest→newest) so a resumed chat can recall what was already sent.
        let history_seed: Vec<String> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::User { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        composer.update(cx, |c, cx| {
            c.set_state(disconnected, thread.turn_active, cx);
            c.set_controls(model.clone(), None, None, caps.supports_modes, caps.supports_config, cx);
            c.set_slash_commands(seed_slash, cx);
            c.seed_history(history_seed);
        });

        // Enrich the palette with on-disk descriptions + grouping (the init list
        // is bare names). The scan reads ~100 small files, so it runs off the
        // main thread and pushes the result in when ready.
        if caps.supports_slash {
            let scan_cwd = cwd.clone();
            cx.spawn(async move |this, cx| {
                let catalog = cx
                    .background_spawn(async move {
                        slash_command_catalog::discover_catalog(&scan_cwd)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.composer.update(cx, |c, cx| c.set_command_catalog(catalog, cx));
                });
            })
            .detach();
        }

        // Resolve the git checkpoint engine for `cwd` off-thread (it shells out
        // to `git rev-parse`). Folds into `checkpoint_engine` when ready; a
        // non-repo cwd or old git leaves it `None` (rewind offers conversation
        // -only). Runs on the tokio runtime like the mention scan.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let engine_cwd = cwd.clone();
            let (tx, rx) =
                tokio::sync::oneshot::channel::<Option<oximux_git::checkpoint::CheckpointEngine>>();
            handle.spawn(async move {
                let engine = oximux_git::checkpoint::CheckpointEngine::new(&engine_cwd)
                    .await
                    .ok()
                    .flatten();
                let _ = tx.send(engine);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Some(engine)) = rx.await {
                    let _ = this.update(cx, |this, _cx| {
                        this.checkpoint_engine = Some(Arc::new(engine));
                    });
                }
            })
            .detach();
        }

        // Scan the project's files once for `@file` mention autocomplete. `rg`
        // runs on the tokio runtime (not gpui's executor), so hop through the
        // tokio handle like the terminal composer does, then fold the list back in
        // on the UI thread. Missing `rg` / no runtime degrades to an empty list.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let scan_root = cwd.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
            handle.spawn(async move {
                let files =
                    crate::shell::compose_bar::mention_resolver::scan_candidates(scan_root).await;
                let _ = tx.send(files);
            });
            cx.spawn(async move |this, cx| {
                if let Ok(files) = rx.await {
                    let _ = this.update(cx, |this, cx| {
                        this.composer.update(cx, |c, cx| c.set_mention_candidates(files, cx));
                    });
                }
            })
            .detach();
        }

        Self {
            thread,
            connection,
            composer,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            stick_to_bottom: true,
            // Kick the follow so a restored transcript (which loads at
            // construction, not via `on_event`) is pinned to the true bottom
            // once its async markdown layout settles.
            follow_frames: FOLLOW_FRAMES,
            last_max_offset: 0.0,
            theme,
            density,
            typography,
            cwd,
            model,
            permission_mode: None,
            effort: None,
            disconnected,
            interrupted: false,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: RefCell::new(HashMap::new()),
            preview: None,
            _drain_task: drain_task,
            _subscriptions: subscriptions,
            question_cards: HashMap::new(),
            question_card_subs: HashMap::new(),
            checkpoint_engine: None,
            pre_turn_checkpoint: None,
            rewind_confirm: None,
            rewinding: false,
            rewind_then_send: None,
            pending_edit: None,
            session_picker: None,
            session_picker_sub: None,
        }
    }

    /// Snapshot the transcript for persistence, or `None` when there's nothing
    /// worth restoring. A session id is required (it keys the blob and drives
    /// `--resume`); a chat with no completed turn has neither an id nor history,
    /// so it simply won't restore — the tab reopens fresh.
    pub fn transcript_snapshot(&self) -> Option<crate::persisted_chat::PersistedChatTranscript> {
        let session_id = self.thread.session_id.clone()?;
        if self.thread.entries.is_empty() {
            return None;
        }
        Some(crate::persisted_chat::PersistedChatTranscript {
            session_id,
            model: self.thread.model.clone().or_else(|| self.model.clone()),
            entries: self.thread.entries.clone(),
            slash_commands: self.thread.slash_commands.clone(),
            thinking_level: self.thinking_level,
        })
    }

    /// The chat's session id once Claude has minted one (after the first turn
    /// begins). Persisted in the tab's `PersistedTabKind::AgentChat` so restore
    /// can find the matching transcript blob and `--resume`.
    pub fn session_id(&self) -> Option<&str> {
        self.thread.session_id.as_deref()
    }

    /// Toggle the in-chat "Sessions" browser. Opening spawns a child picker
    /// entity scoped to this chat's project directory; choosing a session
    /// bubbles `AgentChatEvent::OpenSessionAsChat` up to the pane group.
    fn toggle_session_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_picker.is_some() {
            self.close_session_picker(window, cx);
            return;
        }
        // Scope discovery to this chat's project root. `cwd` is the launch dir;
        // worktree paths aren't tracked here, so a single-element scope is used
        // (the index still matches the project's slug dir).
        let scope_paths = vec![self.cwd.to_string_lossy().into_owned()];
        let fallback_cwd = self.cwd.clone();
        let (theme, typo) = (self.theme, self.typography.clone());
        let picker = cx.new(|cx| {
            session_picker::SessionPickerView::new(
                scope_paths, fallback_cwd, theme, typo, window, cx,
            )
        });
        let sub = cx.subscribe_in(
            &picker,
            window,
            |this, _picker, ev: &session_picker::SessionPickerEvent, window, cx| match ev {
                session_picker::SessionPickerEvent::Chosen { session_id, path, cwd } => {
                    this.close_session_picker(window, cx);
                    cx.emit(AgentChatEvent::OpenSessionAsChat {
                        session_id: session_id.clone(),
                        path: path.clone(),
                        cwd: cwd.clone(),
                    });
                }
                session_picker::SessionPickerEvent::Closed => {
                    this.close_session_picker(window, cx);
                }
            },
        );
        self.session_picker = Some(picker);
        self.session_picker_sub = Some(sub);
        cx.notify();
    }

    /// Close the session browser and return focus to the chat.
    fn close_session_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_picker.take().is_some() {
            self.session_picker_sub = None;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// FocusHandle of the inner composer — the pane focuses this on activate so
    /// keystrokes land in the draft without a click first.
    pub fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }

    /// Push the current connection/turn state + session controls into the
    /// composer so its status line, Send button, and bottom-toolbar pickers all
    /// reflect reality. Cheap no-op when nothing changed (both setters guard).
    fn sync_composer(&self, cx: &mut Context<Self>) {
        // A rewind in flight disables the composer just like a disconnect until
        // it resolves (respawn or error).
        let (disconnected, turn_active) =
            (self.disconnected || self.rewinding, self.thread.turn_active);
        // Advertise controls by capability, not by hard-coding the provider.
        let caps = self
            .connection
            .as_ref()
            .map(|c| c.capabilities())
            .unwrap_or_default();
        let (model, permission_mode, effort) =
            (self.model.clone(), self.permission_mode.clone(), self.effort.clone());
        // The command palette is offered only when the backend advertises
        // commands (Claude does; others send an empty list, which disables it).
        let slash_commands =
            if caps.supports_slash { self.thread.slash_commands.clone() } else { Vec::new() };
        self.composer.update(cx, |c, cx| {
            c.set_state(disconnected, turn_active, cx);
            c.set_controls(model, permission_mode, effort, caps.supports_modes, caps.supports_config, cx);
            c.set_slash_commands(slash_commands, cx);
        });
    }

    /// Stage image files dropped onto the chat surface into the composer. The
    /// read + decode runs on a background executor (an image can be large), then
    /// the staged attachments are handed to the composer on the foreground.
    fn attach_dropped_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let composer = self.composer.clone();
        cx.spawn(async move |_this, cx| {
            let staged = cx
                .background_spawn(async move {
                    paths
                        .iter()
                        .filter(|p| image_attach::is_image_path(p))
                        .filter_map(|p| image_attach::pending_from_path(p))
                        .collect::<Vec<_>>()
                })
                .await;
            composer.update(cx, |c, cx| c.add_pending_images(staged, cx));
        })
        .detach();
    }

    /// Record + transmit a submitted prompt (from the composer's Submit event).
    /// The composer has already cleared its own input.
    fn send_text(&mut self, text: String, images: Vec<ChatImage>, cx: &mut Context<Self>) {
        if text.is_empty() && images.is_empty() {
            return;
        }
        // A prior Stop killed the child but left the session resumable — bring it
        // back with `--resume` before sending so the conversation continues.
        if self.interrupted {
            self.respawn(cx);
        }
        if self.disconnected {
            return; // unrecoverable (a crash, or the resume failed) — nothing to send to
        }
        // Optimistically record the prompt; the reply streams in via `on_event`.
        self.thread.push_user_message_with_images(text.clone(), images.clone());
        // Snapshot the repo for this turn's rewind anchor (background — never
        // blocks the send). The user entry we just pushed is the last one.
        let user_index = self.thread.entries.len() - 1;
        self.take_checkpoint_for(user_index, cx);
        if let Some(conn) = &self.connection
            && let Err(e) = conn.send_user_message_with_images(&text, &images)
        {
            self.thread.last_error = Some(format!("Send failed: {e}"));
        }
        // Jump to (and re-arm following of) the bottom for the new turn.
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Interrupt the streaming turn (the composer's Stop button). SIGINTs the
    /// child, finalizes the transcript, and fail-closes any pending approval,
    /// then marks the session **resumable-idle**: the next send respawns via
    /// `--resume`. Not marked `disconnected` — the stop was intentional, so no
    /// error banner is shown.
    fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if !self.thread.turn_active {
            return; // nothing is streaming
        }
        if let Some(conn) = &self.connection {
            let _ = conn.cancel();
        }
        self.interrupted = true;
        self.thread.interrupt();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Reap the current child and spawn a fresh one resuming the same session
    /// (`--resume <session_id>`) with the current model + permission mode +
    /// effort, rewiring the event drain. The one place a live chat re-establishes
    /// its subprocess — shared by Stop→next-send and in-chat model / permission /
    /// effort switches (all fixed at spawn). Reads `self.model` /
    /// `self.permission_mode` / `self.effort`, so callers set those first.
    /// Degrades to a read-only error state if the respawn fails.
    fn respawn(&mut self, cx: &mut Context<Self>) {
        // Reap the old connection before replacing it — `Child`'s Drop neither
        // kills nor waits, so after a Stop this harvests the already-dead child
        // (and hard-kills it if somehow still alive).
        if let Some(old) = self.connection.take() {
            old.shutdown();
        }
        let session_id = self.thread.session_id.clone();
        let model = self.model.clone();
        let permission_mode = self.permission_mode.clone();
        let effort = self.effort.clone();
        match ClaudeStreamJsonConnection::spawn_resumed(
            &self.cwd,
            model.as_deref(),
            session_id.as_deref(),
            permission_mode.as_deref(),
            effort.as_deref(),
        ) {
            Ok((conn, rx)) => {
                self.connection = Some(Box::new(conn));
                // Reassigning drops the old drain task, cancelling its foreground
                // half; its forwarder thread then exits on the dead child's
                // stdout EOF. We're single-threaded here, so no stale
                // `on_disconnect` can interleave onto the fresh connection.
                self._drain_task = Some(Self::spawn_drain(rx, cx));
                self.interrupted = false;
                self.disconnected = false;
                self.thread.last_error = None;
            }
            Err(e) => {
                self.thread.last_error = Some(format!("Failed to resume agent: {e}"));
                self.disconnected = true;
                self.interrupted = false;
            }
        }
    }

    /// Switch the model for this chat tab. The CLI fixes `--model` at spawn, so
    /// a live switch reuses the resume path: kill the child and respawn it
    /// resumed on the new model (the conversation continues). The choice is
    /// raised as an event so the host persists it in the tab kind. No-op when
    /// the model is unchanged.
    fn change_model(&mut self, model: String, cx: &mut Context<Self>) {
        if self.model.as_deref() == Some(model.as_str()) {
            return;
        }
        self.model = Some(model.clone());
        self.thread.model = Some(model.clone());
        self.respawn(cx);
        self.sync_composer(cx); // reflect the new model in the toolbar label
        cx.emit(AgentChatEvent::ModelChanged(model));
        cx.notify();
    }

    /// Switch the permission mode for this chat tab. Like the model, `--permission
    /// -mode` is fixed at spawn, so a live switch respawns resumed on the new
    /// mode. Not persisted (see the field note), so no host event is raised.
    /// No-op when the mode is unchanged.
    fn change_permission_mode(&mut self, mode: String, cx: &mut Context<Self>) {
        let current = self.permission_mode.as_deref().unwrap_or(DEFAULT_PERMISSION_MODE);
        if current == mode {
            return;
        }
        // Normalize the baseline to `None` so `respawn` omits the flag entirely.
        self.permission_mode =
            (mode != DEFAULT_PERMISSION_MODE).then(|| mode.clone());
        self.respawn(cx);
        self.sync_composer(cx); // reflect the new mode in the toolbar label
        cx.notify();
    }

    /// Switch the reasoning effort for this chat tab. `--effort` is fixed at
    /// spawn (like `--model`), so a live switch respawns resumed on the new
    /// level. Not persisted, so no host event is raised. No-op when unchanged.
    fn change_effort(&mut self, effort: String, cx: &mut Context<Self>) {
        let current = self.effort.as_deref().unwrap_or(DEFAULT_EFFORT);
        if current == effort {
            return;
        }
        self.effort = Some(effort);
        self.respawn(cx);
        self.sync_composer(cx); // reflect the new effort in the toolbar label
        cx.notify();
    }

    /// Test-only constructor: inject a connection (a `StubConnection`) instead
    /// of spawning a real subprocess, and skip the background drain so a
    /// `#[gpui::test]` can drive `on_event`/`on_disconnect` synchronously.
    #[cfg(test)]
    fn with_connection_for_test(
        connection: Box<dyn AgentConnection>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer =
            cx.new(|cx| ComposerView::new(theme, density, typography.clone(), window, cx));
        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            composer,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            stick_to_bottom: true,
            // Kick the follow so a restored transcript (which loads at
            // construction, not via `on_event`) is pinned to the true bottom
            // once its async markdown layout settles.
            follow_frames: FOLLOW_FRAMES,
            last_max_offset: 0.0,
            theme,
            density,
            typography,
            cwd: PathBuf::new(),
            model: None,
            permission_mode: None,
            effort: None,
            disconnected: false,
            interrupted: false,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: RefCell::new(HashMap::new()),
            preview: None,
            _drain_task: None,
            _subscriptions: Vec::new(),
            question_cards: HashMap::new(),
            question_card_subs: HashMap::new(),
            checkpoint_engine: None,
            pre_turn_checkpoint: None,
            rewind_confirm: None,
            rewinding: false,
            rewind_then_send: None,
            pending_edit: None,
            session_picker: None,
            session_picker_sub: None,
        }
    }

    /// Bridge the connection's blocking `std::mpsc` receiver onto the
    /// foreground: a dedicated OS thread forwards each decoded event to an async
    /// channel a `cx.spawn` task awaits and applies. The forwarder exits when
    /// the process closes stdout, which ends the async channel and triggers the
    /// fail-closed disconnect handler.
    fn spawn_drain(
        rx: std::sync::mpsc::Receiver<ThreadEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let (fwd_tx, mut fwd_rx) = futures::channel::mpsc::unbounded::<ThreadEvent>();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                if fwd_tx.unbounded_send(ev).is_err() {
                    break; // view gone
                }
            }
            // `rx` disconnected (stdout EOF / process exit): `fwd_tx` drops here,
            // so the foreground task observes the channel end and fails closed.
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(ev) = fwd_rx.next().await {
                if this.update(cx, |view, cx| view.on_event(ev, cx)).is_err() {
                    return; // view dropped
                }
            }
            let _ = this.update(cx, |view, cx| view.on_disconnect(cx));
        })
    }

    /// Fold one decoded event into the thread and repaint.
    fn on_event(&mut self, ev: ThreadEvent, cx: &mut Context<Self>) {
        let was_active = self.thread.turn_active;
        self.thread.apply(&ev);
        // A user-initiated Stop makes `claude` end the turn with an
        // `error_during_execution` result (terminal_reason: aborted_streaming).
        // That's the expected shape of an interrupt, not a failure — swallow it
        // so an intentional Stop never flashes an error banner.
        if self.interrupted {
            self.thread.last_error = None;
        }
        // Following (and the actual `scroll_to_bottom`) is owned by `render` via
        // `stick_to_bottom`, so newly-arrived content — streamed text, a tall
        // tool card, an Allow/Reject row — stays glued as it settles. Arm a short
        // run of follow frames so the pin keeps re-asserting for a moment after
        // this event: the markdown lays out async, so its true height (and thus
        // the correct `content_size`) only lands a few frames later.
        if self.stick_to_bottom {
            self.follow_frames = FOLLOW_FRAMES;
        }
        // The turn's active flag may have flipped (e.g. `TurnEnded`); keep the
        // composer's status line in step.
        self.sync_composer(cx);
        cx.notify();
        // A turn just completed normally (active→idle edge) — release the next
        // message the user queued while it streamed, as a fresh turn. Skipped
        // after an intentional Stop (`interrupted`) or a dead process
        // (`disconnected`), where there is nothing live to send to; those leave
        // the queued chips in place, to drain on the next send or be cancelled.
        if was_active && !self.thread.turn_active && !self.interrupted && !self.disconnected {
            // A turn just completed — decide whether it changed repo state, so
            // the rewind "restore files" affordance only lights up when there's
            // something to restore. Background compare against the pre-turn sha.
            self.compare_turn_checkpoint(cx);
            self.flush_next_queued(cx);
        }
    }

    /// Take a checkpoint anchored to the user entry at `user_index`, off-thread.
    /// Attaches the sha to that entry when done (a no-op if the thread moved on)
    /// and records it as the pre-turn snapshot for the turn-end compare.
    fn take_checkpoint_for(&mut self, user_index: usize, cx: &mut Context<Self>) {
        let Some(engine) = self.checkpoint_engine.clone() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        // Bind the snapshot to the session it was taken for. A rewind mints a
        // new session id and renumbers entries, so a straggling callback from a
        // pre-rewind turn must not misattach onto a same-index entry.
        let session = self.thread.session_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.spawn(async move {
            let _ = tx.send(engine.create().await.ok());
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(sha)) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.thread.session_id != session {
                        return; // stale — the session was rewound out from under us
                    }
                    this.thread.attach_checkpoint(user_index, sha.0.clone());
                    this.pre_turn_checkpoint =
                        Some((user_index, oximux_git::checkpoint::CheckpointSha(sha.0)));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// After a turn ends, compare a fresh snapshot against the pre-turn one; if
    /// they differ, light up the rewind "restore files" affordance on that
    /// turn's user entry. The fresh snapshot is only used for the compare.
    fn compare_turn_checkpoint(&mut self, cx: &mut Context<Self>) {
        let (Some(engine), Some((index, pre_sha))) =
            (self.checkpoint_engine.clone(), self.pre_turn_checkpoint.take())
        else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let session = self.thread.session_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        handle.spawn(async move {
            let changed = match engine.create().await {
                Ok(post) => engine.differs(&pre_sha, &post).await.unwrap_or(false),
                Err(_) => false,
            };
            let _ = tx.send(changed);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(changed) = rx.await
                && changed
            {
                let _ = this.update(cx, |this, cx| {
                    if this.thread.session_id != session {
                        return; // stale — session rewound before the compare landed
                    }
                    this.thread.set_checkpoint_show(index, true);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Send the oldest message the composer parked while a turn streamed, if any.
    /// Driven off the natural turn-end edge in [`Self::on_event`]; since each
    /// completed turn releases exactly one, a lined-up batch drains in order.
    fn flush_next_queued(&mut self, cx: &mut Context<Self>) {
        if let Some((text, images)) = self.composer.update(cx, |c, cx| c.take_next_queued(cx)) {
            self.send_text(text, images, cx);
        }
    }

    /// Whether the transcript is scrolled to (within one card of) the bottom.
    /// `offset().y` is `<= 0` and reaches `-max_offset().y` at the very bottom,
    /// so their sum is the remaining scroll distance. Fresh views (no paint yet)
    /// report `0`, i.e. "at bottom", so the first turn follows.
    fn is_near_bottom(&self) -> bool {
        let sh = &self.list_scroll;
        sh.max_offset().y + sh.offset().y <= px(160.0)
    }

    /// The event channel closed — the agent process exited or its stdout was
    /// closed. Fail closed: if a permission was still pending, reject it (the
    /// tool never ran, since the process is gone). Best-effort deny in case
    /// stdin is briefly still writable, then mark the tool `Rejected` so the UI
    /// never shows a dangling approval prompt.
    fn on_disconnect(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .thread
            .pending_permission()
            .map(|(tool_id, req)| (tool_id.to_string(), req.request_id.clone()));
        if let Some((tool_id, request_id)) = pending {
            if let Some(conn) = &self.connection {
                let _ = conn.resolve_permission(
                    &request_id,
                    PermissionDecision::Deny { message: "agent disconnected".into() },
                );
            }
            self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
        }
        // Fail-close a pending AskUserQuestion too: the process is gone, so it can
        // never be answered — reject it and drop its card rather than stranding an
        // unanswerable prompt.
        if let Some(tool_id) = self.thread.pending_question().map(|(id, _)| id.to_string()) {
            self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
            self.question_cards.remove(&tool_id);
            self.question_card_subs.remove(&tool_id);
        }
        self.thread.turn_active = false;
        if self.interrupted {
            // Intentional Stop: the child exited exactly as asked. Stay
            // resumable-idle (the next send respawns via `--resume`) instead of
            // marking the tab unavailable.
            self.thread.last_error = None;
            self.sync_composer(cx);
            cx.notify();
            return;
        }
        self.disconnected = true;
        if self.thread.last_error.is_none() {
            self.thread.last_error = Some("Agent process exited.".into());
        }
        self.sync_composer(cx);
        cx.notify();
    }

    /// Whether entry `idx`'s thinking block renders expanded, resolving the
    /// chat-wide level against the user's per-entry expand/collapse overrides.
    /// In `Auto`, the streaming thought (last entry, turn active, no text yet)
    /// auto-expands UNLESS the user explicitly collapsed it.
    fn thinking_expanded(&self, idx: usize, is_last: bool, msg: &AssistantMessage) -> bool {
        match self.thinking_level {
            ThinkingLevel::Hidden => false,
            ThinkingLevel::Expanded => true,
            ThinkingLevel::Auto => {
                if self.collapsed_thinking.contains(&idx) {
                    false
                } else {
                    self.expanded_thinking.contains(&idx)
                        || (is_last && self.thread.turn_active && msg.text.is_empty())
                }
            }
        }
    }

    /// Toggle a thinking block: compute its current resolved state and flip it
    /// explicitly (so a manual collapse wins over Auto's stream auto-expand on
    /// the first click, and vice-versa).
    fn toggle_thinking(&mut self, idx: usize, cx: &mut Context<Self>) {
        let is_last = idx + 1 == self.thread.entries.len();
        let currently = match self.thread.entries.get(idx) {
            Some(ThreadEntry::Assistant(msg)) => self.thinking_expanded(idx, is_last, msg),
            _ => self.expanded_thinking.contains(&idx),
        };
        if currently {
            self.expanded_thinking.remove(&idx);
            self.collapsed_thinking.insert(idx);
        } else {
            self.collapsed_thinking.remove(&idx);
            self.expanded_thinking.insert(idx);
        }
        cx.notify();
    }

    /// Expand a collapsed tool run (its "N more" click).
    fn expand_tool_run(&mut self, run_start: usize, cx: &mut Context<Self>) {
        self.expanded_tool_runs.insert(run_start);
        cx.notify();
    }

    /// The "… N more tool calls" expander for a collapsed run.
    fn render_tool_run_expander(
        &self,
        run_start: usize,
        hidden: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        div()
            .id(("tool-run-expander", run_start))
            .flex()
            .items_center()
            .gap(px(6.0))
            .w_full()
            .py(px(2.0))
            .text_xs()
            .text_color(t.fg_subtle)
            .cursor_pointer()
            .hover(|s| s.text_color(t.fg_base))
            .child(SharedString::from(format!("··· {hidden} more tool calls")))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _e, _w, cx| this.expand_tool_run(run_start, cx)),
            )
            .into_any_element()
    }

    /// Cycle the chat-wide thinking level (Hidden → Auto → Expanded → …), from
    /// the pill above the composer. Persisted via `transcript_snapshot`.
    fn cycle_thinking_level(&mut self, cx: &mut Context<Self>) {
        self.thinking_level = self.thinking_level.next();
        cx.notify();
    }

    /// Count tool calls still awaiting the user (permission or question).
    fn awaiting_count(&self) -> usize {
        self.thread
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ThreadEntry::ToolCall(tc)
                        if matches!(
                            tc.status,
                            ToolCallStatus::WaitingForConfirmation(_)
                                | ToolCallStatus::AwaitingAnswer(_)
                        )
                )
            })
            .count()
    }

    /// A pinned "awaiting your approval — Jump" banner, shown only when there IS
    /// a pending card AND the user has scrolled up away from it (near-bottom is
    /// treated as "the card is visible"). Conservative by design: index-based,
    /// no per-entry pixel math (which fights the async markdown layout).
    fn render_awaiting_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let n = self.awaiting_count();
        if n == 0 || self.is_near_bottom() {
            return None;
        }
        let t = self.theme;
        Some(
            div()
                .id("awaiting-approval-banner")
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .max_w(px(CONTENT_MAX_W))
                .px(px(10.0))
                .py(px(5.0))
                .rounded(px(8.0))
                .bg(t.status_warn.opacity(0.15))
                .text_sm()
                .text_color(t.fg_base)
                .cursor_pointer()
                .hover(|s| s.bg(t.status_warn.opacity(0.22)))
                .child(SharedString::from(format!(
                    "Awaiting your approval ({n})"
                )))
                .child(div().text_xs().text_color(t.fg_muted).child("Jump ↓"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| {
                        this.stick_to_bottom = true;
                        this.follow_frames = FOLLOW_FRAMES;
                        this.list_scroll.scroll_to_bottom();
                        cx.notify();
                    }),
                ),
        )
    }

    /// The compact thinking-level pill shown above the composer.
    fn render_thinking_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .flex()
            .justify_end()
            .w_full()
            .max_w(px(CONTENT_MAX_W))
            .child(
                div()
                    .id("thinking-level-toggle")
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .text_xs()
                    .text_color(t.fg_subtle)
                    .cursor_pointer()
                    .hover(|s| s.text_color(t.fg_base).bg(t.bg_panel_alt))
                    .child(SharedString::from(self.thinking_level.label()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| this.cycle_thinking_level(cx)),
                    ),
            )
    }

    fn toggle_tool_expanded(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.expanded_tool_calls.insert(id.clone()) {
            self.expanded_tool_calls.remove(&id);
        }
        cx.notify();
    }

    /// Answer a pending tool permission from a card button. Routes the decision
    /// to the connection by `request_id`, then transitions the local status so
    /// the card updates immediately: Allow → `InProgress` (the tool proceeds and
    /// the later `ToolResult` finalizes it); Deny → `Rejected`.
    fn resolve_permission(
        &mut self,
        tool_id: String,
        request_id: String,
        decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        // Idempotency guard: only answer a tool that is STILL awaiting. Once
        // answered its status leaves `WaitingForConfirmation` (below) and the
        // buttons drop on re-render, but this closes the sub-frame window where
        // a stray second click could send a second control_response for an
        // already-decided request_id.
        let still_awaiting = self.thread.entries.iter().any(|e| {
            matches!(e, ThreadEntry::ToolCall(tc)
                if tc.id == tool_id
                    && matches!(&tc.status,
                        ToolCallStatus::WaitingForConfirmation(r) if r.request_id == request_id))
        });
        if !still_awaiting {
            return;
        }
        if let Some(conn) = &self.connection {
            let _ = conn.resolve_permission(&request_id, decision.clone());
        }
        let status = match &decision {
            PermissionDecision::Deny { .. } => ToolCallStatus::Rejected,
            PermissionDecision::Allow { .. } | PermissionDecision::AllowWithSuggestion { .. } => {
                ToolCallStatus::InProgress
            }
        };
        self.thread.set_tool_status(&tool_id, status);
        cx.notify();
    }

    /// Create/drop the interactive question-card entities to match the thread's
    /// `AwaitingAnswer` tool calls. Runs each render (which owns `window`, needed
    /// to build the cards' text inputs) and is idempotent once a card exists.
    fn reconcile_question_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let live: Vec<(String, QuestionRequest)> = self
            .thread
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::ToolCall(tc) => match &tc.status {
                    ToolCallStatus::AwaitingAnswer(req) => Some((tc.id.clone(), req.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        // Drop cards whose tool is no longer awaiting an answer (answered,
        // rejected on disconnect/interrupt, etc.).
        let live_ids: HashSet<&String> = live.iter().map(|(id, _)| id).collect();
        self.question_cards.retain(|id, _| live_ids.contains(id));
        self.question_card_subs.retain(|id, _| live_ids.contains(id));
        // Create any missing cards, wiring each card's Submit/Skip to the answer.
        let (theme, density, typo) = (self.theme, self.density, self.typography.clone());
        for (tool_id, req) in live {
            if self.question_cards.contains_key(&tool_id) {
                continue;
            }
            let card = cx.new(|cx| {
                QuestionCard::new(tool_id.clone(), req, theme, density, typo.clone(), window, cx)
            });
            let sub = cx.subscribe(&card, |this, _card, ev: &QuestionCardEvent, cx| match ev {
                QuestionCardEvent::Submit { tool_id, answers } => {
                    this.answer_question(tool_id.clone(), answers.clone(), cx)
                }
                QuestionCardEvent::Skip { tool_id } => {
                    this.answer_question(tool_id.clone(), QuestionAnswers::default(), cx)
                }
            });
            self.question_cards.insert(tool_id.clone(), card);
            self.question_card_subs.insert(tool_id, sub);
        }
    }

    /// Answer a pending `AskUserQuestion` by tool id: look up its request +
    /// questions from the thread, send the selections back, and settle the tool
    /// locally so the card drops immediately (the CLI's `tool_result` finalizes
    /// the row to `Completed` right after). Empty `answers` = Skip — a plain
    /// allow the CLI reads as "did not answer".
    fn answer_question(
        &mut self,
        tool_id: String,
        answers: QuestionAnswers,
        cx: &mut Context<Self>,
    ) {
        // Idempotency: only answer a tool STILL awaiting (guards a stray second
        // Submit racing the re-render that drops the card).
        let found = self.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == tool_id => match &tc.status {
                ToolCallStatus::AwaitingAnswer(req) => {
                    Some((req.request_id.clone(), req.questions.clone()))
                }
                _ => None,
            },
            _ => None,
        });
        let Some((request_id, questions)) = found else {
            return;
        };
        if let Some(conn) = &self.connection {
            let _ = conn.answer_question(&request_id, &questions, &answers);
        }
        self.thread.set_tool_status(&tool_id, ToolCallStatus::InProgress);
        self.question_cards.remove(&tool_id);
        self.question_card_subs.remove(&tool_id);
        cx.notify();
    }

    /// Decoded thumbnails for a user entry's attached images, memoized in
    /// [`Self::image_cache`] by the stable (entry, image) position so a streaming
    /// repaint never re-decodes base64. Corrupt attachments are skipped.
    fn decoded_images(&self, idx: usize, images: &[ChatImage]) -> Vec<Arc<Image>> {
        let mut cache = self.image_cache.borrow_mut();
        let mut out = Vec::with_capacity(images.len());
        for (i, chat) in images.iter().enumerate() {
            let decoded =
                cache.entry((idx, i)).or_insert_with(|| image_attach::decode_render(chat));
            if let Some(arc) = decoded {
                out.push(arc.clone());
            }
        }
        out
    }

    /// A user prompt: its attached-image thumbnails (each clickable to open the
    /// full-size lightbox) stacked above the right-aligned text bubble.
    fn render_user_entry(
        &self,
        idx: usize,
        text: &str,
        images: &[ChatImage],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let decoded = self.decoded_images(idx, images);
        let mut col = div().flex().flex_col().items_end().w_full().gap(px(6.0));
        if !decoded.is_empty() {
            let mut thumbs = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .justify_end()
                .gap(px(6.0))
                .max_w(px(bubble::USER_IMAGES_MAX_W));
            for (i, im) in decoded.iter().enumerate() {
                thumbs = thumbs.child(
                    div()
                        .id(SharedString::from(format!("user-img-{idx}-{i}")))
                        .w(px(200.0))
                        .h(px(150.0))
                        .flex_none()
                        .rounded(px(density.r_card))
                        .overflow_hidden()
                        .border_1()
                        .border_color(theme.border_inactive)
                        .cursor_pointer()
                        .hover(|s| s.border_color(theme.focus_ring))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| {
                                this.open_image_preview(idx, i, cx)
                            }),
                        )
                        .child(
                            img(ImageSource::Image(im.clone()))
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        ),
                );
            }
            col = col.child(thumbs);
        }
        if !text.is_empty() {
            col = col.child(bubble::user_body(text, theme, density, &typo));
        }
        // A ↺ Rewind affordance appears on hover once this turn has a session to
        // fork (session id present) and we're not mid-rewind. It's shown for any
        // prior user message; the confirm card gates the files axis on whether
        // the checkpoint actually captured a change.
        let can_rewind = self.thread.session_id.is_some() && !self.rewinding;
        if can_rewind {
            let group = SharedString::from(format!("user-entry-{idx}"));
            col = div()
                .group(group.clone())
                .flex()
                .flex_col()
                .items_end()
                .w_full()
                .gap(px(6.0))
                .child(col)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        // Edit is offered only when the turn is idle (a live turn
                        // would queue the resend instead of routing it) — Rewind,
                        // which cancels the turn first, stays available.
                        .when(!self.thread.turn_active, |row| {
                            row.child(message_action_chip(
                                SharedString::from(format!("edit-btn-{idx}")),
                                "✎",
                                "Edit",
                                theme,
                                cx.listener(move |this, _e, window, cx| {
                                    this.enter_pending_edit(idx, window, cx);
                                }),
                            ))
                        })
                        .child(message_action_chip(
                            SharedString::from(format!("rewind-btn-{idx}")),
                            "↺",
                            "Rewind",
                            theme,
                            cx.listener(move |this, _e, _w, cx| {
                                this.open_rewind_confirm(idx, cx)
                            }),
                        )),
                );
        }
        col.into_any_element()
    }

    /// The decoded images of one transcript entry (empty for a non-user entry or
    /// a stale index) — the group the lightbox pager walks.
    fn entry_images(&self, entry_idx: usize) -> Vec<Arc<Image>> {
        match self.thread.entries.get(entry_idx) {
            Some(ThreadEntry::User { images, .. }) if !images.is_empty() => {
                self.decoded_images(entry_idx, images)
            }
            _ => Vec::new(),
        }
    }

    /// Open the full-size lightbox on one message's image.
    fn open_image_preview(&mut self, entry_idx: usize, img_idx: usize, cx: &mut Context<Self>) {
        self.preview = Some((entry_idx, img_idx));
        cx.notify();
    }

    /// Dismiss the lightbox (backdrop click or the ✕).
    fn close_image_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    /// Step within the CURRENT message's image group, wrapping at the ends.
    fn step_image_preview(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some((entry, img)) = self.preview {
            let n = self.entry_images(entry).len();
            if n == 0 {
                return;
            }
            let next = (img as isize + delta).rem_euclid(n as isize) as usize;
            self.preview = Some((entry, next));
            cx.notify();
        }
    }

    /// The full-size image lightbox: a dimmed backdrop (click to dismiss) with
    /// the current image fit (aspect-preserved), a ‹ N of M › pager across the
    /// SAME message's images, and a ✕ — Claude-Desktop-style. Rendered over
    /// everything when [`Self::preview`] is set.
    fn render_image_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (entry, img_idx) = self.preview?;
        let group = self.entry_images(entry);
        let i = img_idx.min(group.len().saturating_sub(1));
        let image = group.get(i)?.clone();
        let n = group.len();
        let theme = self.theme;
        let typo = &self.typography;

        // A circular control glyph used for the ✕ and the ‹ › arrows.
        let control = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .size(px(30.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_input)
                .text_color(theme.fg_muted)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.fg_base))
                .child(SharedString::from(glyph))
        };

        // The image box is sized RELATIVE TO THE BACKDROP (a definite full-window
        // box), NOT a shrink-wrapped column — otherwise `relative(..)` resolves
        // against an auto-sized parent and collapses to zero (blank image). It's
        // a direct child of the backdrop for that reason. Clicking the image
        // itself is swallowed so only the dark margin (or ✕) dismisses.
        let image_box = div()
            .w(relative(0.86))
            .h(relative(0.78))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
            .child(
                img(ImageSource::Image(image))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );

        let pager = (n > 1).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(16.0))
                // Clicks on the pager (label / gaps) shouldn't close either.
                .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                .child(control("chat-image-prev", "‹").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.step_image_preview(-1, cx)),
                ))
                .child(
                    div()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(format!("{} of {}", i + 1, n))),
                )
                .child(control("chat-image-next", "›").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.step_image_preview(1, cx)),
                ))
        });

        Some(
            div()
                .id("chat-image-preview")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .bg(gpui::black().opacity(0.82))
                // A click on the bare (dark) backdrop closes the lightbox.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.close_image_preview(cx)),
                )
                .child(image_box)
                .children(pager)
                // ✕ sits on the backdrop, so clicking it bubbles to the close
                // handler.
                .child(
                    control("chat-image-preview-close", "✕")
                        .absolute()
                        .top(px(16.0))
                        .right(px(16.0)),
                )
                .into_any_element(),
        )
    }

    /// The scrollable transcript column. Entries stack in a centered reading
    /// column ([`CONTENT_MAX_W`]) so wide windows don't stretch text edge-to-
    /// edge; the outer element only scrolls and centers.
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let scroll = div()
            .id("agent-chat-list")
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .flex_1()
            // `min_h(0)` is essential: a flex child defaults to `min-height:auto`
            // (= content height), so without this the transcript grows to its
            // content size instead of shrinking to the flex-allocated space —
            // its scroll box then extends past the composer and the true bottom
            // (the newest message / approval row) is never reachable, no matter
            // the scroll offset. Pinning min-height to 0 lets it shrink so
            // `overflow_y_scroll` actually bounds the box to the visible area.
            .min_h(px(0.0))
            .px(px(density.pad_panel))
            .py(px(density.pad_panel))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            // Release auto-follow when the user scrolls UP to read history (so a
            // streaming turn doesn't yank them back down); re-arm once they
            // return to the bottom. gpui's scroll offset grows more negative as
            // you scroll down, so a positive wheel delta means "toward the top".
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _window, cx| {
                let dy = ev.delta.pixel_delta(px(20.0)).y;
                let was = this.stick_to_bottom;
                if dy > px(0.0) {
                    this.stick_to_bottom = false;
                } else if this.is_near_bottom() {
                    this.stick_to_bottom = true;
                }
                if this.stick_to_bottom != was {
                    cx.notify();
                }
            }));

        if self.thread.entries.is_empty() {
            // Even the empty state rides the scroll box + overlay so the layout
            // is identical once messages arrive; the scrollbar auto-hides when
            // content fits.
            return self
                .wrap_scroll(scroll.child(self.render_empty_hint(&theme, &typo)))
                .into_any_element();
        }

        // Turns breathe a little more than inline content — a chat rhythm.
        let mut content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(CONTENT_MAX_W))
            .gap(px(density.pad_panel * 2.0));

        // Group long runs of tool cards: a run of >8 collapses to first-3 +
        // "N more" + last-2, with pending/failed cards always kept visible.
        let is_tool: Vec<bool> = self
            .thread
            .entries
            .iter()
            .map(|e| matches!(e, ThreadEntry::ToolCall(_)))
            .collect();
        let force_show: Vec<bool> = self
            .thread
            .entries
            .iter()
            .map(|e| {
                matches!(
                    e,
                    ThreadEntry::ToolCall(tc)
                        if matches!(
                            tc.status,
                            ToolCallStatus::WaitingForConfirmation(_)
                                | ToolCallStatus::AwaitingAnswer(_)
                                | ToolCallStatus::Failed(_)
                                | ToolCallStatus::Pending
                                | ToolCallStatus::InProgress
                        )
                )
            })
            .collect();
        let group_plan = plan_tool_grouping(&is_tool, &force_show, &self.expanded_tool_runs);

        for (idx, entry) in self.thread.entries.iter().enumerate() {
            if matches!(group_plan[idx], EntryDisplay::Hide) {
                continue;
            }
            let el: Option<AnyElement> = match entry {
                ThreadEntry::User { text, images, .. } => {
                    // No "You" caption — the right-aligned bubble is the signal.
                    Some(self.render_user_entry(idx, text, images, cx))
                }
                ThreadEntry::Assistant(msg) => {
                    if msg.is_empty() {
                        None
                    } else {
                        let group = SharedString::from(format!("chat-asst-{idx}"));
                        let mut block = div()
                            .group(group.clone())
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .w_full()
                            .child(assistant_header(group, &msg.text, theme, &typo, cx));
                        // Thinking display honors the chat-wide level (see
                        // `thinking_expanded`): Hidden drops the block; Expanded
                        // forces it open; Auto peeks the streaming thought and
                        // otherwise respects the user's per-entry toggle.
                        if !msg.thinking.is_empty() && self.thinking_level != ThinkingLevel::Hidden {
                            let is_last = idx + 1 == self.thread.entries.len();
                            let expanded = self.thinking_expanded(idx, is_last, msg);
                            block = block.child(thinking_block(
                                idx, expanded, &msg.thinking, theme, density, &typo, cx,
                            ));
                        }
                        if !msg.text.is_empty() {
                            block = block.child(bubble::assistant_body(idx, &msg.text, &typo));
                        }
                        Some(block.into_any_element())
                    }
                }
                ThreadEntry::ToolCall(tc) => {
                    // An AskUserQuestion awaiting answers renders as the dedicated
                    // interactive question card (reconciled into `question_cards`
                    // before this loop); a TodoWrite as a read-only plan checklist;
                    // every other tool call uses the generic (expandable) card.
                    if matches!(tc.status, ToolCallStatus::AwaitingAnswer(_)) {
                        self.question_cards.get(&tc.id).map(|c| c.clone().into_any_element())
                    } else if question_card::is_question(tc) {
                        // Answered/skipped question → a compact one-line summary.
                        Some(question_card::render_settled(tc, theme, density, &typo)
                            .into_any_element())
                    } else if plan_panel::is_plan(tc) {
                        Some(plan_panel::render_plan_card(tc, theme, density, &typo)
                            .into_any_element())
                    } else {
                        let expanded = self.expanded_tool_calls.contains(&tc.id);
                        Some(tool_card::render_tool_card(tc, expanded, theme, density, &typo, cx)
                            .into_any_element())
                    }
                }
                ThreadEntry::ContextCompaction { summary } => {
                    Some(compaction_divider(summary, theme, &typo).into_any_element())
                }
            };
            if let Some(el) = el {
                // A staged edit dims the messages it will remove on send.
                if self.is_pending_edit_dimmed(idx) {
                    content = content.child(div().w_full().opacity(0.4).child(el));
                } else {
                    content = content.child(el);
                }
            }
            // A collapsed tool-run expander follows its anchor entry.
            if let EntryDisplay::ShowThenExpander { run_start, hidden } = group_plan[idx] {
                content = content.child(self.render_tool_run_expander(run_start, hidden, cx));
            }
        }
        // Live turn / disconnect state lives at the tail of the transcript (like
        // a native chat), NOT above the composer — so it never resizes the input.
        if self.disconnected {
            content = content.child(
                div()
                    .w_full()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("Agent process exited.")),
            );
        } else if self.thread.turn_active {
            // While a question card is pending, the agent isn't working — it's
            // blocked on the user's answer — so don't show the "working…" spinner
            // (it would also add height that pushes the card's controls down).
            if self.thread.pending_question().is_none() {
                content = content.child(working_indicator(theme, &typo));
            }
        } else {
            // A settled turn: surface its one-line summary and token/cost usage
            // (both decoded by the backend; shown only when present).
            if let Some(summary) = self
                .thread
                .last_summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                content = content.child(summary_line(summary, theme, &typo));
            }
            if let Some(usage) = self.thread.usage.as_ref() {
                content = content.child(usage_footer(usage, theme, &typo));
            }
        }
        // Trailing clearance INSIDE the scrollable content, above the composer.
        // `scroll_to_bottom` pins the offset to gpui's `scroll_max`, derived from
        // the content height sampled at layout. The catch: gpui-component's
        // markdown reports a height that counts only its FIRST block — the rest of
        // a multi-paragraph reply paints correctly but falls OUTSIDE the measured
        // `content_size` (only the last message is affected; earlier ones sit
        // above enough content to stay in range). The scroll box clips at its own
        // viewport, not at `content_size`, so the fix is to add enough real
        // scrollable room below the last message that scroll-to-bottom can bring
        // the whole reply — overflow and all — up into the viewport. Size that
        // room to the last reply's under-counted tail so short replies keep a
        // tight bottom margin while long ones are fully reachable.
        let tail_gap = if self.thread.pending_question().is_some() {
            px(160.0)
        } else {
            // `base` is the breathing margin below the last line; `reveal` is the
            // extra scroll room a multi-block reply needs to bring its under-
            // counted tail into view. Stack them so the reveal room is always
            // topped with a consistent margin (the reveal estimate alone can land
            // flush against the composer), while keeping the estimate un-inflated
            // so the bottom gap stays modest.
            let base = density.pad_panel * 4.0;
            let reveal = self
                .thread
                .entries
                .iter()
                .rev()
                .find_map(|e| match e {
                    ThreadEntry::Assistant(m) if !m.text.is_empty() => Some(m.text.as_str()),
                    _ => None,
                })
                .map(|t| markdown_reveal_gap(t, &typo))
                .unwrap_or(0.0);
            px(base + reveal)
        };
        content = content.child(div().flex_none().w_full().h(tail_gap));
        self.wrap_scroll(scroll.child(content)).into_any_element()
    }

    /// Wrap the scrolling transcript box in a positioned container and overlay a
    /// fading scrollbar bound to the SAME [`ScrollHandle`]. The bar paints on the
    /// container's right edge, auto-hides when the content fits, and — being a
    /// `Normal` hitbox gated to its own 16px strip — never blocks clicks on the
    /// messages, tool cards, or Allow/Reject rows beneath it.
    fn wrap_scroll(&self, scroll_box: impl IntoElement) -> gpui::Div {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(scroll_box)
            .child(Scrollbar::vertical(&self.list_scroll))
    }

    fn render_empty_hint(&self, theme: &Theme, typo: &Typography) -> AnyElement {
        // Disconnected → surface the error plainly. Otherwise a calm, centered
        // greeting (title + hint) rather than a lone sentence.
        let (title, subtitle, title_color) = if self.disconnected {
            (
                "Agent unavailable",
                self.thread.last_error.as_deref().unwrap_or("The agent process exited."),
                theme.status_error,
            )
        } else {
            (
                "Start a conversation",
                "Ask Claude to explain code, make edits, or run commands.",
                theme.fg_muted,
            )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .w_full()
            .child(
                div()
                    .text_size(px(typo.t_body_lg))
                    .text_color(title_color)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(subtitle.to_string())),
            )
            .into_any_element()
    }

}

impl Drop for AgentChatView {
    fn drop(&mut self) {
        // Kill + reap the `claude` child so closing the tab doesn't leak it.
        if let Some(conn) = &self.connection {
            conn.shutdown();
        }
    }
}

impl EventEmitter<AgentChatEvent> for AgentChatView {}

impl Focusable for AgentChatView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // Create/drop the interactive question cards to match the thread before
        // the (immutable) transcript render reads them. Needs `window` for the
        // cards' text inputs, so it lives here rather than in `render_transcript`.
        self.reconcile_question_cards(window, cx);
        // Keyboard focus must live on the composer, not this view's root. The
        // pane focuses the composer on open, but an inline focus during action/
        // click dispatch is clobbered onto the root's tracked handle — so
        // keystrokes hit the root, the composer stays empty, and ⌘↵ never
        // dispatches the field's Enter action. If the root holds focus, hand it
        // to the composer (deferred so it wins the post-dispatch focus race).
        // Self-limiting: once the composer is focused the root no longer is.
        if self.focus_handle.is_focused(window) {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.read(cx).focus_handle(cx).focus(window, cx);
            });
        }
        // Re-pin to the bottom every frame while following. Re-asserting each
        // render (not just once per event) keeps the newest row glued as its
        // height settles a frame after it arrives (markdown/diff measuring) and
        // through the end of the turn — a single per-event scroll lands short in
        // that case. Released when the user scrolls up (see the wheel handler on
        // the transcript). `scroll_to_bottom` only sets a flag consumed at paint,
        // so this is cheap.
        if self.stick_to_bottom {
            self.list_scroll.scroll_to_bottom();
            // The async markdown layout that follows a content change does not
            // re-run this render, so one pin lands on a too-short `content_size`.
            // Keep re-arming the follow while the scrollable extent is still
            // growing (the layout settling), so a slow/large reply is followed to
            // its true bottom; once it holds steady the counter drains and the
            // frame loop stops. Each armed frame forces a re-render that re-pins
            // to the freshly-settled height.
            let max_y = f32::from(self.list_scroll.max_offset().y);
            if (max_y - self.last_max_offset).abs() > 0.5 {
                self.follow_frames = FOLLOW_FRAMES;
            }
            self.last_max_offset = max_y;
            if self.follow_frames > 0 {
                self.follow_frames -= 1;
                let this = cx.entity().downgrade();
                window.on_next_frame(move |_window, cx| {
                    let _ = this.update(cx, |_this, cx| cx.notify());
                });
            }
        }
        let transcript = self.render_transcript(cx);
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_panel)
            // Escape is bound app-wide to `DismissOverlay` (not the input's own
            // Escape action), so a staged-edit cancel must hook THAT. This
            // `on_action` fires on bubble before the workspace root's handler;
            // consume it only when we actually had a staged edit to cancel, so
            // a normal Escape still dismisses other overlays.
            .on_action(cx.listener(|this, _: &crate::actions::DismissOverlay, window, cx| {
                if this.pending_edit.is_some() {
                    this.cancel_pending_edit(window, cx);
                    cx.stop_propagation();
                }
            }))
            // The Input context binds BOTH `enter` and `shift+enter` to the same
            // Enter{secondary:false} action, so the action alone can't tell them
            // apart — read the live shift modifier. Capture here (the field would
            // otherwise consume Enter before any `on_key_down`): a plain ↵
            // submits, ⇧↵ falls through to the multi-line field as a newline.
            // `on_enter_key` returns whether it consumed the key — only then do
            // we stop propagation (otherwise the field inserts the newline). An
            // open slash/mention overlay makes ↵ accept the highlighted item.
            .capture_action(cx.listener(|this, _action: &InputEnter, window, cx| {
                let shift = window.modifiers().shift;
                let handled = this
                    .composer
                    .update(cx, |c, cx| c.on_enter_key(shift, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            }))
            // Drop image files anywhere on the chat surface to attach them.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.attach_dropped_paths(paths.paths().to_vec(), cx);
            }))
            .child(transcript)
            // The thinking-level pill shows once any assistant thought exists in
            // the transcript (nothing to toggle otherwise).
            .when(
                self.thread.entries.iter().any(|e| {
                    matches!(e, ThreadEntry::Assistant(m) if !m.thinking.is_empty())
                }),
                |col| col.child(self.render_thinking_toggle(cx)),
            )
            // A pinned "awaiting approval — jump" banner when a pending card is
            // scrolled off above the composer.
            .children(self.render_awaiting_banner(cx))
            // Staged-edit banner + rewind-confirm card sit just above the
            // composer while active (mutually exclusive — entering edit clears
            // any open confirm).
            .children(self.render_pending_edit_banner(window, cx))
            .children(self.render_rewind_confirm(window, cx))
            // The in-chat session browser, when open, sits above the composer as
            // a centered, width-capped card (matching the banners).
            .children(self.session_picker.clone().map(|picker| {
                div().flex().w_full().justify_center().child(picker)
            }))
            .child(self.composer.clone())
            // The image lightbox overlays everything when a thumbnail is opened.
            .children(self.render_image_preview(cx))
    }
}

/// Estimate the extra scrollable room needed to reveal a reply's under-counted
/// tail (see the tail-gap comment in `render_transcript`). gpui-component's
/// markdown folds only its FIRST block into the measured height; the remaining
/// blocks paint but sit past `content_size`, so scroll-to-bottom can't bring them
/// into view without extra room. This returns a padded over-estimate of those
/// trailing blocks' height — a text heuristic (wrapped lines × line height plus a
/// per-block gap), biased high so a slightly-off estimate errs toward a hair of
/// bottom slack rather than re-clipping, and capped so a runaway reply can't
/// reserve an absurd gap. A single-block reply reserves nothing (it measures
/// whole), keeping its bottom margin tight.
fn markdown_reveal_gap(body: &str, typo: &Typography) -> f32 {
    let Some((_, rest)) = body.split_once("\n\n") else {
        return 0.0;
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return 0.0;
    }
    // Roughly the chars that fit on one line of the reading column at the body
    // font size, times a per-line height plus a per-block gap. The markdown body
    // renders at ~2x the font size per line (line + inter-line leading), so the
    // reveal tracks that rather than a tight 1.5x — a smaller factor lands the
    // gap just short of the reply's last line. Capped so a runaway reply can't
    // reserve an absurd gap.
    let chars_per_line = (CONTENT_MAX_W / (typo.t_body_md * 0.5)).max(1.0);
    let line_height = typo.t_body_md * 2.0;
    let mut lines = 0.0f32;
    let mut blocks = 0.0f32;
    for block in rest.split("\n\n") {
        blocks += 1.0;
        for raw in block.lines() {
            lines += (raw.chars().count() as f32 / chars_per_line).ceil().max(1.0);
        }
    }
    (lines * line_height + blocks * typo.t_body_md).min(1100.0)
}

/// A live "Claude is working…" row shown at the tail of the transcript while a
/// turn streams — a stepped rotating spinner (the reused rail cadence: 12
/// mechanical ticks/sec) plus muted text. Keeping it here rather than above the
/// composer means the input never resizes when a turn starts or ends.
fn working_indicator(theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .w_full()
        .child(
            Icon::default()
                .path("icons/loader-circle.svg")
                .size(px(13.0))
                .text_color(theme.fg_muted)
                .with_animation(
                    SharedString::from("chat-working-spinner"),
                    Animation::new(Duration::from_secs(1)).repeat(),
                    |icon, delta| {
                        let stepped = (delta * 12.0).floor() / 12.0;
                        icon.transform(Transformation::rotate(percentage(stepped)))
                    },
                ),
        )
        .child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Claude is working…")),
        )
        .into_any_element()
}

/// A muted one-line turn summary (the backend's `post_turn_summary` detail),
/// shown under a settled turn like a subtle status caption.
fn summary_line(text: &str, theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// A context-compaction / truncation divider — a centered muted label flanked by
/// hairline rules, marking where imported history was summarized or capped.
fn compaction_divider(summary: &str, theme: Theme, typo: &Typography) -> AnyElement {
    let rule = || div().flex_1().h(px(1.0)).bg(theme.border_inactive);
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(10.0))
        .py(px(4.0))
        .child(rule())
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(summary.to_string())),
        )
        .child(rule())
        .into_any_element()
}

/// The per-turn usage footer: input/output tokens, an optional context-window
/// percentage, and cost when reported — a calm, muted caption.
fn usage_footer(usage: &TurnUsage, theme: Theme, typo: &Typography) -> AnyElement {
    let mut parts = vec![
        format!("{} in", fmt_tokens(usage.input_tokens)),
        format!("{} out", fmt_tokens(usage.output_tokens)),
    ];
    if let Some(window) = usage.context_window.filter(|w| *w > 0) {
        let used = usage.input_tokens + usage.cache_read_tokens + usage.cache_creation_tokens;
        let pct = ((used as f64 / window as f64) * 100.0).round() as u64;
        parts.push(format!("{pct}% ctx"));
    }
    if let Some(cost) = usage.cost_usd.filter(|c| *c > 0.0) {
        parts.push(format!("${cost:.3}"));
    }
    div()
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(parts.join(" · ")))
        .into_any_element()
}

/// Compact token count for the footer: `714`, `1.2k`, `16.7k`.
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// The assistant caption row: the "Claude" label on the left and a Copy
/// affordance on the right that's revealed while the message block is hovered
/// (`group`) — the copy-on-hover pattern of a native chat. Clicking copies the
/// reply's raw markdown to the clipboard. Built here (not `bubble`) because the
/// click needs a `Context` listener.
fn assistant_header(
    group: SharedString,
    text: &str,
    theme: Theme,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let copy_text = text.to_string();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .child(bubble::role_caption("Claude", theme.fg_muted, typo))
        .child(
            div()
                .id(SharedString::from(format!("copy-{group}")))
                .flex_none()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .cursor_pointer()
                // Reserve its slot (invisible, not absent) so the caption never
                // shifts; reveal on hover of the surrounding message block.
                .invisible()
                .group_hover(group, |s| s.visible())
                .hover(|s| s.text_color(theme.fg_base))
                .on_click(cx.listener(move |_this, _e, _w, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                }))
                .child(SharedString::from("Copy")),
        )
        .into_any_element()
}

/// A collapsible thinking disclosure: a clickable header (chevron + "Thinking")
/// and, when expanded, the muted body. Built here rather than in `bubble` since
/// the toggle needs a `Context` listener.
fn thinking_block(
    idx: usize,
    expanded: bool,
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let chevron = if expanded { "▾" } else { "▸" };
    let header = div()
        .id(("agent-chat-thinking", idx))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .hover(|s| s.text_color(theme.fg_muted))
        .on_click(cx.listener(move |this, _e, _window, cx| this.toggle_thinking(idx, cx)))
        .child(SharedString::from(format!("{chevron} Thinking")));

    let mut block = div().flex().flex_col().gap(px(2.0)).w_full().child(header);
    if expanded {
        block = block.child(bubble::thinking_body(text, theme, density, typo));
    }
    block.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::StubConnection;
    use serde_json::json;

    #[test]
    fn markdown_reveal_gap_zero_for_single_block_and_scales_with_tail() {
        let typo = Typography::default();
        // A single-block reply is measured whole → no extra reveal room.
        assert_eq!(markdown_reveal_gap("just one paragraph, no blank line", &typo), 0.0);
        assert_eq!(markdown_reveal_gap("", &typo), 0.0);
        // The blocks AFTER the first drive the gap; more trailing text → more room.
        let one_tail = markdown_reveal_gap("first\n\nsecond paragraph", &typo);
        let two_tails =
            markdown_reveal_gap("first\n\nsecond paragraph\n\nthird paragraph here", &typo);
        assert!(one_tail > 0.0);
        assert!(two_tails > one_tail);
        // Runaway replies are capped so the tail can't reserve an absurd gap.
        let huge = "first\n\n".to_string() + &"x ".repeat(20_000);
        assert!(markdown_reveal_gap(&huge, &typo) <= 1100.0);
    }

    /// The spike's central fail-closed requirement: if the agent channel
    /// disconnects (process exit / EOF) while a permission is pending, the view
    /// rejects it — clearing the prompt and sending a best-effort deny — rather
    /// than leaving a dangling approval.
    #[gpui::test]
    async fn disconnect_fails_closed_pending_permission(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("edit notes");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({"file_path": "notes.txt"}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "notes.txt".into(),
                    suggestions: vec![],
                });
                assert!(
                    view.thread.pending_permission().is_some(),
                    "permission pending before disconnect"
                );

                view.on_disconnect(cx);

                assert!(
                    view.thread.pending_permission().is_none(),
                    "fail-closed clears the pending permission"
                );
                assert!(view.disconnected, "view marks itself disconnected");
            })
            .expect("window update");

        // Best-effort deny reached the (stub) connection.
        let sent = stub_probe.sent();
        assert!(
            sent.iter()
                .any(|s| s["response"]["response"]["behavior"] == "deny"),
            "disconnect must send a deny control_response, got {sent:?}"
        );
    }

    /// A normal streamed turn folds into user + assistant entries via `on_event`.
    #[gpui::test]
    async fn on_event_builds_transcript(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("hi");
                view.on_event(ThreadEvent::AssistantText("Hello!".into()), cx);
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: Some("Hello!".into()),
                        usage: None,
                        is_error: false,
                    },
                    cx,
                );
                assert_eq!(view.thread.entries.len(), 2, "user + assistant");
                assert!(!view.thread.turn_active, "turn ended");
            })
            .expect("window update");
    }

    /// The multi-line composer splits Enter by the shift modifier: a plain ↵
    /// submits and clears the draft; ⇧↵ falls through to the field as a newline
    /// and does NOT submit; an empty draft submits nothing even on ↵.
    #[gpui::test]
    async fn enter_submits_shift_enter_newlines(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Capture the composer's Submit events (the test constructor wires no
        // subscription, so observe them directly).
        let submits = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        window
            .update(cx, |view, _window, cx| {
                let sink = submits.clone();
                let sub = cx.subscribe(&view.composer, move |_this, _composer, ev, _cx| {
                    if let ComposerEvent::Submit { text, .. } = ev {
                        sink.borrow_mut().push(text.clone());
                    }
                });
                view._subscriptions.push(sub);
            })
            .expect("window update");

        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("hello", window, cx);
                    // ⇧↵ (shift) is not consumed → falls through to a newline.
                    assert!(
                        !c.on_enter_key(true, window, cx),
                        "Shift+Enter falls through to a newline"
                    );
                    assert_eq!(c.draft_for_test(cx), "hello", "Shift+Enter kept the draft");
                    // Plain ↵ (no shift) submits and clears the draft.
                    assert!(c.on_enter_key(false, window, cx), "Enter is consumed (submit)");
                    assert!(c.draft_for_test(cx).is_empty(), "submit cleared the draft");
                });
            })
            .expect("window update");
        cx.run_until_parked();
        assert_eq!(*submits.borrow(), vec!["hello".to_string()], "only plain Enter submitted");

        // An empty draft never submits, even on ↵ (consumed, but no event).
        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("", window, cx);
                    assert!(c.on_enter_key(false, window, cx), "Enter is still consumed");
                });
            })
            .expect("window update");
        cx.run_until_parked();
        assert_eq!(submits.borrow().len(), 1, "empty Enter emitted no Submit");
    }

    /// A message submitted while a turn streams is QUEUED (not sent), then
    /// released as a fresh turn when the streaming turn completes — the
    /// composer-parks + parent-drains-on-turn-end loop, end to end.
    #[gpui::test]
    async fn message_submitted_mid_turn_queues_then_sends_on_turn_end(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let count_users = |view: &AgentChatView| {
            view.thread
                .entries
                .iter()
                .filter(|e| matches!(e, ThreadEntry::User { .. }))
                .count()
        };

        window
            .update(cx, |view, window, cx| {
                // Start a turn.
                view.send_text("first".into(), Vec::new(), cx);
                assert!(view.thread.turn_active, "first send started a turn");
                assert_eq!(count_users(view), 1);

                // Submitting now parks the message instead of sending it (the
                // composer sees turn_active via the sync above).
                view.composer.update(cx, |c, cx| {
                    c.set_draft_for_test("second", window, cx);
                    c.submit(window, cx);
                    assert!(c.draft_for_test(cx).is_empty(), "queued submit cleared the draft");
                });
                assert_eq!(count_users(view), 1, "queued, not sent while the turn is active");

                // The turn completes → the queued message is released as a new turn.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false },
                    cx,
                );
                assert_eq!(count_users(view), 2, "queued message sent on turn end");
                assert!(view.thread.turn_active, "the flushed message started a fresh turn");

                // Queue now empty → a second turn end sends nothing more.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false },
                    cx,
                );
                assert_eq!(count_users(view), 2, "no phantom re-send when the queue is empty");
            })
            .expect("window update");
    }

    /// Accepting a slash command parks the caret after the inserted `/name `
    /// (not back at offset 0 in the multi-line box) and surfaces the command's
    /// argument hint until an argument is typed.
    #[gpui::test]
    async fn accepting_command_parks_caret_and_shows_arg_hint(cx: &mut TestAppContext) {
        use super::slash_command_catalog::{CommandCatalog, CommandGroup, CommandMeta};

        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, window, cx| {
                view.composer.update(cx, |c, cx| {
                    // A backend advertising `git`, enriched with an argument hint.
                    c.set_slash_commands(vec!["git".into(), "compact".into()], cx);
                    let mut cat = CommandCatalog::new();
                    cat.insert(
                        "git".into(),
                        CommandMeta {
                            description: Some("Git operations".into()),
                            argument_hint: Some("cm|cp|pr|merge [args]".into()),
                            group: CommandGroup::BuiltIn,
                            source_label: None,
                        },
                    );
                    c.set_command_catalog(cat, cx);

                    // Type a partial command, open the palette, accept the match.
                    c.set_draft_for_test("/gi", window, cx);
                    c.recompute_overlays_for_test(cx);
                    assert!(c.accept_highlighted_for_test(window, cx), "palette accepted a match");

                    // The whole `/git ` is inserted and the caret sits AFTER the
                    // trailing space — not jumped back to the start of the box.
                    assert_eq!(c.draft_for_test(cx), "/git ");
                    assert_eq!(c.cursor_for_test(cx), "/git ".len());

                    // The argument hint now shows (palette closed by the space).
                    assert_eq!(
                        c.usage_hint_for_test(cx),
                        Some(("git".to_string(), "cm|cp|pr|merge [args]".to_string())),
                    );

                    // Typing an argument hides the hint again.
                    c.set_draft_for_test("/git cm", window, cx);
                    c.recompute_overlays_for_test(cx);
                    assert_eq!(c.usage_hint_for_test(cx), None);
                });
            })
            .expect("window update");
    }

    /// Card buttons route Allow/Reject to the connection by request_id and flip
    /// the local status (Allow → InProgress; Deny → Rejected), clearing the
    /// pending prompt. Allow echoes the tool input as updatedInput.
    #[gpui::test]
    async fn approve_and_reject_route_permission_decisions(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("do two things");
                for (tid, rid, name, input) in [
                    ("t1", "r1", "Edit", json!({"file_path": "a.txt"})),
                    ("t2", "r2", "Bash", json!({"command": "rm x"})),
                ] {
                    view.thread.apply(&ThreadEvent::ToolCallStarted {
                        id: tid.into(),
                        name: name.into(),
                        input: input.clone(),
                    });
                    view.thread.apply(&ThreadEvent::PermissionRequested {
                        request_id: rid.into(),
                        tool_use_id: Some(tid.into()),
                        tool_name: name.into(),
                        input,
                        description: name.into(),
                        suggestions: vec![],
                    });
                }

                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({"file_path": "a.txt"}) },
                    cx,
                );
                view.resolve_permission(
                    "t2".into(),
                    "r2".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );

                assert!(
                    view.thread.pending_permission().is_none(),
                    "both permissions resolved"
                );
                assert_eq!(tool_status(view, "t1"), Some("InProgress"));
                assert_eq!(tool_status(view, "t2"), Some("Rejected"));
            })
            .expect("window update");

        let sent = stub_probe.sent();
        let allow = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r1")
            .expect("r1 control_response");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(
            allow["response"]["response"]["updatedInput"],
            json!({"file_path": "a.txt"})
        );
        let deny = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r2")
            .expect("r2 control_response");
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
    }

    /// Answering an AskUserQuestion routes the selection back as an `allow` whose
    /// `updatedInput` carries the answers map (keyed by question text), settles
    /// the tool locally, and clears the pending question.
    #[gpui::test]
    async fn answer_question_routes_selection_and_settles(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                use oximux_agents::thread::{parse_questions, QuestionAnswer, QuestionAnswers};
                view.thread.push_user_message("choose");
                let input = json!({"questions":[{"question":"Tabs or spaces?","header":"Indent",
                    "options":[{"label":"Tabs","description":""},{"label":"Spaces","description":""}],
                    "multiSelect":false}]});
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "AskUserQuestion".into(),
                    input: input.clone(),
                });
                view.thread.apply(&ThreadEvent::QuestionAsked {
                    request_id: "rq".into(),
                    tool_use_id: Some("t1".into()),
                    questions: parse_questions(&input),
                });
                assert_eq!(tool_status(view, "t1"), Some("AwaitingAnswer"));

                let mut answers = QuestionAnswers::default();
                answers.by_question.insert(
                    "q-0".into(),
                    QuestionAnswer { selected: vec!["Tabs".into()], custom: None },
                );
                view.answer_question("t1".into(), answers, cx);

                assert!(view.thread.pending_question().is_none(), "question answered");
                assert_eq!(tool_status(view, "t1"), Some("InProgress"));
            })
            .expect("window update");

        let sent = stub_probe.sent();
        let ans = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "rq")
            .expect("rq control_response");
        assert_eq!(ans["response"]["response"]["behavior"], "allow");
        assert_eq!(
            ans["response"]["response"]["updatedInput"]["answers"]["Tabs or spaces?"],
            json!("Tabs")
        );
    }

    /// A stray second click after a card is answered must not send a second
    /// control_response or flip the decision — the guard makes it a no-op.
    #[gpui::test]
    async fn second_answer_is_ignored(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                });
                // First answer: allow.
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({}) },
                    cx,
                );
                // Stray second answer: deny — must be ignored (already decided).
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );
                assert_eq!(
                    tool_status(view, "t1"),
                    Some("InProgress"),
                    "stays allowed, not flipped to Rejected by the second click"
                );
            })
            .expect("window update");

        let responses: Vec<_> = stub_probe
            .sent()
            .into_iter()
            .filter(|s| s["response"]["request_id"] == "r1")
            .collect();
        assert_eq!(responses.len(), 1, "exactly one control_response for r1");
        assert_eq!(responses[0]["response"]["response"]["behavior"], "allow");
    }

    /// Stop mid-turn: the turn clears, a pending approval fail-closes, and the
    /// tab enters resumable-idle (interrupted, NOT disconnected — no error).
    #[gpui::test]
    async fn stop_turn_interrupts_and_stays_resumable(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("do a long thing");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                });
                assert!(view.thread.turn_active, "turn active before Stop");

                view.stop_turn(cx);

                assert!(!view.thread.turn_active, "Stop ends the turn");
                assert!(view.interrupted, "session marked resumable-idle");
                assert!(!view.disconnected, "an intentional Stop is not a disconnect");
                assert!(
                    view.thread.pending_permission().is_none(),
                    "pending approval fail-closes on Stop"
                );
                assert_eq!(tool_status(view, "t1"), Some("Rejected"));

                // The interrupt `result` arrives flagged as an error; it must be
                // swallowed, not shown as a banner.
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: None,
                        usage: None,
                        is_error: true,
                    },
                    cx,
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "the interrupt's error result is suppressed"
                );

                // The child's stdout then EOFs: still resumable, still no error.
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after an intentional Stop stays resumable");
                assert!(view.interrupted);
                assert!(view.thread.last_error.is_none());
            })
            .expect("window update");
    }

    /// Order-independence: if the child's stdout EOF is observed BEFORE the
    /// interrupt's `result` event, the tab must still stay resumable-idle (not
    /// flip to disconnected/unavailable), and a straggler error result arriving
    /// afterward is still suppressed.
    #[gpui::test]
    async fn stop_then_eof_before_result_stays_resumable(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.stop_turn(cx);
                assert!(view.interrupted);

                // EOF arrives first (before any TurnEnded is folded in).
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after Stop stays resumable, order-independent");
                assert!(view.thread.last_error.is_none());

                // A late error result then folds in — still suppressed.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: true },
                    cx,
                );
                assert!(view.thread.last_error.is_none());
                assert!(view.interrupted, "still resumable for the next send");
            })
            .expect("window update");
    }

    /// A Stop with no live turn is a no-op (nothing to interrupt).
    #[gpui::test]
    async fn stop_turn_without_active_turn_is_noop(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                assert!(!view.thread.turn_active);
                view.stop_turn(cx);
                assert!(!view.interrupted, "no turn → Stop does nothing");
            })
            .expect("window update");
    }

    /// Rewind race: while a rewind is in flight the connection is taken and the
    /// old child killed, so the old drain task's `on_disconnect` fires on the
    /// foreground racing the rewind's completion. Because `perform_rewind` marks
    /// the kill intentional (`interrupted = true`), that stray `on_disconnect`
    /// must take its resumable-idle branch — NOT strand the tab as disconnected.
    #[gpui::test]
    async fn on_disconnect_during_rewind_does_not_strand_tab(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                // Simulate the state perform_rewind establishes before its
                // background half runs: connection taken, kill marked intentional.
                view.thread.session_id = Some("old-sid".into());
                view.rewinding = true;
                view.interrupted = true;
                view.connection = None;

                // The killed child's stdout EOFs mid-rewind.
                view.on_disconnect(cx);

                assert!(
                    !view.disconnected,
                    "EOF during a rewind must not mark the tab disconnected"
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "no error banner for the rewind's own intentional kill"
                );
                assert!(view.interrupted, "stays resumable-idle for finish_rewind");
            })
            .expect("window update");
    }

    /// Staged edit-and-resend must be a TRUE no-op on cancel: entering edit mode
    /// prefills the composer and dims later messages, but Escape/cancel restores
    /// the prior draft and touches neither the transcript nor the session.
    #[gpui::test]
    async fn pending_edit_cancel_is_a_no_op(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.thread.session_id = Some("sid".into());
                view.thread.push_user_message("first");
                view.thread.apply(&ThreadEvent::AssistantText("a1".into()));
                view.thread.push_user_message("second");
                view.thread.apply(&ThreadEvent::AssistantText("a2".into()));
                // Edit is only offered on an idle turn (a live turn would queue
                // the resend instead of routing it).
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: None,
                    usage: None,
                    is_error: false,
                });
                let entries_before = view.thread.entries.clone();

                // The user was mid-typing an unrelated draft WITH a staged image.
                let staged = ChatImage {
                    media_type: "image/png".into(),
                    // 1x1 transparent PNG.
                    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".into(),
                };
                view.composer.update(cx, |c, cx| {
                    c.prefill("half-typed thought".into(), vec![staged.clone()], window, cx)
                });

                // Edit the FIRST user message (entry index 0).
                view.enter_pending_edit(0, window, cx);
                assert!(view.pending_edit.is_some(), "edit mode entered");
                assert_eq!(
                    view.composer.read(cx).current_draft(cx),
                    "first",
                    "composer prefilled with the edited message"
                );
                assert!(view.is_pending_edit_dimmed(1), "later messages dim");
                assert!(!view.is_pending_edit_dimmed(0), "the edited message itself is not dimmed");

                // Cancel: draft AND staged image restored, nothing removed.
                view.cancel_pending_edit(window, cx);
                assert!(view.pending_edit.is_none(), "edit mode exited");
                assert_eq!(
                    view.composer.read(cx).current_draft(cx),
                    "half-typed thought",
                    "the prior draft is restored verbatim"
                );
                assert_eq!(
                    view.composer.read(cx).current_images(),
                    vec![staged],
                    "the pre-existing staged image is restored (true no-op)"
                );
                assert_eq!(view.thread.entries, entries_before, "transcript untouched");
                assert_eq!(view.thread.session_id.as_deref(), Some("sid"), "session untouched");
            })
            .expect("window update");
    }

    /// A manual collapse during Auto stream auto-expand must register on the
    /// first click (the collapsed override wins over the streaming peek).
    #[gpui::test]
    async fn thinking_manual_collapse_wins_over_auto_stream(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                // A streaming thought on the last entry, no text yet, turn active.
                view.thread.apply(&ThreadEvent::ThinkingDelta("pondering".into()));
                assert!(view.thread.turn_active);
                let last = view.thread.entries.len() - 1;
                let msg = match &view.thread.entries[last] {
                    ThreadEntry::Assistant(m) => m.clone(),
                    _ => panic!("expected assistant entry"),
                };
                assert!(
                    view.thinking_expanded(last, true, &msg),
                    "Auto auto-expands the streaming thought"
                );

                // One click must collapse it despite the auto-expand.
                view.toggle_thinking(last, cx);
                assert!(
                    !view.thinking_expanded(last, true, &msg),
                    "manual collapse wins on the FIRST click"
                );
                // Toggling again re-expands.
                view.toggle_thinking(last, cx);
                assert!(view.thinking_expanded(last, true, &msg), "re-expands on next click");
            })
            .expect("window update");
    }

    #[test]
    fn tool_grouping_leaves_short_runs_and_messages_alone() {
        // messages interleaved with short tool runs: nothing collapses.
        let is_tool = vec![false, true, true, false, true, false];
        let force = vec![false; 6];
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        assert!(plan.iter().all(|d| matches!(d, EntryDisplay::Show)));
    }

    fn tool_status(view: &AgentChatView, id: &str) -> Option<&'static str> {
        view.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(match tc.status {
                ToolCallStatus::InProgress => "InProgress",
                ToolCallStatus::Rejected => "Rejected",
                ToolCallStatus::Completed => "Completed",
                ToolCallStatus::WaitingForConfirmation(_) => "WaitingForConfirmation",
                ToolCallStatus::AwaitingAnswer(_) => "AwaitingAnswer",
                _ => "Other",
            }),
            _ => None,
        })
    }
}

/// A hover-revealed action chip on a user message (Edit / Rewind). Rendered as a
/// small ghost button — icon + label inside a subtle bordered pill that fills on
/// hover — so the affordance reads as a control rather than loose link text.
fn message_action_chip(
    id: SharedString,
    icon: &'static str,
    label: &'static str,
    theme: Theme,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(7.0))
        .py(px(3.0))
        .rounded(px(6.0))
        .text_xs()
        .text_color(theme.fg_muted)
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
        .cursor_pointer()
        .hover(|s| {
            s.bg(theme.hover_overlay)
                .text_color(theme.fg_base)
                .border_color(theme.border_active)
        })
        .child(div().text_color(theme.fg_subtle).child(icon))
        .child(label)
        .on_mouse_down(MouseButton::Left, on_click)
}
