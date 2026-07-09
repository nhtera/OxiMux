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

mod background_tasks_panel;
mod bubble;
mod composer;
mod composer_history;
mod context_providers;
mod diff_card;
mod error_card;
mod find_bar;
mod image_attach;
mod jump_menu;
mod message_rail;
mod pending_edit;
mod plan_panel;
mod question_card;
mod rewind_menu;
mod roster;
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
use gpui_component::input::Escape as InputEscape;
use gpui_component::scroll::Scrollbar;

/// Max width of the reading column (transcript + composer). Wider windows keep
/// the conversation centered in a comfortable measure rather than stretching
/// text edge-to-edge — the calm, focused feel of a dedicated chat surface.
pub(super) const CONTENT_MAX_W: f32 = 720.0;

/// Width of the left timeline gutter (the message tick-rail). The reading column
/// sits to its right; overlays (jump dropdown, hover preview) offset by this.
pub(super) const RAIL_W: f32 = 30.0;

/// How many frames to keep re-pinning the transcript to the bottom after a
/// content change (see [`AgentChatView::follow_frames`]). ~10 frames (≈160ms at
/// 60fps) comfortably outlasts the async markdown parse/layout of a normal reply
/// so the follow catches the message's settled height, then stops (no idle spin).
const FOLLOW_FRAMES: u8 = 10;

/// How many frames the jumped-to-message highlight lingers before it clears.
/// ~48 frames (≈0.8s at 60fps) — long enough to catch the eye after a jump,
/// short enough not to distract. The tint alpha scales with the remaining
/// frames so it fades out rather than snapping off.
const FLASH_FRAMES: u8 = 48;

/// Bundle the live connection's picker vocabulary (models / permission modes /
/// efforts + their "current when unset" defaults) for the composer. Empty when
/// there's no connection (spawn failed) — the pickers then show only the current
/// value as static text. The vocab now lives with the backend that speaks it
/// (the agents crate), not as app-crate constants, so a non-Claude provider
/// advertises its own set with no view change.
fn control_vocab_of(conn: Option<&dyn AgentConnection>) -> ControlVocab {
    match conn {
        Some(c) => ControlVocab {
            models: c.models(),
            permission_modes: c.permission_modes(),
            efforts: c.efforts(),
            default_model: c.default_model(),
            default_mode: c.default_mode(),
            default_effort: c.default_effort(),
        },
        None => ControlVocab::default(),
    }
}

/// Decoded user-attached image thumbnails, memoized by `(entry index, image
/// index)`. Interior-mutable so the immutable `render` path can fill it lazily.
type ImageCache = RefCell<HashMap<(usize, usize), Option<Arc<Image>>>>;

/// Events the chat view raises for its host (the pane group) to act on.
pub enum AgentChatEvent {
    /// The user picked a different model; the host persists it in the tab kind
    /// so the choice survives relaunch (the view already respawned on it).
    ModelChanged(String),
    /// The agent set a session title (ACP `session_info_update`); the host uses it
    /// as the tab's fallback label (a user's manual rename still wins over it).
    TitleChanged(String),
    /// "Fork from here": a truncated fork of this session was written to disk;
    /// the host should open it as a NEW chat tab (this tab is left untouched).
    /// Carries everything `open_agent_chat_tab_restored` needs to rehydrate the
    /// branch and resume it with `--resume <session_id>`.
    ForkReady {
        cwd: PathBuf,
        model: Option<String>,
        session_id: String,
        entries: Vec<ThreadEntry>,
        slash_commands: Vec<String>,
        thinking_level: ThinkingLevel,
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

use composer::{ComposerEvent, ComposerView, ControlVocab};
use context_providers::{ContextRequest, ContextSource};
use question_card::{QuestionCard, QuestionCardEvent};
use tool_grouping::{plan_tool_grouping, EntryDisplay};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_view::TerminalView;
use oximux_agents::thread::{
    connect, AgentConnection, AssistantMessage, ChatBackend, ChatImage, ChatThread, ConnectSpec,
    ModelChoice, PermissionDecision, QuestionAnswers, QuestionRequest, ThreadEntry, ThreadEvent,
    ToolCallStatus, Transport, TurnUsage,
};
use oximux_git::GitCmd;
use oximux_settings::{AgentLaunchSettings, Density, Theme, Typography};

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
    /// Which backend this chat runs over (Claude stream-json / Codex app-server /
    /// an external ACP command). Threaded into every `ConnectSpec` (fresh +
    /// respawn) and written to the persisted transcript so a restore reconnects
    /// the same provider — including the ACP command, which settings don't retain
    /// per session.
    backend: ChatBackend,
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
    /// A "draft" chat opened via the unified **New Agent** entry: no subprocess
    /// has spawned yet, and `self.backend`/`self.model` reflect the *currently
    /// picked* (but not yet committed) agent + model. Deferred binding — the
    /// first `send_text` flips this off and `respawn()`s to spawn the chosen
    /// agent (see [`Self::bind_now`]). A chat opened via a per-agent quick-launch
    /// starts already bound (`false`), so its lifecycle is unchanged.
    unbound: bool,
    /// While `unbound`, the adapter id of the *currently picked* agent (the
    /// composer's agent dropdown selection) — e.g. `claude-code`/`codex`/
    /// `opencode`. Drives the pre-bind model vocab and the post-bind tab label.
    /// `None` for a chat that started bound. Retained (not cleared) after binding
    /// so the label resolution still finds the roster display name.
    unbound_agent_id: Option<String>,
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
    /// Whether the Background Tasks drawer (subagents + background bash) is
    /// expanded. The toggle chip only shows once the turn has spawned a task.
    show_background_tasks: bool,
    /// Transient highlight on a message the user jumped to (rewind menu, jump
    /// nav, message rail), by entry index. Set on jump, fades over
    /// [`FLASH_FRAMES`] frames, then clears. Cleared on rewind/truncate so a
    /// shifted index never tints the wrong bubble.
    flash_entry: Option<usize>,
    flash_frames: u8,
    /// Entry index whose Copy action just fired — swaps that bubble's copy glyph
    /// to a ✓ for a beat as confirmation. Transient/cosmetic, so an index that
    /// shifts under a rewind at worst mistints for <1.5s. Reverted by
    /// [`Self::_copied_clear_task`].
    recently_copied: Option<usize>,
    /// Reverts `recently_copied` to `None` a beat after a copy. Held so a rapid
    /// second copy replaces (cancels) the prior revert timer.
    _copied_clear_task: Option<Task<()>>,
    /// Child index within the tracked scroll box for each USER turn, in order
    /// (user ordinal → child index). Rebuilt every render so a jump can
    /// `ScrollHandle::scroll_to_item` the exact bubble. `RefCell` because the
    /// transcript renders behind `&self`; only touched on the main thread.
    user_child_ix: RefCell<Vec<usize>>,
    /// Child index within the tracked scroll box for each RENDERED entry
    /// (`entry index → child index`), rebuilt every render. Unlike
    /// [`Self::user_child_ix`] this covers assistant/tool entries too, so the
    /// in-chat find bar can jump to any matching entry, not just user turns.
    entry_child_ix: RefCell<HashMap<usize, usize>>,
    /// The in-transcript find bar (Cmd+F), when open. See [`find_bar`].
    find_bar: Option<find_bar::FindBar>,
    /// Pointer is over the left tick-rail. Either this or [`Self::menu_hover`]
    /// being set expands the jump-to-message list next to the rail — hovering
    /// the rail reveals it, hovering the list keeps it open (they sit edge-to-
    /// edge, so the pointer never leaves both at once while crossing between).
    rail_hover: bool,
    /// Pointer is over the expanded jump-to-message list. See [`Self::rail_hover`].
    menu_hover: bool,
    /// Weak handle to the owning pane group, set after construction by the tab
    /// factory. Lets the `@terminal` context provider enumerate sibling terminal
    /// tabs and pull their scrollback. Weak so it never keeps the group alive (the
    /// group already owns this view's `Entity`). `None` in tests / standalone use,
    /// which simply omits the terminal sources.
    pane_group: Option<WeakEntity<PaneGroup>>,
}

/// Compute, for each USER turn in order, its child index within the flattened
/// transcript scroll box — the input to `ScrollHandle::scroll_to_item` for a
/// jump. Each slice is indexed by entry position in transcript order:
/// `produces[i]` = entry `i` rendered a direct child element; `is_user[i]` =
/// it's a user turn; `has_expander[i]` = a collapsed-tool-run expander child is
/// pushed right after it. Children are counted in the exact push order
/// `render_transcript` uses, so the returned indices line up with the tracked
/// `list_scroll` child bounds. Pure + unit-tested; render feeds it the live
/// per-entry flags.
fn user_turn_child_indices(produces: &[bool], is_user: &[bool], has_expander: &[bool]) -> Vec<usize> {
    let mut child_ord = 0usize;
    let mut out = Vec::new();
    for (i, &produced) in produces.iter().enumerate() {
        if produced {
            if is_user.get(i).copied().unwrap_or(false) {
                out.push(child_ord);
            }
            child_ord += 1;
        }
        if has_expander.get(i).copied().unwrap_or(false) {
            child_ord += 1;
        }
    }
    out
}

/// Map every RENDERED entry's transcript index → its scroll-child index (for the
/// in-chat find bar's jump-to-match), mirroring the push order in
/// `render_transcript`: one child per producing row, plus one per trailing
/// expander. Rows that produce no element are absent. Pure for unit testing.
fn entry_child_indices(
    entry_idx: &[usize],
    produces: &[bool],
    has_expander: &[bool],
) -> Vec<(usize, usize)> {
    let mut child_ord = 0usize;
    let mut out = Vec::new();
    for i in 0..produces.len() {
        if produces[i] {
            out.push((entry_idx[i], child_ord));
            child_ord += 1;
        }
        if has_expander.get(i).copied().unwrap_or(false) {
            child_ord += 1;
        }
    }
    out
}

impl AgentChatView {
    /// Construct a chat view and spawn its headless `claude` subprocess in
    /// `cwd`. A spawn failure degrades to a read-only error state rather than
    /// panicking, so the tab still opens and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            true,
            theme,
            density,
            typography,
            window,
            cx,
        )
    }

    /// Construct an **unbound** chat view for the unified *New Agent* entry: no
    /// subprocess is spawned. `backend`/`model` seed the *currently picked* agent
    /// (the composer's agent picker can change them before the first send); the
    /// first `send_text` binds the transport and spawns the chosen agent. A
    /// provider-agnostic caller defaults to [`ChatBackend::stream_json`] (Claude).
    #[allow(clippy::too_many_arguments)]
    pub fn new_unbound(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            false,
            theme,
            density,
            typography,
            window,
            cx,
        );
        // Seed the composer's agent + model pickers from the chat roster so the
        // draft offers the choice on its first paint (before any subprocess).
        view.sync_unbound_composer(cx);
        view
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
        backend: ChatBackend,
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
        let mut view = Self::assemble(
            cwd, model, backend, thread, true, theme, density, typography, window, cx,
        );
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
        backend: ChatBackend,
        mut thread: ChatThread,
        // `false` opens an unbound **New Agent** draft: skip the subprocess spawn
        // entirely and wait for the first send to bind (see [`Self::new_unbound`]).
        connect_now: bool,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            ComposerView::new(
                theme,
                density,
                typography.clone(),
                backend.provider_display_name(),
                window,
                cx,
            )
        });
        // The composer owns its input and repaints itself per keystroke. We only
        // react when it reports a finished submission — so typing never touches
        // this view (and thus never rebuilds the transcript, which is the lag we
        // want to avoid).
        let subscriptions = vec![cx.subscribe(
            &composer,
            |this, _composer, ev: &ComposerEvent, cx| match ev {
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
                ComposerEvent::NewChat => this.new_chat(cx),
                ComposerEvent::ModelPicked(model) => this.change_model(model.clone(), cx),
                ComposerEvent::PermissionModePicked(mode) => {
                    this.change_permission_mode(mode.clone(), cx)
                }
                ComposerEvent::EffortPicked(effort) => this.change_effort(effort.clone(), cx),
                ComposerEvent::AgentPicked(id) => this.change_agent(id.clone(), cx),
                ComposerEvent::MentionOpened => this.refresh_context_sources(cx),
                ComposerEvent::CaptureContext(request) => {
                    this.capture_context(request.clone(), cx)
                }
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
        // An unbound draft (`!connect_now`) spawns nothing yet — the first send
        // binds via `respawn()`, which connects `self.backend` fresh.
        if connect_now {
            match connect(ConnectSpec::for_backend(
                &backend,
                cwd.clone(),
                model.clone(),
                resume_session_id.clone(),
                None,
                None,
            )) {
                Ok((conn, rx)) => {
                    connection = Some(conn);
                    drain_task = Some(Self::spawn_drain(rx, cx));
                }
                Err(e) => {
                    thread.last_error = Some(format!("Failed to start agent: {e}"));
                    disconnected = true;
                }
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
        let vocab = control_vocab_of(connection.as_deref());
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
            c.set_controls(model.clone(), None, None, caps.supports_modes, caps.supports_config, vocab, cx);
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

        // The unbound draft's initial agent id, derived from the seed backend's
        // transport (a New Agent draft defaults to Claude; the composer's agent
        // picker can switch it before the first send). `None` when bound.
        let unbound_agent_id = (!connect_now).then(|| {
            match backend.transport {
                Transport::StreamJson => "claude-code",
                Transport::AppServer => "codex",
                Transport::Acp => "cursor",
            }
            .to_string()
        });

        Self {
            thread,
            connection,
            backend,
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
            unbound: !connect_now,
            unbound_agent_id,
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
            pane_group: None,
            show_background_tasks: false,
            flash_entry: None,
            flash_frames: 0,
            recently_copied: None,
            _copied_clear_task: None,
            user_child_ix: RefCell::new(Vec::new()),
            entry_child_ix: RefCell::new(HashMap::new()),
            find_bar: None,
            rail_hover: false,
            menu_hover: false,
        }
    }

    /// Snapshot the transcript for persistence, or `None` when there's nothing
    /// worth restoring. A session id is required (it keys the blob and drives
    /// `--resume`); a chat with no completed turn has neither an id nor history,
    /// so it simply won't restore — the tab reopens fresh.
    /// Whether the live backend keeps an on-disk session log the rewind/fork
    /// truncate-fork can read (Claude's `~/.claude/projects/*.jsonl`; Codex/ACP
    /// don't). Gates the Edit / Rewind / Regenerate / Fork affordances so they
    /// aren't offered on a backend whose session file the fork can't locate.
    fn backend_supports_rewind(&self) -> bool {
        self.connection
            .as_ref()
            .map(|c| c.capabilities().supports_rewind)
            .unwrap_or(false)
    }

    /// Human-facing provider name for this chat's captions, placeholder, and
    /// permission prompts ("Claude" for stream-json, "Codex" for app-server).
    /// Sourced from the transport (fixed at launch) so it's correct even before a
    /// connection exists — the empty state and composer render immediately.
    fn provider_label(&self) -> &'static str {
        self.backend.provider_display_name()
    }

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
            // The backend that minted this session, so a restored tab reconnects
            // the same provider (Claude stream-json / Codex app-server / an ACP
            // command). The ACP command + args ride along because settings don't
            // retain them per session — the transcript is the source of truth on
            // restore. Empty for Claude/Codex.
            provider: self.backend.transport,
            acp_command: self.backend.acp_command.clone(),
            acp_args: self.backend.acp_args.clone(),
        })
    }

    /// The chat's session id once Claude has minted one (after the first turn
    /// begins). Persisted in the tab's `PersistedTabKind::AgentChat` so restore
    /// can find the matching transcript blob and `--resume`.
    pub fn session_id(&self) -> Option<&str> {
        self.thread.session_id.as_deref()
    }

    /// Whether this is still an unbound *New Agent* draft (no subprocess has
    /// spawned; the first send binds it). The host reads this to label the tab
    /// "New Agent" and to skip persisting the empty draft.
    pub fn is_unbound(&self) -> bool {
        self.unbound
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
        let vocab = control_vocab_of(self.connection.as_deref());
        let (model, permission_mode, effort) =
            (self.model.clone(), self.permission_mode.clone(), self.effort.clone());
        // The command palette is offered only when the backend advertises
        // commands (Claude does; others send an empty list, which disables it).
        let slash_commands =
            if caps.supports_slash { self.thread.slash_commands.clone() } else { Vec::new() };
        self.composer.update(cx, |c, cx| {
            c.set_state(disconnected, turn_active, cx);
            c.set_controls(model, permission_mode, effort, caps.supports_modes, caps.supports_config, vocab, cx);
            c.set_slash_commands(slash_commands, cx);
            // A bound chat never shows the agent picker (its transport is fixed);
            // clearing here is what hides it after `bind_now` (cheap no-op once
            // already cleared).
            c.set_agent_picker(false, Vec::new(), None, cx);
        });
    }

    /// Seed the composer for an unbound *New Agent* draft: push the chat roster
    /// into the agent dropdown, mark the current pick, and offer the picked
    /// agent's static pre-bind model vocabulary (so the model picker has choices
    /// before any subprocess exists). Mode + effort pickers stay hidden until the
    /// connection binds and advertises its real capabilities. Called on
    /// construction and after every agent/model pick while unbound.
    fn sync_unbound_composer(&self, cx: &mut Context<Self>) {
        let roster = roster::chat_roster_from_cx(cx);
        let agents: Vec<(String, String)> =
            roster.iter().map(|e| (e.id.clone(), e.display.clone())).collect();
        let current = self.unbound_agent_id.as_ref().and_then(|id| {
            roster.iter().find(|e| &e.id == id).map(|e| (e.id.clone(), e.display.clone()))
        });
        // The picked agent's static model list becomes the pre-bind vocab; ACP
        // presets carry none (their models come from session negotiation), so the
        // model picker simply hides until bound.
        let vocab = self
            .unbound_agent_id
            .as_ref()
            .and_then(|id| roster.iter().find(|e| &e.id == id))
            .map(|e| ControlVocab {
                models: e.models.iter().map(|m| ModelChoice { wire: m.clone() }).collect(),
                permission_modes: Vec::new(),
                efforts: Vec::new(),
                default_model: e.default_model().map(str::to_string),
                default_mode: None,
                default_effort: None,
            })
            .unwrap_or_default();
        let model = self.model.clone();
        self.composer.update(cx, |c, cx| {
            c.set_agent_picker(true, agents, current, cx);
            // Pre-bind: only the model picker (no modes/effort until the live conn).
            c.set_controls(model, None, None, false, false, vocab, cx);
        });
    }

    /// The display name of the currently-picked unbound agent (from the roster),
    /// used to relabel the tab after binding. `None` when bound / unresolved.
    fn unbound_agent_display(&self, cx: &App) -> Option<String> {
        let id = self.unbound_agent_id.as_ref()?;
        roster::chat_roster_from_cx(cx).into_iter().find(|e| &e.id == id).map(|e| e.display)
    }

    /// Switch the *picked* agent on an unbound draft: rebuild the backend
    /// (transport + ACP command/args) for the new adapter id, preselect that
    /// agent's default model, and re-seed the composer's agent + model pickers.
    /// No subprocess is touched — binding still waits for the first send. No-op
    /// once bound (a live session's transport is fixed) or when unchanged.
    fn change_agent(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.unbound || self.unbound_agent_id.as_deref() == Some(id.as_str()) {
            return;
        }
        // Resolve the backend for this id from the live settings global (mirrors
        // the launcher's own preset resolution).
        self.backend = match cx.try_global::<AgentLaunchSettings>() {
            Some(settings) => crate::workspace_root::chat_backend_for(settings, &id),
            None => crate::workspace_root::chat_backend_for(&AgentLaunchSettings::default(), &id),
        };
        // Preselect the new agent's default model; drop the prior mode/effort so
        // the draft doesn't carry a selector the new agent may not support.
        let roster = roster::chat_roster_from_cx(cx);
        self.model = roster
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.default_model().map(str::to_string));
        self.thread.model = self.model.clone();
        self.permission_mode = None;
        self.effort = None;
        self.unbound_agent_id = Some(id);
        self.sync_unbound_composer(cx);
        cx.notify();
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
        // `/clear` is a UI command (blank the transcript + free context), not
        // agent input — reset in place rather than transmitting the literal text
        // to the subprocess (matching the CLI's own TUI). `/compact` stays a
        // pass-through; real compaction is a backend concern.
        if images.is_empty() && text.trim() == "/clear" {
            self.new_chat(cx);
            return;
        }
        // First message on an unbound *New Agent* draft: spawn the picked agent
        // now (deferred binding), then send into the fresh session. A bind failure
        // leaves `disconnected` set with the error, handled by the guard below.
        if self.unbound {
            self.bind_now(cx);
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

    /// Re-send the last user prompt after a turn ended in error (or the child
    /// crashed). Reachable only from the idle error / disconnected tail cards —
    /// gated on `!turn_active` so it never double-sends mid-turn. A crashed or
    /// stopped child is respawned (via `--resume`) before the prompt is
    /// retransmitted; the prompt bubble is already the tail entry, so it is NOT
    /// pushed again.
    fn retry_last_turn(&mut self, cx: &mut Context<Self>) {
        if self.thread.turn_active {
            return; // a turn is already streaming — nothing to retry
        }
        // A crashed / stopped child can't receive input — bring it back first.
        if self.disconnected || self.interrupted {
            self.respawn(cx);
            if self.disconnected {
                // Respawn failed (e.g. the resume file is gone); `respawn` left
                // its own error text — keep the card rather than silently no-op.
                cx.notify();
                return;
            }
        }
        let last_user_idx = self
            .thread
            .entries
            .iter()
            .rposition(|e| matches!(e, ThreadEntry::User { .. }));
        let last_user = last_user_idx.and_then(|i| match &self.thread.entries[i] {
            ThreadEntry::User { text, images, .. } => Some((i, text.clone(), images.clone())),
            _ => None,
        });
        match last_user {
            Some((idx, text, images)) => {
                let sent = match &self.connection {
                    Some(conn) => match conn.send_user_message_with_images(&text, &images) {
                        Ok(()) => {
                            self.thread.last_error = None;
                            self.thread.turn_active = true;
                            true
                        }
                        Err(e) => {
                            self.thread.last_error = Some(format!("Send failed: {e}"));
                            false
                        }
                    },
                    None => false,
                };
                // Re-anchor the pre-turn checkpoint to the retried turn (as a
                // fresh send would), so the "restore files" rewind affordance
                // keeps tracking repo changes for it.
                if sent {
                    self.take_checkpoint_for(idx, cx);
                }
            }
            None => {
                // No prompt to replay (the connection failed before any turn) —
                // the respawn above already restored a working, error-free idle
                // state, so just drop the card.
                self.thread.last_error = None;
            }
        }
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// A small "Retry" control for the error / disconnected tail cards. Its
    /// click re-sends the last user prompt (respawning the child first if it
    /// crashed or was stopped).
    fn retry_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typo = &self.typography;
        div()
            .id("chat-retry-turn")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(theme.status_error.opacity(0.15))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_error)
            .hover(|s| s.bg(theme.status_error.opacity(0.28)))
            .child(
                Icon::default()
                    .path("icons/refresh-cw.svg")
                    .size(px(13.0))
                    .text_color(theme.status_error),
            )
            .child(SharedString::from("Retry"))
            .on_click(cx.listener(|this, _e, _window, cx| this.retry_last_turn(cx)))
    }

    /// Start a fresh conversation in this tab without closing it (Claude-Desktop
    /// "New chat" / the CLI's `/clear`). Blanks the transcript, drops any
    /// transient UI bound to it, and respawns a **non-resumed** session (the
    /// cleared thread has no session id, so `respawn` starts clean and reaps the
    /// old child). A fresh session mints its own id on the first turn, so the tab
    /// persists empty until then (`transcript_snapshot` returns `None`).
    fn new_chat(&mut self, cx: &mut Context<Self>) {
        // A rewind in flight will, on completion, overwrite this tab's session id
        // with its forked id and respawn again — which would silently resurrect
        // the discarded conversation into the "blank" new chat. Refuse until it
        // settles (mirrors every other rewind-adjacent entry point).
        if self.rewinding {
            return;
        }
        self.thread.clear();
        // Transient view state keyed to the old transcript must not survive.
        self.pending_edit = None;
        self.rewind_confirm = None;
        self.rewind_then_send = None;
        self.pre_turn_checkpoint = None;
        self.flash_entry = None;
        self.flash_frames = 0;
        self.recently_copied = None;
        // Respawn reads the now-`None` session id → a fresh session, and clears
        // `disconnected`/`interrupted`/`last_error` itself on success.
        self.respawn(cx);
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

    /// Commit an unbound *New Agent* draft: spawn the picked agent for the first
    /// time and relabel the tab from "New Agent" to the bound provider. `respawn`
    /// does the actual connect — with no session id and no old child to reap it
    /// simply starts a fresh session over `self.backend` (the currently-picked
    /// agent). After this the chat behaves like any bound chat. No-op if already
    /// bound. A connect failure leaves `disconnected` set (with the error), so the
    /// send path bails just as it does for a failed initial spawn.
    fn bind_now(&mut self, cx: &mut Context<Self>) {
        if !self.unbound {
            return;
        }
        self.unbound = false;
        self.respawn(cx);
        // Pick up the now-live connection's capability-gated pickers + vocab (this
        // also clears the agent picker, since the chat is now bound).
        self.sync_composer(cx);
        // Relabel the tab to the picked agent's name (`Cursor`/`OpenCode`/`Codex`/
        // `Claude`), which is more specific than the transport's generic
        // `provider_display_name` (ACP → "Agent"). Falls back to the transport
        // name if the roster can't resolve it. A user rename still wins; see the
        // host's `TitleChanged` handling.
        let label = self
            .unbound_agent_display(cx)
            .unwrap_or_else(|| self.backend.provider_display_name().to_string());
        cx.emit(AgentChatEvent::TitleChanged(label));
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
        match connect(ConnectSpec::for_backend(
            &self.backend,
            self.cwd.clone(),
            model,
            session_id,
            permission_mode,
            effort,
        )) {
            Ok((conn, rx)) => {
                self.connection = Some(conn);
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
        // On an unbound draft there's no subprocess to respawn — just record the
        // pick and re-seed so the picker's checkmark moves. The choice binds when
        // the first message spawns the agent.
        if self.unbound {
            self.sync_unbound_composer(cx);
            cx.notify();
            return;
        }
        self.respawn(cx);
        self.sync_composer(cx); // reflect the new model in the toolbar label
        cx.emit(AgentChatEvent::ModelChanged(model));
        cx.notify();
    }

    /// Switch the permission mode for this chat tab. Two backends, two paths:
    /// Claude fixes `--permission-mode` at spawn, so a live switch respawns resumed
    /// on the new mode; an ACP agent switches modes **in-session** via
    /// `session/set_mode`, so its `set_mode` succeeds and we skip the respawn
    /// (respawning an ACP child would drop the live session). Not persisted (see
    /// the field note). No-op when the mode is unchanged.
    fn change_permission_mode(&mut self, mode: String, cx: &mut Context<Self>) {
        // Unreachable pre-bind (the mode picker is hidden on an unbound draft),
        // but guard anyway so a stray pick can't early-spawn the subprocess.
        if self.unbound {
            return;
        }
        // The baseline ("no flag") mode comes from the backend, not a const —
        // Claude's is "default"; another provider advertises its own.
        let default_mode = self
            .connection
            .as_ref()
            .and_then(|c| c.default_mode())
            .unwrap_or_default();
        let current = self.permission_mode.clone().unwrap_or_else(|| default_mode.clone());
        if current == mode {
            return;
        }
        // Normalize the baseline to `None` so `respawn` omits the flag entirely.
        self.permission_mode = (mode != default_mode).then(|| mode.clone());
        // Prefer an in-session runtime switch (ACP); fall back to the resume-respawn
        // path when the backend can't switch live (Claude's `set_mode` bails).
        let switched_live = self
            .connection
            .as_ref()
            .is_some_and(|c| c.set_mode(&mode).is_ok());
        if !switched_live {
            self.respawn(cx);
        }
        self.sync_composer(cx); // reflect the new mode in the toolbar label
        cx.notify();
    }

    /// Switch the reasoning effort for this chat tab. `--effort` is fixed at
    /// spawn (like `--model`), so a live switch respawns resumed on the new
    /// level. Not persisted, so no host event is raised. No-op when unchanged.
    fn change_effort(&mut self, effort: String, cx: &mut Context<Self>) {
        // Unreachable pre-bind (the effort picker is hidden on an unbound draft),
        // but guard anyway so a stray pick can't early-spawn the subprocess.
        if self.unbound {
            return;
        }
        // The "current when unset" effort comes from the backend, not a const.
        let default_effort = self
            .connection
            .as_ref()
            .and_then(|c| c.default_effort())
            .unwrap_or_default();
        let current = self.effort.clone().unwrap_or(default_effort);
        if current == effort {
            return;
        }
        self.effort = Some(effort);
        self.respawn(cx);
        self.sync_composer(cx); // reflect the new effort in the toolbar label
        cx.notify();
    }

    /// Wire the owning pane group (called by the tab factory right after
    /// construction) so the `@terminal` context provider can enumerate sibling
    /// terminal tabs.
    pub fn set_pane_group(&mut self, group: WeakEntity<PaneGroup>) {
        self.pane_group = Some(group);
    }

    /// Rebuild the composer's `@`-menu context sources (`@diff`, `@clipboard`, one
    /// `@terminal` per sibling terminal tab) and push them in. Called each time the
    /// menu opens so the terminal list is live — terminals opened/closed since the
    /// last open are reflected.
    fn refresh_context_sources(&mut self, cx: &mut Context<Self>) {
        let sources = self.context_sources(cx);
        self.composer.update(cx, |c, cx| c.set_context_sources(sources, cx));
    }

    /// The context sources to offer: always `@diff` + `@clipboard`, plus one
    /// `@terminal` per sibling terminal tab in the owning group (each named by its
    /// tab title, keyed by the stable PTY session id for capture).
    fn context_sources(&self, cx: &App) -> Vec<ContextSource> {
        let mut sources = vec![ContextSource::diff(), ContextSource::clipboard()];
        let Some(group) = self.pane_group.as_ref().and_then(|w| w.upgrade()) else {
            return sources;
        };
        let group = group.read(cx);
        for (_idx, tab) in group.visible_tabs() {
            let PaneContent::Terminal(tree) = &tab.content else {
                continue;
            };
            let Some(view) = tree.active_view() else { continue };
            let tv = view.read(cx);
            let title = tab
                .custom_title
                .as_ref()
                .map(|s| s.to_string())
                .or_else(|| tv.title().map(str::to_string))
                .unwrap_or_else(|| tab.label.to_string());
            sources.push(ContextSource::terminal(tv.session_id(), &title));
        }
        sources
    }

    /// Capture a picked context provider and hand the resulting chip back to the
    /// composer. Clipboard is synchronous; diff shells out to git off-thread;
    /// terminal re-resolves the tab by its PTY id (it may have closed since the
    /// menu opened) and reads its scrollback / selection.
    fn capture_context(&mut self, request: ContextRequest, cx: &mut Context<Self>) {
        match request {
            ContextRequest::Clipboard => {
                let text = cx.read_from_clipboard().and_then(|i| i.text());
                if let Some(chip) = context_providers::clipboard_chip(text) {
                    self.composer.update(cx, |c, cx| c.add_context_chip(chip, cx));
                }
            }
            ContextRequest::Diff => self.capture_diff(cx),
            ContextRequest::Terminal { id, title } => {
                let Some(group) = self.pane_group.as_ref().and_then(|w| w.upgrade()) else {
                    return;
                };
                // Re-resolve the live view by PTY id, cloning the entity so the
                // group borrow ends before we read the terminal.
                let view: Option<Entity<TerminalView>> = {
                    let group = group.read(cx);
                    group.visible_tabs().find_map(|(_i, tab)| {
                        let PaneContent::Terminal(tree) = &tab.content else {
                            return None;
                        };
                        tree.iter_all_views()
                            .find(|(_l, _t, v)| v.read(cx).session_id() == id)
                            .map(|(_l, _t, v)| v.clone())
                    })
                };
                let Some(view) = view else { return };
                let (text, truncated) =
                    view.read(cx).capture_agent_context(context_providers::TERMINAL_MAX_LINES);
                if let Some(chip) = context_providers::terminal_chip(&title, text, truncated) {
                    self.composer.update(cx, |c, cx| c.add_context_chip(chip, cx));
                }
            }
        }
    }

    /// Shell out `git diff` + `git diff --cached` in the chat cwd off the tokio
    /// runtime (like the checkpoint engine — never `cx.background_spawn`, which has
    /// no reactor), combine + cap them into a chip, and hand it to the composer.
    fn capture_diff(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<(String, String)>();
        handle.spawn(async move {
            async fn run(cwd: &std::path::Path, extra: &[&str]) -> String {
                let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
                args.extend_from_slice(extra);
                GitCmd::new(cwd)
                    .timeout(Duration::from_secs(30))
                    .args(args)
                    .run()
                    .await
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .unwrap_or_default()
            }
            let unstaged = run(&cwd, &[]).await;
            let staged = run(&cwd, &["--cached"]).await;
            let _ = tx.send((unstaged, staged));
        });
        cx.spawn(async move |this, cx| {
            let Ok((unstaged, staged)) = rx.await else {
                return;
            };
            let chip = context_providers::diff_chip(&unstaged, &staged);
            let _ = this.update(cx, |this, cx| {
                this.composer.update(cx, |c, cx| c.add_context_chip(chip, cx));
            });
        })
        .detach();
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
        let composer = cx.new(|cx| {
            ComposerView::new(
                theme,
                density,
                typography.clone(),
                ChatBackend::stream_json().provider_display_name(),
                window,
                cx,
            )
        });
        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            backend: ChatBackend::stream_json(),
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
            // The test injects a live connection, so this chat is already bound.
            unbound: false,
            unbound_agent_id: None,
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
            pane_group: None,
            show_background_tasks: false,
            flash_entry: None,
            flash_frames: 0,
            recently_copied: None,
            _copied_clear_task: None,
            user_child_ix: RefCell::new(Vec::new()),
            entry_child_ix: RefCell::new(HashMap::new()),
            find_bar: None,
            rail_hover: false,
            menu_hover: false,
        }
    }

    /// Test-only: put this view into the unbound *New Agent* draft state (no
    /// connection, Claude picked) so a `#[gpui::test]` can drive `change_agent` /
    /// `change_model` on a draft without spawning a subprocess.
    #[cfg(test)]
    fn make_unbound_for_test(&mut self) {
        self.connection = None;
        self.unbound = true;
        self.unbound_agent_id = Some("claude-code".to_string());
        self.backend = ChatBackend::stream_json();
        self.model = Some("opus".to_string());
    }

    #[cfg(test)]
    fn backend_transport_for_test(&self) -> Transport {
        self.backend.transport
    }

    #[cfg(test)]
    fn model_for_test(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[cfg(test)]
    fn unbound_agent_id_for_test(&self) -> Option<&str> {
        self.unbound_agent_id.as_deref()
    }

    #[cfg(test)]
    fn is_bound_for_test(&self) -> bool {
        !self.unbound && self.connection.is_some()
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
        // A couple of ACP session updates drive view/host state the ChatThread
        // fold alone doesn't reach: an agent-driven mode switch must sync the
        // picker's own field; a title update rides up to the tab label.
        match &ev {
            ThreadEvent::ModeChanged { mode_id } => {
                // Normalize the backend baseline to `None` (matching the manual
                // switch path) so the picker shows the default, not a redundant
                // explicit value.
                let default_mode = self
                    .connection
                    .as_ref()
                    .and_then(|c| c.default_mode())
                    .unwrap_or_default();
                self.permission_mode = (*mode_id != default_mode).then(|| mode_id.clone());
            }
            ThreadEvent::TitleUpdated { title } => {
                cx.emit(AgentChatEvent::TitleChanged(title.clone()));
            }
            _ => {}
        }
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

    /// Jump the transcript to the `n`-th user turn (0-based ordinal among user
    /// messages) and briefly highlight it. Releases auto-follow so the jump
    /// sticks, and re-issues the scroll once next frame in case the target's
    /// markdown height is still settling. No-op if `n` is out of range or that
    /// turn wasn't rendered this frame. Shared primitive for the jump menu and
    /// (later) the message rail.
    fn scroll_to_user_ordinal(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let child_ix = match self.user_child_ix.borrow().get(n) {
            Some(&ix) => ix,
            None => return,
        };
        let Some(entry_idx) = self.thread.user_entry_index(n) else {
            return;
        };
        self.stick_to_bottom = false;
        self.list_scroll.scroll_to_item(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
        // The target's height can settle a frame late (async markdown), which
        // leaves a first scroll landing short when a long reply sits above it;
        // re-issue once on the next frame against the freshly-measured bounds.
        let this = cx.entity().downgrade();
        window.on_next_frame(move |_window, cx| {
            let _ = this.update(cx, |this, cx| {
                if let Some(&ix) = this.user_child_ix.borrow().get(n) {
                    this.list_scroll.scroll_to_item(ix);
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// Scroll so the entry at `entry_idx` is in view and briefly flash it — the
    /// find bar's jump-to-match. Window-free (a single `scroll_to_item`, no
    /// next-frame re-measure) because it's driven from the find input's change
    /// subscription, which has no `Window`. Reads the per-entry child map
    /// rebuilt each render (`entry_child_ix`).
    fn scroll_to_entry(&mut self, entry_idx: usize, cx: &mut Context<Self>) {
        let Some(&child_ix) = self.entry_child_ix.borrow().get(&entry_idx) else {
            return;
        };
        self.stick_to_bottom = false;
        self.list_scroll.scroll_to_item(child_ix);
        self.flash_entry = Some(entry_idx);
        self.flash_frames = FLASH_FRAMES;
        cx.notify();
    }

    /// The user-turn ordinal currently at (or just above) the top of the
    /// viewport — the anchor for prev/next navigation. `user_child_ix` is sorted
    /// ascending (child index grows with ordinal), so a binary search over it
    /// against the top visible child maps back to an ordinal. Returns 0 when
    /// nothing is scrolled or there are no user turns.
    fn current_user_ordinal(&self) -> usize {
        let top_child = self.list_scroll.top_item();
        match self.user_child_ix.borrow().binary_search(&top_child) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
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
        // Hover-revealed action row of minimal icon buttons (native-chat style):
        // Copy is always available; Edit / Rewind appear once this turn has a
        // session to fork (session id present) and we're not mid-rewind. Edit is
        // idle-only (a live turn would queue the resend instead of routing it);
        // Rewind cancels the turn first, so it stays available.
        let can_rewind =
            self.thread.session_id.is_some() && !self.rewinding && self.backend_supports_rewind();
        let copied = self.recently_copied == Some(idx);
        let copy_text = text.to_string();
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
                    .gap(px(2.0))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(message_action_icon(
                        SharedString::from(format!("copy-btn-{idx}")),
                        if copied { "icons/check.svg" } else { "icons/copy.svg" },
                        if copied { "Copied" } else { "Copy" },
                        if copied { theme.status_ok } else { theme.fg_muted },
                        theme,
                        cx.listener(move |this, _e, _w, cx| {
                            this.copy_message(idx, copy_text.clone(), cx);
                        }),
                    ))
                    .when(can_rewind && !self.thread.turn_active, |row| {
                        row.child(message_action_icon(
                            SharedString::from(format!("edit-btn-{idx}")),
                            "icons/pencil.svg",
                            "Edit message",
                            theme.fg_muted,
                            theme,
                            cx.listener(move |this, _e, window, cx| {
                                this.enter_pending_edit(idx, window, cx);
                            }),
                        ))
                    })
                    .when(can_rewind, |row| {
                        row.child(message_action_icon(
                            SharedString::from(format!("rewind-btn-{idx}")),
                            "icons/undo-2.svg",
                            "Rewind to here",
                            theme.fg_muted,
                            theme,
                            cx.listener(move |this, _e, _w, cx| {
                                this.open_rewind_confirm(idx, cx)
                            }),
                        ))
                    })
                    // Fork branches to a NEW tab, reading the on-disk session
                    // file directly — so it's idle-only (like Edit), whereas
                    // Rewind cancels the turn first.
                    .when(can_rewind && !self.thread.turn_active, |row| {
                        row.child(message_action_icon(
                            SharedString::from(format!("fork-btn-{idx}")),
                            "icons/git-branch.svg",
                            "Fork from here",
                            theme.fg_muted,
                            theme,
                            cx.listener(move |this, _e, _w, cx| {
                                this.request_fork(idx, cx)
                            }),
                        ))
                    }),
            );
        col.into_any_element()
    }

    /// Copy a message's text to the clipboard and flash the source bubble's copy
    /// glyph to a ✓ for a beat as confirmation. A rapid second copy replaces the
    /// prior revert timer (held in `_copied_clear_task`) so the ✓ tracks the
    /// latest copy.
    fn copy_message(&mut self, entry_idx: usize, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.recently_copied = Some(entry_idx);
        cx.notify();
        self._copied_clear_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.recently_copied == Some(entry_idx) {
                    view.recently_copied = None;
                    cx.notify();
                }
            });
        }));
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

        // Flatten the transcript: each turn is a DIRECT child of the tracked
        // scroll box (wrapped in a centered max-width column so the reading
        // measure is unchanged) rather than sharing one inner `content` column.
        // gpui records child bounds for direct children only, so this is what
        // lets `ScrollHandle::scroll_to_item` reveal an exact user turn for jump
        // navigation. The inter-turn gap moves from the old column onto the
        // scroll box; turns breathe a little more than inline content.
        let mut scroll = scroll.gap(px(density.pad_panel * 2.0));

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

        // Build each visible entry's element first, capturing per-entry flags so
        // "which scroll child is which user turn" is a pure, unit-tested function
        // (`user_turn_child_indices`) rather than logic tangled into the push loop.
        struct Row {
            entry_idx: usize,
            el: Option<AnyElement>,
            dimmed: bool,
            is_user: bool,
            expander: Option<AnyElement>,
        }
        let mut rows: Vec<Row> = Vec::with_capacity(self.thread.entries.len());
        for (idx, entry) in self.thread.entries.iter().enumerate() {
            if matches!(group_plan[idx], EntryDisplay::Hide) {
                continue;
            }
            let is_user = matches!(entry, ThreadEntry::User { .. });
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
                            // Let the column shrink to the max-width wrapper so a
                            // long markdown line wraps instead of overflowing the
                            // edge (see `bubble::assistant_body`).
                            .min_w_0()
                            .gap(px(4.0))
                            .w_full()
                            .child(assistant_header(
                                idx,
                                self.recently_copied == Some(idx),
                                // Regenerate is a constrained rewind: offer it only
                                // on a settled, resumable, connected thread AND only
                                // on a reply in the LAST turn (no user prompt after
                                // it). Regenerating an earlier reply would silently
                                // fork + drop every later turn in one click, with no
                                // confirmation — so it's restricted to the tail turn,
                                // where the only thing dropped is the reply itself.
                                !self.thread.turn_active
                                    && !self.disconnected
                                    && !self.rewinding
                                    && self.thread.session_id.is_some()
                                    && self.backend_supports_rewind()
                                    && !self.thread.entries[idx + 1..]
                                        .iter()
                                        .any(|e| matches!(e, ThreadEntry::User { .. })),
                                group,
                                &msg.text,
                                self.provider_label(),
                                theme,
                                &typo,
                                cx,
                            ));
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
                        Some(
                            tool_card::render_tool_card(
                                tc,
                                expanded,
                                self.provider_label(),
                                theme,
                                density,
                                &typo,
                                cx,
                            )
                            .into_any_element(),
                        )
                    }
                }
                ThreadEntry::ContextCompaction { summary } => {
                    Some(compaction_divider(summary, theme, &typo).into_any_element())
                }
            };
            // A collapsed tool-run expander follows its anchor entry as its own child.
            let expander = match group_plan[idx] {
                EntryDisplay::ShowThenExpander { run_start, hidden } => {
                    Some(self.render_tool_run_expander(run_start, hidden, cx))
                }
                _ => None,
            };
            let dimmed = el.is_some() && self.is_pending_edit_dimmed(idx);
            rows.push(Row { entry_idx: idx, el, dimmed, is_user, expander });
        }

        // Pure child-index map (user ordinal → scroll child index), rebuilt every
        // render and read by `scroll_to_user_ordinal` for jump nav / the rail.
        let produces: Vec<bool> = rows.iter().map(|r| r.el.is_some()).collect();
        let user_flags: Vec<bool> = rows.iter().map(|r| r.is_user).collect();
        let expander_flags: Vec<bool> = rows.iter().map(|r| r.expander.is_some()).collect();
        *self.user_child_ix.borrow_mut() =
            user_turn_child_indices(&produces, &user_flags, &expander_flags);
        // Same child accounting, but keyed by entry index across all rendered
        // entries — the find bar jumps to any matching entry, not just user turns.
        let rows_entry_idx: Vec<usize> = rows.iter().map(|r| r.entry_idx).collect();
        *self.entry_child_ix.borrow_mut() =
            entry_child_indices(&rows_entry_idx, &produces, &expander_flags).into_iter().collect();

        // Push each entry (then any trailing tool-run expander) as a DIRECT child
        // of the scroll box, in the exact order the index map counted, each in a
        // centered max-width wrapper matching the old single column. The wrapper
        // MUST be `flex().flex_col()` (not a bare block) so the max-width actually
        // caps the child — a plain block lets a wide bubble overflow past the edge.
        for row in rows {
            if let Some(el) = row.el {
                let mut wrap =
                    div().flex().flex_col().w_full().max_w(px(CONTENT_MAX_W)).child(el);
                if row.dimmed {
                    // A staged edit dims the messages it will remove on send.
                    wrap = wrap.opacity(0.4);
                }
                // A jumped-to turn briefly tints its wrapper (whole-row highlight),
                // fading with the frame counter so it settles rather than snaps.
                if self.flash_entry == Some(row.entry_idx) {
                    let a = (self.flash_frames as f32 / FLASH_FRAMES as f32).clamp(0.0, 1.0);
                    wrap = wrap
                        .rounded(px(density.r_card))
                        .bg(theme.focus_ring.opacity(0.16 * a));
                }
                scroll = scroll.child(wrap);
            }
            if let Some(expander) = row.expander {
                scroll = scroll.child(
                    div().flex().flex_col().w_full().max_w(px(CONTENT_MAX_W)).child(expander),
                );
            }
        }
        // The agent's execution plan (ACP `Plan`) as a pinned checklist at the tail
        // of the transcript — one card, full-replaced on each `PlanUpdated`, kept
        // across turns until cleared. Reuses the `TodoWrite` checklist renderer.
        if let Some(entries) = self.thread.plan.as_ref().filter(|e| !e.is_empty()) {
            scroll = scroll.child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(CONTENT_MAX_W))
                    .child(plan_panel::render_plan_entries(entries, theme, density, &typo)),
            );
        }
        // Live turn / disconnect state lives at the tail of the transcript (like
        // a native chat), NOT above the composer — so it never resizes the input.
        // These trail every user turn, so they never shift the child-index map.
        if self.disconnected {
            // A crash is terminal for this child, but the session is usually
            // resumable — offer Retry, which respawns via `--resume` then
            // re-sends the last prompt.
            let msg = self
                .thread
                .last_error
                .clone()
                .unwrap_or_else(|| "Agent process exited.".to_string());
            let retry = self.retry_button(cx);
            scroll = scroll.child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(CONTENT_MAX_W))
                    .child(error_card::error_card(&msg, theme, &typo, retry)),
            );
        } else if self.thread.turn_active {
            // While a question card is pending, the agent isn't working — it's
            // blocked on the user's answer — so don't show the "working…" spinner
            // (it would also add height that pushes the card's controls down).
            if self.thread.pending_question().is_none() {
                scroll = scroll.child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .max_w(px(CONTENT_MAX_W))
                        .child(working_indicator(self.provider_label(), theme, &typo)),
                );
            }
        } else if let Some(err) = self.thread.last_error.clone() {
            // An idle turn that ended in error: surface it inline at the tail
            // with a Retry. This is the ONLY place a failure after the first
            // message becomes visible — the empty-state hint that also renders
            // `last_error` only paints when the transcript is empty.
            let retry = self.retry_button(cx);
            scroll = scroll.child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(CONTENT_MAX_W))
                    .child(error_card::error_card(&err, theme, &typo, retry)),
            );
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
                scroll = scroll.child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .max_w(px(CONTENT_MAX_W))
                        .child(summary_line(summary, theme, &typo)),
                );
            }
            if let Some(usage) = self.thread.usage.as_ref() {
                scroll = scroll.child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .max_w(px(CONTENT_MAX_W))
                        .child(usage_footer(usage, theme, &typo)),
                );
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
        scroll = scroll.child(div().flex_none().w_full().h(tail_gap));
        // Compose the timeline row: the left tick-rail, the scrolling transcript,
        // and the top-left jump dropdown + hover preview as absolute overlays over
        // it. The `relative` row is the positioning context all three overlays
        // (and the rail's per-tick fractions) resolve against.
        div()
            .relative()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .children(self.render_message_rail(cx))
            .child(self.wrap_scroll(scroll))
            .children(self.render_jump_list(cx))
            .children(self.render_find_bar(cx))
            .into_any_element()
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
                self.thread.last_error.as_deref().unwrap_or("The agent process exited.").to_string(),
                theme.status_error,
            )
        } else {
            (
                "Start a conversation",
                format!("Ask {} to explain code, make edits, or run commands.", self.provider_label()),
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
                    .child(SharedString::from(subtitle)),
            )
            .into_any_element()
    }

    /// The Background Tasks toggle chip + inline drawer, shown once the current
    /// chat has spawned any subagent / background bash. The chip carries a
    /// running-count badge and expands a Running/Finished list above the composer.
    fn render_background_tasks(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.thread.background_tasks.is_empty() {
            return None;
        }
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let running = self.thread.running_task_count();
        let total = self.thread.background_tasks.len();
        let expanded = self.show_background_tasks;

        let label = if running > 0 {
            format!("Background tasks · {running} running")
        } else {
            format!("Background tasks · {total}")
        };
        let header = div()
            .id("bg-tasks-toggle")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .cursor_pointer()
            .text_size(px(typo.t_label_xs))
            .text_color(theme.fg_subtle)
            .hover(|s| s.text_color(theme.fg_muted))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.show_background_tasks = !this.show_background_tasks;
                    cx.notify();
                }),
            )
            .child(SharedString::from(if expanded { "▾" } else { "▸" }))
            .child(SharedString::from(label));

        let mut container = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(CONTENT_MAX_W))
            .gap(px(density.gap_inline * 0.5))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .px(px(density.pad_panel))
            .py(px(density.gap_inline))
            .child(header);
        if expanded {
            container = container.child(background_tasks_panel::render_drawer(
                &self.thread.background_tasks,
                theme,
                density,
                typo,
            ));
        }
        // Center on the reading column so it lines up with the composer + messages.
        Some(div().flex().flex_col().items_center().w_full().child(container))
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
        // Fade out the jump highlight: the tinted bubble's alpha scales with
        // `flash_frames` in `render_transcript`, so drain the counter a frame at a
        // time (forcing a re-render each step) until it clears. A jump releases
        // stick-to-bottom, so this normally runs alone; if the user scrolls back
        // to the bottom while a flash is still fading, the follow loop above
        // re-arms and both run for the remaining frames — harmless, as each
        // counter is independently bounded (no shared state, no runaway).
        if self.flash_frames > 0 {
            self.flash_frames -= 1;
            if self.flash_frames == 0 {
                self.flash_entry = None;
            }
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_window, cx| {
                let _ = this.update(cx, |_this, cx| cx.notify());
            });
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
                if this.find_bar.is_some() {
                    this.close_find(window, cx);
                    cx.stop_propagation();
                } else if this.pending_edit.is_some() {
                    this.cancel_pending_edit(window, cx);
                    cx.stop_propagation();
                }
            }))
            // Cmd+F toggles the in-transcript find bar. This listener sits on the
            // focused chat's dispatch path, so it fires (and stops propagation)
            // before the workspace-root fallback routes `Search` to the active
            // terminal's scrollback search — no collision. Toggling also gives a
            // reliable keyboard CLOSE: some macOS input methods swallow Escape
            // while a text field is focused (so the Esc-to-close below can't fire
            // for those users), but a cmd-chord always reaches the app.
            .on_action(cx.listener(|this, _: &crate::actions::Search, window, cx| {
                if this.find_bar.is_some() {
                    this.close_find(window, cx);
                } else {
                    this.open_find(window, cx);
                }
                cx.stop_propagation();
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
                // The find bar owns Enter while its input is focused: ↵ steps to
                // the next match, ⇧↵ to the previous. Otherwise route to the
                // composer as before.
                if this.find_bar_focused(window, cx) {
                    if window.modifiers().shift {
                        this.find_prev(cx);
                    } else {
                        this.find_next(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                let shift = window.modifiers().shift;
                let handled = this
                    .composer
                    .update(cx, |c, cx| c.on_enter_key(shift, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            }))
            // A focused gpui-component input dispatches its OWN `Escape`
            // (`InputEscape`), never the app-wide `DismissOverlay`, so the
            // bubble-phase DismissOverlay handler above never sees Escape while
            // the find input holds focus. Capture it here (ancestor-first, so it
            // runs before the composer's own InputEscape) and close the bar; fall
            // through otherwise so the composer keeps owning its Escape.
            .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                if this.find_bar_focused(window, cx) {
                    this.close_find(window, cx);
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
            // Background-tasks drawer (subagents + background bash) — shown once
            // the turn has spawned any; sits above the composer like the banners.
            .children(self.render_background_tasks(cx))
            // Staged-edit banner + rewind-confirm card sit just above the
            // composer while active (mutually exclusive — entering edit clears
            // any open confirm).
            .children(self.render_pending_edit_banner(window, cx))
            .children(self.render_rewind_confirm(window, cx))
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

/// A live "<provider> is working…" row shown at the tail of the transcript while
/// a turn streams — a stepped rotating spinner (the reused rail cadence: 12
/// mechanical ticks/sec) plus muted text. Keeping it here rather than above the
/// composer means the input never resizes when a turn starts or ends.
fn working_indicator(label: &str, theme: Theme, typo: &Typography) -> AnyElement {
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
                .child(SharedString::from(format!("{label} is working…"))),
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

/// The assistant caption row: the provider label ("Claude"/"Codex") on the left
/// and hover-revealed actions on the right (`group`) — the affordance-on-hover
/// pattern of a native chat. Copy copies the reply's raw markdown; Regenerate
/// (shown only on a settled, resumable thread) re-rolls the reply to the
/// preceding prompt. Built here (not `bubble`) because the clicks need a
/// `Context` listener.
#[allow(clippy::too_many_arguments)]
fn assistant_header(
    entry_idx: usize,
    copied: bool,
    can_regenerate: bool,
    group: SharedString,
    text: &str,
    provider: &str,
    theme: Theme,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let copy_text = text.to_string();
    let tip: SharedString = if copied { "Copied".into() } else { "Copy".into() };
    // A hover-revealed ghost action button (reserves its slot so the caption
    // never shifts). The trailing `child` (the glyph) is supplied per action.
    let action_slot = |id: SharedString, group: SharedString| {
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(22.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .invisible()
            .group_hover(group, |s| s.visible())
            .hover(|s| s.bg(theme.hover_overlay))
    };
    let mut actions = div().flex().flex_row().items_center().gap(px(2.0));
    if can_regenerate {
        let regen_tip: SharedString = "Regenerate".into();
        actions = actions.child(
            action_slot(SharedString::from(format!("regen-{group}")), group.clone())
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(regen_tip.clone()).build(window, cx)
                })
                .on_click(cx.listener(move |this, _e, _w, cx| this.regenerate(entry_idx, cx)))
                .child(
                    Icon::default()
                        .path("icons/refresh-cw.svg")
                        .size(px(13.0))
                        .text_color(theme.fg_subtle),
                ),
        );
    }
    actions = actions.child(
        action_slot(SharedString::from(format!("copy-{group}")), group)
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.copy_message(entry_idx, copy_text.clone(), cx);
            }))
            .child(
                Icon::default()
                    .path(if copied { "icons/check.svg" } else { "icons/copy.svg" })
                    .size(px(13.0))
                    .text_color(if copied { theme.status_ok } else { theme.fg_subtle }),
            ),
    );
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .child(bubble::role_caption(provider, theme.fg_muted, typo))
        .child(actions)
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
        block = block.child(bubble::thinking_body(idx, text, theme, density, typo));
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
    fn user_child_indices_map_ordinals_to_scroll_children() {
        // Alternating user/assistant, every entry renders a child: user turns sit
        // at even child indices.
        assert_eq!(
            user_turn_child_indices(
                &[true, true, true, true],
                &[true, false, true, false],
                &[false, false, false, false],
            ),
            vec![0, 2],
        );

        // An empty assistant renders no child, so it doesn't consume a child
        // index — the following user turn shifts up by one.
        assert_eq!(
            user_turn_child_indices(
                &[true, false, true],
                &[true, false, true],
                &[false, false, false],
            ),
            vec![0, 1],
        );

        // A collapsed tool-run expander is its own extra child pushed after its
        // anchor, so it advances the child counter without being a user turn.
        assert_eq!(
            user_turn_child_indices(
                &[true, true, true, true],
                &[true, false, false, true],
                &[false, true, false, false],
            ),
            vec![0, 4],
        );

        // No entries → no user turns.
        assert_eq!(user_turn_child_indices(&[], &[], &[]), Vec::<usize>::new());
    }

    #[test]
    fn entry_child_indices_map_every_rendered_entry() {
        // entry_idx per row, produces, has_expander. Row 1 (an empty assistant)
        // produces no child; row 2 carries a trailing expander.
        let entry_idx = [0usize, 1, 2, 3];
        let produces = [true, false, true, true];
        let has_expander = [false, false, true, false];
        // Child indices: entry0→0, entry1 skipped, entry2→1 (+expander at 2),
        // entry3→3. Keyed by entry index, not ordinal.
        let mut got = entry_child_indices(&entry_idx, &produces, &has_expander);
        got.sort();
        assert_eq!(got, vec![(0, 0), (2, 1), (3, 3)]);
        // Empty transcript → empty map.
        assert!(entry_child_indices(&[], &[], &[]).is_empty());
    }

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

    /// Regenerate stages the PRECEDING user prompt (unchanged) for re-send via
    /// the rewind machinery — the selection logic that decides *what* re-rolls.
    /// The fork/respawn half is the shared rewind path (covered elsewhere); here
    /// we assert the pick + the idle-only guard without a live async runtime.
    #[gpui::test]
    async fn regenerate_stages_preceding_user_prompt(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                // Regenerate is a rewind-gated (Claude) feature.
                Box::new(StubConnection::default().with_capabilities(
                    oximux_agents::thread::AgentCapabilities { supports_rewind: true, ..Default::default() },
                )),
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
                view.thread.session_id = Some("sid".into());
                view.thread.push_user_message("first prompt");
                view.thread.apply(&ThreadEvent::AssistantText("first reply".into()));
                view.thread.push_user_message("second prompt");
                view.thread.apply(&ThreadEvent::AssistantText("second reply".into()));
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: None,
                    usage: None,
                    is_error: false,
                });

                // Guard: while a turn is active, regenerate stages nothing.
                view.thread.turn_active = true;
                let asst_idx = view.thread.entries.len() - 1;
                view.regenerate(asst_idx, cx);
                assert!(view.rewind_then_send.is_none(), "no regenerate mid-turn");
                view.thread.turn_active = false;

                // Regenerating an EARLIER reply (the first, which has a later user
                // turn) is refused — it would silently drop the later turn.
                view.regenerate(1, cx);
                assert!(
                    view.rewind_then_send.is_none(),
                    "regenerate refuses a non-tail reply (later turns would be lost)",
                );

                // Regenerating the last reply stages its owning prompt ("second
                // prompt") unchanged for re-send — not the earlier turn.
                view.regenerate(asst_idx, cx);
                let staged =
                    view.rewind_then_send.as_ref().expect("prompt staged for re-send");
                assert_eq!(staged.0, "second prompt");
                assert!(staged.1.is_empty(), "no images on this prompt");
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
                // Edit-and-resend is a rewind-gated (Claude) feature.
                Box::new(StubConnection::default().with_capabilities(
                    oximux_agents::thread::AgentCapabilities { supports_rewind: true, ..Default::default() },
                )),
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

    /// A turn that ends in error on a NON-empty transcript records the error and
    /// stays idle — the state the tail error-card arm renders against. Retry
    /// clears the error, re-opens the turn, and re-sends the last prompt (without
    /// pushing a duplicate user bubble).
    #[gpui::test]
    async fn turn_error_surfaces_and_retry_resends_last_prompt(cx: &mut TestAppContext) {
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
                view.thread.push_user_message("do the thing");
                view.thread.apply(&ThreadEvent::TurnEnded {
                    result: Some("API error: overloaded".into()),
                    usage: None,
                    is_error: true,
                });

                // Precondition the tail error-card arm keys on: idle, connected,
                // non-empty transcript, an error recorded — and nothing sent yet.
                assert!(!view.thread.turn_active, "turn settled");
                assert!(!view.disconnected, "still connected");
                assert!(!view.thread.entries.is_empty(), "transcript non-empty");
                assert_eq!(
                    view.thread.last_error.as_deref(),
                    Some("API error: overloaded"),
                    "error recorded for the tail card",
                );
                assert!(stub_probe.sent().is_empty(), "push_user_message does not transmit");

                view.retry_last_turn(cx);
                assert!(view.thread.last_error.is_none(), "error cleared on retry");
                assert!(view.thread.turn_active, "retry re-opened the turn");
                assert_eq!(
                    view.thread.entries.len(),
                    1,
                    "retry re-sends the existing prompt, not a duplicate bubble",
                );
            })
            .expect("window update");

        let sent = stub_probe.sent();
        assert_eq!(sent.len(), 1, "exactly the retried prompt was transmitted");
        assert_eq!(sent[0]["message"]["content"], json!("do the thing"));
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

    /// An unbound *New Agent* draft switches its picked agent + model in place —
    /// rebuilding the backend transport and preselecting the new agent's default
    /// model — WITHOUT spawning a subprocess (binding waits for the first send).
    #[gpui::test]
    async fn unbound_draft_switches_agent_and_model_without_binding(cx: &mut TestAppContext) {
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
                // Drop into the draft state: Claude picked, no subprocess.
                view.make_unbound_for_test();
                assert_eq!(view.backend_transport_for_test(), Transport::StreamJson);
                assert_eq!(view.model_for_test(), Some("opus"));
                assert!(!view.is_bound_for_test(), "a draft has no connection");

                // Pick Codex: transport flips to app-server, model resets to
                // Codex's default — and still no subprocess is spawned.
                view.change_agent("codex".into(), cx);
                assert_eq!(view.backend_transport_for_test(), Transport::AppServer);
                assert_eq!(view.model_for_test(), Some("gpt-5-codex"));
                assert_eq!(view.unbound_agent_id_for_test(), Some("codex"));
                assert!(!view.is_bound_for_test(), "picking an agent must not bind");

                // Pick an ACP preset: transport becomes ACP; presets carry no
                // static model list, so the draft holds no model until bound.
                view.change_agent("opencode".into(), cx);
                assert_eq!(view.backend_transport_for_test(), Transport::Acp);
                assert_eq!(view.model_for_test(), None);
                assert!(!view.is_bound_for_test());

                // Back to Claude, then switch the model on the draft: it records
                // the pick (no respawn) and still hasn't bound.
                view.change_agent("claude-code".into(), cx);
                assert_eq!(view.model_for_test(), Some("opus"));
                view.change_model("sonnet".into(), cx);
                assert_eq!(view.model_for_test(), Some("sonnet"));
                assert!(!view.is_bound_for_test(), "a model pick on a draft must not bind");
            })
            .expect("window update");
    }
}

/// A hover-revealed icon action on a user message (Copy / Edit / Rewind).
/// Minimal ghost button — just the glyph with a soft hover wash and a tooltip
/// naming the action — matching the restrained affordances of a native chat
/// client. `icon_color` lets the caller tint it (e.g. green ✓ right after Copy).
fn message_action_icon(
    id: SharedString,
    icon_path: &'static str,
    tooltip: &'static str,
    icon_color: gpui::Hsla,
    theme: Theme,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let tip = SharedString::from(tooltip);
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .child(
            Icon::default()
                .path(icon_path)
                .size(px(14.0))
                .text_color(icon_color),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}
