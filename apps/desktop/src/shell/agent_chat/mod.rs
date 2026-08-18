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

mod apply_patch;
mod attention;
mod auth_card;
mod background_tasks_panel;
mod bubble;
mod companion_sync;
mod composer;
#[cfg(any(target_os = "macos", windows))]
pub(crate) mod computer_use;
// Where computer use does not exist, these three keep their names and answer
// accordingly — see the module's own header for why that beats cfg-ing ~35
// call sites through the transcript renderer.
#[cfg(not(any(target_os = "macos", windows)))]
mod screen_control_absent;
#[cfg(not(any(target_os = "macos", windows)))]
use screen_control_absent::{computer_use, screen_card, screen_consent};
mod composer_history;
mod acp_terminal_host;
mod context_meter;
mod dictation_history;
mod dictation_hud;
mod dictation_service;
mod dictation_ui;
mod remote_dictation;
mod dictation_waveform;
mod context_providers;
mod diff_card;
mod error_card;
mod find_bar;
mod image_attach;
mod image_cache;
mod jump_menu;
mod login_card;
mod message_rail;
mod pending_edit;
mod plan_approval_card;
mod plan_panel;
mod publish_throttle;
mod question_card;
mod remote_turn;
mod turn_summary_card;
mod rewind_menu;
mod session_persistence;
#[cfg(any(target_os = "macos", windows))]
mod screen_card;
#[cfg(any(target_os = "macos", windows))]
mod screen_consent;
mod session_detail;
mod roster;
mod slash_command_catalog;
mod slash_palette;
mod tool_bodies;
mod tool_card;
mod tool_sheet;
mod tool_grouping;

/// Install the ACP embedded-terminal host at app boot so ACP agents can drive
/// live inline terminals (re-exported for `main` to call once).
pub use acp_terminal_host::install as install_acp_terminal_host;

/// Install the process-wide voice-dictation service (controller + model
/// manager) at app boot — re-exported for `main` to call once.
pub use dictation_service::install as install_dictation_service;
pub use dictation_service::build_remote_transcriber;

/// Model-management entry points the Voice settings pane drives (the recorder
/// `start`/`stop` stay internal — they carry a `ComposerView` handle). Re-exported
/// at crate scope so `settings_modal` can reach them without the private module.
pub(crate) use dictation_service::{
    cancel_download as cancel_model_download, delete as delete_model, download as download_model,
    status as model_status,
};

/// The per-window "Listening…" HUD entity + its terminal/editor sink, plus the
/// service hooks the workspace root uses to route ⌘E into whatever text pane is
/// focused (dictation is no longer chat-only).
pub(crate) use dictation_hud::{DictationHud, HudSink};
pub(crate) use dictation_service::{
    is_active as dictation_is_active, stop as dictation_stop,
};

/// Recent-transcript store for the Voice pane's "Dictation history" card.
pub(crate) use dictation_history::{
    HistoryEntry, clear as clear_dictation_history, entries as dictation_history_entries,
    format_ts as format_history_ts,
};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context,
    Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, Image, ImageSource, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement, Render, ScrollHandle,
    SharedString,
    StatefulInteractiveElement, Styled, StyledImage as _, Subscription, Task, Transformation,
    WeakEntity, Window, div, img, percentage, px, relative,
};
use gpui_component::Icon;
use gpui_component::input::Enter as InputEnter;
use gpui_component::input::Escape as InputEscape;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::Scrollbar;

/// Max width of the reading column (transcript + composer). Wider windows keep
/// the conversation centered in a comfortable measure rather than stretching
/// text edge-to-edge — the calm, focused feel of a dedicated chat surface.
pub(super) const CONTENT_MAX_W: f32 = 720.0;

/// One direct child of the scrolling transcript: a reading-measure column,
/// `width` px wide. Callers pass [`AgentChatView::content_width`].
///
/// The **definite** width is load-bearing, and the obvious spelling
/// (`w_full().max_w(px(CONTENT_MAX_W))`) is what this must never go back to.
/// Under that spelling taffy sizes the column's height against the container's
/// *available* width and only clamps to the max-width afterwards — so a reply
/// measured across the full pane re-wraps into more lines once it is capped to
/// the reading measure, and paints those extra lines outside the height it
/// reported. The turn below then draws on top of the tail (a ~475px reply
/// reporting 400px was the observed case). Sizing the column to one already-
/// capped number keeps measure width == paint width, so a reply's box always
/// matches the text in it.
///
/// `flex().flex_col()` matters too: a bare block lets a wide bubble escape the
/// column.
fn transcript_column(width: f32) -> gpui::Div {
    div().flex().flex_col().flex_shrink_0().w(px(width))
}

/// Width of the left timeline gutter (the message tick-rail). The reading column
/// sits to its right; overlays (jump dropdown, hover preview) offset by this.
pub(super) const RAIL_W: f32 = 30.0;

/// Fixed height of an inline ACP embedded terminal inside a tool card. Bounded
/// so a live terminal can't stretch the transcript; its own scrollback scrolls
/// past the cap.
const EMBEDDED_TERMINAL_HEIGHT: f32 = 260.0;

/// Synthetic `embedded_terminals` key for the ACP auth login terminal — not a
/// tool-call id, so it can't collide with one; lets the auth card reuse the same
/// mount/reap machinery as tool-call terminals.
const AUTH_TERMINAL_KEY: &str = "__acp_auth_terminal__";

/// How many frames to keep re-pinning the transcript to the bottom after a
/// content change (see [`AgentChatView::follow_frames`]). ~10 frames (≈160ms at
/// 60fps) comfortably outlasts the async markdown parse/layout of a normal reply
/// so the follow catches the message's settled height, then stops (no idle spin).
const FOLLOW_FRAMES: u8 = 10;

/// Shortest gap between transcript repaints while only streamed deltas are
/// arriving (see [`AgentChatView::notify_throttled`]). Each repaint re-parses
/// the whole growing markdown body, so an unthrottled fast model costs one full
/// re-parse per token. 50 ms caps that at ~20 repaints/sec — well under a
/// 60fps frame budget, and far faster than anyone reads, so streaming still
/// looks continuous. Only deltas are throttled; anything the user can act on
/// paints immediately.
const NOTIFY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How many frames the jumped-to-message highlight lingers before it clears.
/// ~48 frames (≈0.8s at 60fps) — long enough to catch the eye after a jump,
/// short enough not to distract. The tint alpha scales with the remaining
/// frames so it fades out rather than snapping off.
const FLASH_FRAMES: u8 = 48;

/// Give the composer's palette the metadata for `connection`'s commands:
/// descriptions, grouping, and attribution (the advertised list is bare names).
///
/// A backend that describes its own commands is taken at its word. Only one that
/// can't gets the on-disk scan, which reads a specific CLI's config directories
/// and so is only meaningful for the CLI it models — pointing it at another
/// agent's commands would attribute them to whatever file happened to share a
/// name. The scan reads ~100 small files, so it runs off the main thread and
/// pushes its result in when ready.
///
/// Called for every connection, not just the one a chat is constructed with: a
/// *New Agent* draft has no connection until its first send, so seeding this at
/// construction alone left every deferred-bound chat with names but no metadata
/// — every row filed under "Built-in", undescribed. Caught by driving the app.
fn push_slash_catalog(
    connection: Option<&dyn AgentConnection>,
    composer: &Entity<ComposerView>,
    cwd: &std::path::Path,
    cx: &mut Context<AgentChatView>,
) {
    let Some(conn) = connection else { return };
    if !conn.capabilities().supports_slash {
        return;
    }
    let advertised = conn.slash_commands();
    if !advertised.is_empty() {
        let catalog = slash_command_catalog::catalog_from_backend(&advertised);
        composer.update(cx, |c, cx| c.set_command_catalog(catalog, cx));
        return;
    }
    let scan_cwd = cwd.to_path_buf();
    cx.spawn(async move |this, cx| {
        let catalog = cx
            .background_spawn(async move { slash_command_catalog::discover_catalog(&scan_cwd) })
            .await;
        let _ = this.update(cx, |this, cx| {
            this.composer.update(cx, |c, cx| c.set_command_catalog(catalog, cx));
        });
    })
    .detach();
}

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
            features: c.features(),
            default_model: c.default_model(),
            default_mode: c.default_mode(),
            default_effort: c.default_effort(),
        },
        None => ControlVocab::default(),
    }
}

pub use session_persistence::RestoredPosture;
use session_persistence::{seed_posture_feature_values, ConnectMode};


/// Overlay the user's optimistic feature picks onto the backend-advertised
/// feature list so the composer reflects a toggle/select change immediately —
/// mirroring how `model`/`effort` hold the pick — rather than waiting for the
/// backend to echo the new value (some ACP agents apply `set_config` silently).
fn apply_feature_overrides(
    features: &mut [FeatureControl],
    overrides: &HashMap<String, FeatureValue>,
) {
    for f in features.iter_mut() {
        match (overrides.get(&f.id), &mut f.kind) {
            (Some(FeatureValue::Bool(b)), FeatureKind::Toggle { on }) => *on = *b,
            (Some(FeatureValue::Choice(c)), FeatureKind::Select { selected, .. }) => {
                *selected = Some(c.clone());
            }
            _ => {}
        }
    }
}

/// One dynamic agent's pre-bind catalog-probe state, cached per adapter id on
/// the draft. `Loading` while the off-thread probe runs, `Ready` with the fetched
/// models once it lands, `Failed` when the agent couldn't be probed (not
/// installed, auth needed, timeout) — `Failed`/`Loading` both leave the model
/// picker hidden, matching an agent that advertises no models.
enum ProbeState {
    Loading,
    Ready(ProbedCatalog),
    Failed,
}

/// Fold a completed catalog probe into the next per-view [`ProbeState`] and the
/// catalog worth caching, if any. Crucially it preserves an already-good seed (a
/// non-empty `Ready`, e.g. one painted from the disk cache): an empty success or
/// a failure on *revalidation* returns `None` for the state — "keep what's
/// shown" — so a transient/empty re-probe never blanks the picker out from under
/// a mid-draft user. Only a non-empty success is returned for caching.
fn fold_probe_result(
    has_good_seed: bool,
    result: anyhow::Result<ProbedCatalog>,
) -> (Option<ProbeState>, Option<ProbedCatalog>) {
    match result {
        // A real catalog: adopt it and hand it back to warm the shared cache.
        Ok(catalog) if !catalog.models.is_empty() => {
            (Some(ProbeState::Ready(catalog.clone())), Some(catalog))
        }
        // Empty success: adopt it (an agent with genuinely no models) only when
        // there was nothing good to show; otherwise keep the seed. Never cached.
        Ok(catalog) => ((!has_good_seed).then_some(ProbeState::Ready(catalog)), None),
        // Failure: mark `Failed` only on a true miss; keep a seed otherwise.
        Err(_) => ((!has_good_seed).then_some(ProbeState::Failed), None),
    }
}

/// Decoded user-attached image thumbnails, memoized by `(entry index, image
/// index)`. Interior-mutable so the immutable `render` path can fill it lazily.
use image_cache::ImageCache;

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
        session_meta: oximux_agents::thread::SessionMeta,
        thinking_level: ThinkingLevel,
    },
    /// The signed-out banner's "Open terminal to sign in" control was clicked;
    /// the host should spawn a terminal tab running this agent's interactive CLI
    /// at `cwd` so the user can run `/login`. Carries the CLI adapter id so the
    /// host picks the right binary.
    OpenLoginTerminalRequested { adapter_id: &'static str, cwd: PathBuf },
    /// A *New Agent* draft with the worktree toggle armed just sent its first
    /// message: the leaf has no `WorkspaceRepo`, so it asks the host to create a
    /// fresh worktree **as a first-class `Workspace`** (DB row + git worktree via
    /// `create_workspace_with_rollback`). The host dispatches
    /// `CreateWorktreeWorkspaceForActiveChat`, which routes up to `WorkspaceRoot`;
    /// the outcome comes back through [`AgentChatView::on_worktree_create_outcome`],
    /// which rebinds this draft's cwd and resumes the staged send.
    WorktreeWorkspaceRequested { slug: String },
    /// An import-bridge tab's "Resume in terminal" control was clicked: the host
    /// should spawn the provider's own PTY resume (via `ResumeAgentSession` →
    /// `import_resume_command`). Routed as an event (not a direct
    /// `window.dispatch_action` from the render closure, which does not reach the
    /// host's action handlers) — the same seam `OpenLoginTerminalRequested` uses.
    ResumeInTerminalRequested {
        preset_id: String,
        resume_handle: String,
        session_id: String,
        cwd: PathBuf,
    },
    /// A turn-end card's "Review" was clicked: the host should open the turn's
    /// accumulated diff in a `DiffView` (via `load_virtual` — the diff is here,
    /// not in the repo). Routed as an event for the same reason the seams above
    /// are: a `window.dispatch_action` from a render closure does not reach the
    /// host's action handlers. `key` makes each turn its own tab.
    ReviewTurnDiffRequested { key: String, diff: String },
    /// A live chat turn reached a state the user should be told about while they
    /// may be looking elsewhere — the turn finished / errored, or it paused on a
    /// permission / question / auth prompt. The host (which owns the notifier +
    /// the window-active / visible-tab context) decides whether to raise a
    /// desktop notification + dock badge, applying the shared notification gates.
    /// Emitted only for LIVE events (a restored transcript seeds entries directly,
    /// never through the event path), so it never fires during restore replay.
    AttentionNeeded {
        kind: crate::notifier::NotificationKind,
        /// Short context for the banner body (tool name, error head); may be empty.
        body: String,
    },
}

/// Which surface an agent-chat tab is showing: its structured chat, or a
/// companion raw-PTY terminal running the same agent session resumed
/// interactively (`claude --resume <id>`). The companion is spawned lazily on
/// the first switch and reaped when the tab closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatViewMode {
    #[default]
    Chat,
    Terminal,
}

/// Everything the host needs to spawn a companion terminal that resumes THIS
/// chat's session interactively. Returned by [`AgentChatView::terminal_launch_spec`]
/// (which the host reads because only it owns the `CliRuntime` that spawns).
#[derive(Debug, Clone)]
pub struct ChatTerminalSpec {
    pub adapter: AgentAdapter,
    pub adapter_id: &'static str,
    /// The chat's session id to `--resume` into the interactive CLI.
    pub session_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Why the "Switch to Terminal View" affordance is (un)available for a chat —
/// distinguishing the two "unavailable" reasons so the hint isn't misleading.
/// A bound ACP chat that HAS sent a message is `NoInteractiveResume`, not
/// `NoSessionYet` — telling that user to "send a message first" is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAvailability {
    /// A companion terminal can be spawned (bound, has a session, resumable CLI).
    Available,
    /// No session yet (unbound draft, or never sent) — send a message first.
    NoSessionYet,
    /// Bound + has a session, but this agent has no interactive resume CLI wired
    /// (the ACP presets today). Sending another message won't help.
    NoInteractiveResume,
}

/// Whether an ACP-agent-supplied session id is safe to splice into an
/// interactive-resume command's argv. The id is an EXTERNAL string minted by the
/// agent (`acp/worker.rs`), so it is validated before it ever reaches a spawned
/// process: non-empty, no leading `-` (so it can't be parsed as a flag), and
/// only `[A-Za-z0-9_-]` (opencode ids look like `ses_0aea7d2e3ffe…`). A reject
/// leaves the companion-terminal toggle disabled rather than passing an
/// attacker-influenceable token to a CLI.
fn is_safe_resume_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
use tool_grouping::{
    must_stay_visible, plan_tool_grouping, summarize_tool_run, EntryDisplay, GroupSummary,
    GroupedTool,
};
use crate::remote_control::{RemoteBinding, RemoteControl, remote_session_id_for};
use attention::attention_for_event;
use computer_use::ScreenControl;
pub use computer_use::clear_stale_screen_control_grants;
use screen_consent::ScreenPrompt;
use oximux_agents::session_registry::{ChoiceKind, RemoteChoice, SessionMeta};
use crate::shell::context_env::SurfaceIds;
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_view::TerminalView;
use oximux_agents::thread::pi::posture::{self as pi_posture, PiPosture};
use oximux_agents::thread::{
    probe_catalog, AgentConnection, AssistantMessage, AuthMethodKind, ChatBackend, ChatImage,
    ChatThread, ConnectSpec, FeatureControl, FeatureKind, FeatureValue, PermissionDecision,
    PermissionSuggestion, ProbedCatalog, QuestionAnswers, QuestionRequest, ThreadEntry,
    ThreadEvent, ToolCall, ToolCallStatus, ToolDetail, Transport, TurnUsage,
};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_git::GitCmd;
use oximux_settings::{AgentLaunchSettings, Density, Theme, Typography};

/// A transcript-only **import bridge**: an OpenCode / Pi session opened as a
/// chat tab for its history, with NO live connection (these providers have no
/// in-app chat backend). The composer is swapped for a "Resume in terminal"
/// action that re-dispatches the provider's own PTY resume via
/// [`crate::actions::ResumeAgentSession`]. Mirrors the terminal-resume bridge
/// pattern: read the past turns here, continue the session in a terminal.
#[derive(Clone, Debug)]
pub struct ImportBridge {
    /// Import-provider preset id (`opencode`/`pi`) — routes the resume dispatch.
    pub preset_id: String,
    /// The session id the row was scanned under (OpenCode/Pi).
    pub session_id: String,
    /// The provider's native resume handle (OpenCode session id / Pi rollout
    /// path) fed to `import_resume_command`.
    pub resume_handle: String,
    /// Where the terminal resume should root.
    pub cwd: PathBuf,
    /// Human provider label for the footer note ("imported OpenCode session…").
    pub provider_display: String,
}

pub struct AgentChatView {
    /// The conversation model. Owned directly (not a nested entity) — the view
    /// is its sole mutator, on the foreground thread.
    thread: ChatThread,
    /// The live agent connection. `None` if the subprocess failed to spawn (a
    /// read-only error state) or after teardown. Shared as an `Arc` so the
    /// session registry can hold the same connection and command it off-thread.
    connection: Option<Arc<dyn AgentConnection>>,
    /// Stable id this session is exposed under to remote (phone) clients. Minted
    /// once per view and kept across respawns, decoupled from the agent's own
    /// (maybe-not-yet-known) session id — remote just needs a key stable for the
    /// view's lifetime.
    remote_session_id: String,
    /// The live tie into the remote-control [`SessionRegistry`], or `None` when
    /// remote control is disabled (the common case → zero per-event cost). `Some`
    /// only while a connection is bound and remote is enabled; each `ThreadEvent`
    /// is teed through it in [`Self::apply_batch`].
    remote: Option<RemoteBinding>,
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
    /// Whether the session-detail popover is open (see [`session_detail`]).
    session_detail_open: bool,
    /// When the transcript last repainted, used to rate-limit streaming
    /// repaints (see [`Self::notify_throttled`]).
    last_notify: std::time::Instant,
    /// Whether a trailing repaint is already queued for deltas applied since
    /// [`Self::last_notify`]. Guards against stacking one timer per delta, and
    /// is cleared by any repaint so a timer that fires after an immediate paint
    /// becomes a no-op.
    flush_scheduled: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// This chat's screen-control identity and the targets it may drive. Held
    /// per view so two chats can never be confused for one another, and dropped
    /// with the view, which releases its grants.
    screen_control: ScreenControl,
    /// Who a pending screen-control card is asking about, keyed by tool-call id.
    /// Resolved once when the card goes up (it costs a `codesign` spawn) and
    /// dropped when the card is answered.
    screen_prompts: HashMap<String, ScreenPrompt>,
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
    /// Optimistic user picks for generic feature controls, keyed by option id.
    /// Overlaid onto the backend-advertised feature list on every `sync_composer`
    /// so a toggle/select reflects immediately (mirroring how `model`/`effort`
    /// hold the pick), instead of waiting for the backend to echo the new value —
    /// some ACP agents apply `set_config` without echoing it back.
    feature_values: HashMap<String, oximux_agents::thread::FeatureValue>,
    /// Set once the event channel closes (process exit / EOF). Disables sending.
    disconnected: bool,
    /// True after the user pressed Stop: the turn was interrupted and the child
    /// exited, but the session is **resumable** — the next send transparently
    /// respawns with `--resume`. Distinct from `disconnected` (an unexpected
    /// crash, which stays unavailable), so an intentional Stop shows no error.
    interrupted: bool,
    /// A restored chat that has not spawned its subprocess yet. Restoring a
    /// layout with many chat tabs must not launch one agent CLI per tab — a
    /// resumed CLI re-reads its whole session file, so a boot with several
    /// large sessions saturates the machine for tens of seconds. Instead the
    /// view comes up resumable-idle and connects on its first render (only
    /// the visible tab renders) or on an explicit remote open. Cleared by
    /// [`Self::ensure_connected`]; `respawn` owns the actual connect.
    dormant: bool,
    /// Leading-edge throttle for the remote transcript snapshot. Sits beside
    /// `last_saved_revision` because it answers the same shape of question for
    /// the other O(transcript) publisher — but by coalescing rather than by
    /// comparing revisions, which cannot skip here (see `publish_throttle`).
    publish_throttle: publish_throttle::PublishThrottle,
    /// The [`ChatThread::revision`] the last committed save persisted;
    /// inequality with the live counter means the on-disk blob is stale.
    /// Set only by [`Self::commit_transcript_save`] after a write succeeds;
    /// `u64::MAX` = never saved. `Cell`: the save path reads via `Entity::read`.
    last_saved_revision: std::cell::Cell<u64>,
    /// Blob fields held on the VIEW (permission mode, thinking level,
    /// posture picks) sit outside the thread's revision counter — their few
    /// setters mark this instead. Cleared with `last_saved_revision`.
    meta_dirty: std::cell::Cell<bool>,
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
    /// Pre-bind model catalogs for dynamic-model agents (Codex/ACP), keyed by
    /// adapter id, so the *New Agent* draft can offer a real model picker before
    /// the user commits. A throwaway probe (spawn → read `model/list` / session
    /// config → drop) fills these off-thread on agent pick; Claude isn't here
    /// (its models are static in the roster). Only consulted while `unbound`.
    probed_catalogs: HashMap<String, ProbeState>,
    /// Whether picking an agent may run a *live* catalog probe — i.e. spawn the
    /// real agent binary on a throwaway thread. True for every real view; false
    /// for the test constructor, which injects a `StubConnection` precisely so no
    /// subprocess is spawned. Without this seam `change_agent` reaches straight
    /// past the injected connection to the real binary: the probe thread is a raw
    /// `std::thread::spawn` that outlives the `#[gpui::test]` scheduler, so its
    /// completion lands during a LATER test and gpui aborts the process for
    /// non-determinism. The probe's result is discarded in that state anyway — a
    /// draft on a stub has no real catalog to show.
    probe_catalogs_live: bool,
    /// Whether the tab shows the chat or its companion terminal.
    view_mode: ChatViewMode,
    /// Companion interactive terminal — the same agent session resumed in a raw
    /// PTY (`--resume`), spawned lazily on the first switch to Terminal view.
    /// `None` until then; the chat process keeps running independently underneath.
    terminal: Option<Entity<TerminalView>>,
    /// The daemon session id of the companion terminal, kept so the host can reap
    /// it (`runtime.cancel`) when the tab closes — else switching to terminal view
    /// then closing the tab would orphan a live CLI. `None` when no companion.
    companion_session: Option<AgentSessionId>,
    /// The chat sent a prompt after the companion spawned — its CLI loaded the
    /// session at spawn and is now missing turns; see `companion_sync`.
    chat_advanced_since_companion: bool,
    /// Repaints this view when the companion terminal notifies (PTY output /
    /// scroll). Held alongside `terminal`; dropped when the companion is dropped.
    _terminal_observer: Option<Subscription>,
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
    /// When `Some(tool_call_id)`, a fullscreen tool-payload sheet is open on that
    /// tool call — a large diff / shell output / read slice rendered full-height
    /// and scrollable (virtualized for diffs). The backdrop / ✕ / Esc clears it;
    /// the sheet reads the tool call live from the thread each render, so a
    /// still-running tool grows in the sheet. Held as an id (not an index) so it
    /// survives transcript growth.
    open_tool_sheet: Option<String>,
    /// True for a beat after the tool sheet's Copy button fires, flashing the
    /// control to "Copied ✓". Cleared by a short timer ([`Self::_sheet_copy_task`]).
    sheet_copied: bool,
    /// Revert timer for [`Self::sheet_copied`]; a rapid second copy replaces it.
    _sheet_copy_task: Option<Task<()>>,
    /// Foreground event-drain task. Dropping it only cancels the *foreground*
    /// half at its next await point — it does NOT stop the forwarder/reader OS
    /// threads or reap the subprocess. Subprocess + thread teardown is owned by
    /// `Drop::shutdown()` (which kills the child → stdout EOF → both threads
    /// unwind). Keep that the single cleanup owner across future refactors.
    _drain_task: Option<Task<()>>,
    /// Relays remotely-injected prompts (phone sends) into this tab's transcript so
    /// the desktop shows the user's own bubble. Re-created on each (re)bind; dropping
    /// it ends the relay.
    _remote_prompt_task: Option<Task<()>>,
    /// Drains model/mode changes relayed from a remote picker. Held for its
    /// lifetime like the prompt relay — dropping it ends the drain. Started once
    /// and never replaced (see [`AgentChatView::choice_relay_sender`]).
    _remote_choice_task: Option<Task<()>>,
    /// The live end of that relay, handed to each binding. Kept so a rebind can
    /// re-register it without disturbing the task.
    remote_choice_tx: Option<futures::channel::mpsc::UnboundedSender<RemoteChoice>>,
    _subscriptions: Vec<Subscription>,
    /// Interactive AskUserQuestion cards for tool calls awaiting answers, keyed
    /// by tool-call id. Each is its own entity so its text inputs repaint without
    /// rebuilding the transcript; reconciled from the thread each render.
    question_cards: HashMap<String, Entity<QuestionCard>>,
    /// Event subscriptions for the cards above, kept alive alongside each card.
    question_card_subs: HashMap<String, Subscription>,
    /// Live inline terminals for ACP tool calls that embed one
    /// (`ToolCallContent::Terminal`), keyed by **tool-call id**. The value pairs
    /// the host's **terminal id** (a distinct id-space — the client-minted
    /// `acp-term-N` the host registry is keyed by) with the `TerminalView`
    /// mounted on that PTY. The terminal id is retained so reaping releases the
    /// host entry with the id it actually stored, not the tool id. Reconciled
    /// from the thread each render; reaped on tab close.
    embedded_terminals: HashMap<String, (String, Entity<TerminalView>)>,
    /// Repaint observers for the terminals above, one per mounted terminal.
    embedded_terminal_subs: HashMap<String, Subscription>,
    /// A pending ACP auth prompt (agent needs login), folded from
    /// `ThreadEvent::AuthRequired`. `None` when the session is authenticated;
    /// cleared on `SessionInit`. Ephemeral — never persisted.
    auth: Option<auth_card::AuthPrompt>,
    /// Masked secret fields for an EnvVar-kind auth method, one per advertised
    /// variable (`(VAR_NAME, input)`), reconciled from `auth` in `render` (which
    /// owns the `Window` `InputState::new` needs). The typed values live ONLY here
    /// and in the respawn's in-flight `ConnectSpec.env` — never persisted to the
    /// transcript blob. Empty whenever the card isn't an EnvVar prompt.
    env_inputs: Vec<(String, Entity<gpui_component::input::InputState>)>,
    /// Repaint observers for the env inputs above, one per field.
    env_input_subs: Vec<Subscription>,
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
    /// Whether a one-shot LLM tab title has already been generated (or is being
    /// generated) for this chat — guards a single fire per session. Set true by a
    /// native ACP `TitleUpdated` too, so a provider title always wins. In-memory
    /// only (a restored chat with history initializes this true; see the trigger).
    title_generated: bool,
    /// The in-flight title-generation task, owned so a tab close drops it (which
    /// prevents an emit into a dead view). `None` when idle.
    title_task: Option<Task<()>>,
    /// Weak handle to the owning pane group, set after construction by the tab
    /// factory. Lets the `@terminal` context provider enumerate sibling terminal
    /// tabs and pull their scrollback. Weak so it never keeps the group alive (the
    /// group already owns this view's `Entity`). `None` in tests / standalone use,
    /// which simply omits the terminal sources.
    pane_group: Option<WeakEntity<PaneGroup>>,
    /// The visible tab title the desktop shows for this chat — a user's manual
    /// rename, else the running `Chat N` / agent label — mirrored here by the
    /// owning pane group so the remote session list renders the same name rather
    /// than the raw `agent-N` id. `None` until the pane group first syncs it (or
    /// in standalone use), where [`Self::publish_remote_meta`] falls back to the
    /// thread's provider-native title.
    remote_tab_title: Option<String>,
    /// True when `cwd` sits inside a git repo, checked once at construction via
    /// a cheap `.git` stat (the same heuristic `workspaces_with_primary_for`
    /// uses for the synthesized primary-row check). Gates the *New Agent*
    /// draft's "Run in a fresh worktree" toggle — hidden entirely for a
    /// non-git project, since `git worktree add` can't possibly work there.
    is_git_project: bool,
    /// While `unbound`: the user has opted into running this draft's first send
    /// inside a freshly created git worktree (branch `oximux/<slug>`) instead of
    /// the project root. Cleared once the worktree exists (or the user opts out
    /// via the failure banner's "continue without a worktree" fallback).
    worktree_draft_enabled: bool,
    /// Slug text input for the worktree toggle, created lazily when the toggle
    /// turns on and dropped when it turns off (mirrors `env_inputs`'s
    /// create-on-demand pattern). `None` while the toggle is off.
    worktree_slug_input: Option<Entity<InputState>>,
    /// Repaints the chat on every keystroke in the slug field so the live
    /// `oximux/<slug>` / validation-error preview stays in sync.
    _worktree_slug_sub: Option<Subscription>,
    /// State of the last worktree-create attempt for this draft.
    worktree_create_state: roster::WorktreeCreateState,
    /// The message staged while a worktree create is in flight (or has
    /// failed) — resent automatically on success, or via the failure banner's
    /// "continue without a worktree" fallback. Cleared once actually sent.
    pending_worktree_send: Option<(String, Vec<ChatImage>)>,
    /// `oximux/<slug>` once the worktree exists, folded into the post-bind tab
    /// label (see `bind_now`) so the tab shows the branch, not just the
    /// picked agent's name.
    worktree_branch_label: Option<String>,
    /// `Some` for an OpenCode / Pi **import bridge** tab: transcript seeded, no
    /// live connection, composer swapped for Resume-in-terminal. Gated strictly
    /// so it never touches the live chat paths (send / respawn are no-ops).
    import_bridge: Option<ImportBridge>,
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
    /// The key this view is registered under in the remote-control
    /// [`SessionRegistry`].
    ///
    /// Exposed for the remote *launch* path, which has to answer a phone with
    /// the id of the session it just asked for. It is available the instant the
    /// view exists — the id is a process-local counter minted in
    /// [`Self::new`], not something the backend hands back — so a caller can
    /// read it without waiting for a connection.
    pub fn remote_session_id(&self) -> &str {
        &self.remote_session_id
    }

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
            ConnectMode::Connect,
            RestoredPosture::default(),
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
        let mut view = Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            ConnectMode::UnboundDraft,
            RestoredPosture::default(),
            theme,
            density,
            typography,
            window,
            cx,
        );
        // Seed the composer's agent + model pickers from the chat roster so the
        // draft offers the choice on its first paint (before any subprocess). If
        // the seed agent is dynamic-model (unusual — the entry defaults to Claude),
        // kick its catalog probe so the picker still fills.
        view.sync_unbound_composer(cx);
        if let Some(id) = view.unbound_agent_id.clone() {
            view.maybe_probe_catalog(id, cx);
        }
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
        session_meta: oximux_agents::thread::SessionMeta,
        thinking_level: ThinkingLevel,
        posture: RestoredPosture,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut thread = ChatThread::rehydrated(session_id, model.clone(), entries, slash_commands);
        // Seeded from the blob so the session-detail popover is populated on a
        // restored chat; a later live init overwrites it.
        thread.session_meta = session_meta;
        // Dormant: a restored chat spawns NO subprocess at construction (a
        // resumed CLI re-reads its whole session file — a layout with many
        // chat tabs would cold-start them all at boot). First render or a
        // remote open connects via `ensure_connected` → `--resume`.
        let mut view = Self::assemble(
            cwd,
            model,
            backend,
            thread,
            ConnectMode::DormantResume,
            posture,
            theme,
            density,
            typography,
            window,
            cx,
        );
        // The blob this view was built from IS the on-disk state — a save
        // before any mutation must skip it.
        view.last_saved_revision.set(view.thread.revision());
        view.thinking_level = thinking_level;
        // A resumed chat that already has history must NOT regenerate (or
        // overwrite) its label on the next send — mark it already-titled.
        view.title_generated = !view.thread.entries.is_empty();
        view
    }

    /// Construct a transcript-only **import bridge** for an OpenCode / Pi
    /// session: seed the transcript, but spawn NO subprocess
    /// ([`ConnectMode::ImportBridge`]) — these providers have no in-app chat
    /// backend. Unlike a *New
    /// Agent* draft (also connection-less), this is not `unbound`: the composer
    /// is swapped for a Resume-in-terminal action ([`Self::import_bridge`]), and
    /// `send_text` is a no-op, so it can never masquerade as a live chat.
    #[allow(clippy::too_many_arguments)]
    pub fn new_import_bridge(
        cwd: PathBuf,
        entries: Vec<ThreadEntry>,
        bridge: ImportBridge,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Seed the transcript with no session id (no `--resume` this view can
        // drive) and the Claude backend as an inert placeholder — it's never
        // connected. The placeholder must not leak into the UI:
        // `provider_label` reads the bridge's own name, so bubbles are
        // captioned with the provider the transcript actually came from.
        let thread = ChatThread::rehydrated(None, None, entries, Vec::new());
        let mut view = Self::assemble(
            cwd,
            None,
            ChatBackend::stream_json(),
            thread,
            ConnectMode::ImportBridge,
            RestoredPosture::default(),
            theme,
            density,
            typography,
            window,
            cx,
        );
        view.title_generated = true;
        view.import_bridge = Some(bridge);
        view
    }

    /// Shared construction for every chat-view flavor: wire the composer,
    /// spawn the subprocess when the mode says so (resuming when
    /// `thread.session_id` is set), and start the event drain. A spawn
    /// failure degrades to a read-only error state so the tab still opens
    /// and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        mut thread: ChatThread,
        mode: ConnectMode,
        // A restored chat's persisted, backend-specific posture, seeded into the
        // connection spawn + the composer's feature picks so the reopened session
        // keeps the choice it was saved with. Empty for fresh launches.
        posture: RestoredPosture,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Cheap, sync `.git` stat (not an async `Repository::open`) — this
        // only gates whether the *New Agent* draft's worktree toggle renders
        // at all, run on every construction so it's ready before first paint.
        let is_git_project = cwd.join(".git").exists();
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
        // `subscribe_in` rather than `subscribe`: the worktree pick has to create
        // or drop the slug `InputState`, which needs a `Window`. Every other arm
        // ignores it.
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
                ComposerEvent::SteerNow { text } => this.steer_text(text.clone(), cx),
                ComposerEvent::Stop => this.stop_turn(cx),
                ComposerEvent::ModelPicked(model) => this.change_model(model.clone(), cx),
                ComposerEvent::PermissionModePicked(mode) => {
                    this.change_permission_mode(mode.clone(), cx)
                }
                ComposerEvent::EffortPicked(effort) => this.change_effort(effort.clone(), cx),
                ComposerEvent::FeaturePicked { id, value } => {
                    this.change_feature(id.clone(), value.clone(), cx)
                }
                ComposerEvent::AgentPicked(id) => this.change_agent(id.clone(), cx),
                ComposerEvent::WorktreeIsolationPicked(enabled) => {
                    this.set_worktree_isolation(*enabled, window, cx)
                }
                ComposerEvent::MentionOpened => this.refresh_context_sources(cx),
                ComposerEvent::CaptureContext(request) => {
                    this.capture_context(request.clone(), cx)
                }
            },
        )];

        // A resumed thread carries the prior session id; a fresh one is `None`
        // (spawn a new session). Either way the subprocess is spawned the same.
        let resume_session_id = thread.session_id.clone();
        let mut connection: Option<Arc<dyn AgentConnection>> = None;
        let mut disconnected = false;
        let mut drain_task = None;
        let screen_control = ScreenControl::new(&cwd);
        // A fresh/restored session always starts in the default permission mode
        // (see the `permission_mode` field note); a live switch respawns.
        // Only an eager chat spawns here. An unbound draft binds via
        // `respawn()` on the first send; a dormant restore connects on first
        // render / remote open; a bridge never connects.
        if mode == ConnectMode::Connect {
            let mut spec = ConnectSpec::for_backend(
                &backend,
                cwd.clone(),
                model.clone(),
                resume_session_id.clone(),
                None,
                None,
            );
            // A restored chat resumes under its persisted posture. For Pi this is
            // the only tool gate there is, so losing it would silently widen the
            // session to the (permissive) default.
            spec.codex_posture = posture.codex.clone();
            spec.pi_posture = posture.pi.clone();
            match computer_use::connect_declaring(spec, &screen_control, cx) {
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

        // Seed the context meter from the backend when it knows its window
        // without having run a turn (Pi reports it per model at the handshake).
        // Without this the meter has no denominator until the first reply lands.
        // `.or()` keeps a restored transcript's cached window when the backend
        // has nothing to offer.
        thread.last_known_context_window =
            connection.as_ref().and_then(|c| c.context_window()).or(thread.last_known_context_window);

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
        // A restored bound chat with a session can open its terminal view right
        composer.update(cx, |c, cx| {
            c.set_state(disconnected, thread.turn_active, cx);
            c.set_can_steer(caps.supports_steer, cx);
            c.set_controls(model.clone(), None, None, caps.supports_modes, caps.supports_config, vocab, cx);
            // Descriptions + hints aren't persisted — a restored session shows
            // names only until the live agent re-advertises via SlashCommandsUpdated.
            c.set_slash_commands(
                seed_slash,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                cx,
            );
            c.seed_history(history_seed);
        });

        push_slash_catalog(connection.as_deref(), &composer, &cwd, cx);

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
        let unbound_agent_id = (mode == ConnectMode::UnboundDraft).then(|| {
            match backend.transport {
                Transport::StreamJson => "claude-code",
                Transport::AppServer => "codex",
                Transport::Acp => "cursor",
                Transport::Rpc => "pi",
            }
            .to_string()
        });

        // Keyed on the agent's own session id when the thread has one — a
        // restored chat always does — so a remote client's reference to this
        // session survives the restart that just rebuilt this view.
        let remote_session_id = remote_session_id_for(thread.session_id.as_deref());
        // Bind this session into the remote-control registry now if remote control
        // is enabled (else `None` — nothing registered, nothing teed).
        let remote = connection.clone().and_then(|conn| {
            cx.try_global::<RemoteControl>().and_then(|rc| rc.bind(&remote_session_id, conn))
        });
        // A restored transcript loads at construction (never through the live event
        // drain that publishes for a running session), so publish its meta + folded
        // history now. Without this a remote client opening an idle restored session
        // sees it unlabelled and empty until the next live event — the exact gap on a
        // host restart. `bind_remote` covers the later respawn/reconnect path; this
        // covers cold restore, where `self` does not exist yet so the binding is
        // driven directly from the locals that seed the struct below.
        let mut remote_prompt_task = None;
        let mut remote_choice_task = None;
        let mut remote_choice_sender = None;
        if let Some(binding) = &remote {
            let model = thread.model.clone().or_else(|| model.clone());
            binding.set_meta(SessionMeta {
                title: thread.title.clone(),
                // The backend's baseline again, for the same reason as the mode
                // below: a session opened remotely carries no pick of its own, so
                // without this its picker would show nothing selected until the
                // user changed something. Mirrors `Self::effective_model`, which
                // takes over from the first live republish.
                model: model
                    .clone()
                    .or_else(|| connection.as_ref().and_then(|c| c.default_model())),
                // The backend's baseline: the tab seeds `permission_mode: None`
                // below, and a restored pick republishes when it is applied.
                permission_mode: connection.as_ref().and_then(|c| c.default_mode()),
                cwd: Some(cwd.clone()),
            });
            if let Ok(entries_json) = serde_json::to_string(&thread.entries) {
                binding.publish_transcript(entries_json, model);
            }
            // Relay remotely-injected prompts (phone sends) into this tab so the
            // desktop shows the user's own bubble, not just the reply.
            let (tx, rx) = futures::channel::mpsc::unbounded();
            binding.set_prompt_sink(tx);
            let (event_tx, event_rx) = futures::channel::mpsc::unbounded();
            binding.set_event_sink(event_tx);
            remote_prompt_task = Some(Self::spawn_remote_prompt_relay(rx, event_rx, cx));
            // Complete model/mode picks the backend fixes at spawn, which only
            // this view can carry out (it owns the respawn).
            let (choice_tx, choice_rx) = futures::channel::mpsc::unbounded();
            binding.set_choice_sink(choice_tx.clone());
            remote_choice_sender = Some(choice_tx);
            remote_choice_task = Some(Self::spawn_remote_choice_relay(choice_rx, cx));
        }

        Self {
            thread,
            connection,
            remote_session_id,
            remote,
            backend,
            composer,
            session_detail_open: false,
            last_notify: std::time::Instant::now(),
            flush_scheduled: false,
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
            screen_control,
            screen_prompts: HashMap::new(),
            cwd,
            model,
            permission_mode: None,
            effort: None,
            // Seed the composer's posture picks from the restored blob so they
            // display (and re-persist) the resumed choice.
            feature_values: seed_posture_feature_values(&posture),
            disconnected,
            // Dormant restores boot resumable-idle: a send respawns via --resume.
            interrupted: mode == ConnectMode::DormantResume,
            dormant: mode == ConnectMode::DormantResume,
            publish_throttle: publish_throttle::PublishThrottle::new(),
            last_saved_revision: std::cell::Cell::new(u64::MAX),
            meta_dirty: std::cell::Cell::new(false),
            unbound: mode == ConnectMode::UnboundDraft,
            unbound_agent_id,
            probed_catalogs: HashMap::new(),
            probe_catalogs_live: true,
            view_mode: ChatViewMode::Chat,
            terminal: None,
            companion_session: None,
            chat_advanced_since_companion: false,
            _terminal_observer: None,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: ImageCache::new(),
            preview: None,
            open_tool_sheet: None,
            sheet_copied: false,
            _sheet_copy_task: None,
            _drain_task: drain_task,
            _remote_prompt_task: remote_prompt_task,
            _remote_choice_task: remote_choice_task,
            remote_choice_tx: remote_choice_sender,
            _subscriptions: subscriptions,
            question_cards: HashMap::new(),
            question_card_subs: HashMap::new(),
            embedded_terminals: HashMap::new(),
            embedded_terminal_subs: HashMap::new(),
            env_inputs: Vec::new(),
            env_input_subs: Vec::new(),
            auth: None,
            checkpoint_engine: None,
            pre_turn_checkpoint: None,
            rewind_confirm: None,
            rewinding: false,
            rewind_then_send: None,
            pending_edit: None,
            pane_group: None,
            remote_tab_title: None,
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
            title_generated: false,
            title_task: None,
            is_git_project,
            worktree_draft_enabled: false,
            worktree_slug_input: None,
            _worktree_slug_sub: None,
            worktree_create_state: roster::WorktreeCreateState::default(),
            pending_worktree_send: None,
            worktree_branch_label: None,
            import_bridge: None,
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
    ///
    /// An import bridge is the exception: it has no live backend, so it assembles
    /// on an inert stream-json placeholder whose name ("Claude") would caption
    /// every bubble of a transcript that is demonstrably *not* Claude's. Its own
    /// provider name is authoritative there.
    fn provider_label(&self) -> &str {
        if let Some(bridge) = self.import_bridge.as_ref() {
            return &bridge.provider_display;
        }
        self.backend.provider_display_name()
    }

    /// The [`ConnectSpec`] a respawn launches on: the session's identity (model,
    /// resume id, mode, effort) plus every spawn-time choice the user has made.
    ///
    /// Split out from `respawn` so what a reconnect *carries* is assertable
    /// without spawning a subprocess. That is not a testing nicety here: a
    /// backend whose gating is spawn-time (pi's `--tools` allowlist) has its
    /// entire safety posture decided by this struct, and a field missing from it
    /// fails silently in the worst direction — the control still moves, the agent
    /// just isn't bound by it.
    fn respawn_spec(
        &self,
        env: Vec<(String, String)>,
        auth_method: Option<String>,
    ) -> ConnectSpec {
        let mut spec = ConnectSpec::for_backend(
            &self.backend,
            self.cwd.clone(),
            self.model.clone(),
            self.thread.session_id.clone(),
            self.permission_mode.clone(),
            self.effort.clone(),
        );
        spec.env = env;
        spec.auth_method = auth_method;
        // Preserve the chosen Codex posture across the respawn (Stop-resume, a
        // rewind fork), so it isn't silently reset to the default on reconnect.
        spec.codex_posture = self.codex_posture_snapshot();
        // Pi's tool gating is a spawn-time allowlist, so a respawn is the ONLY
        // way a posture change takes effect — and this is the line that carries
        // it. Without it, picking Read-only respawned pi on the DEFAULT posture:
        // the pill read "Read-only" while the agent kept writing files, which is
        // worse than having no pill at all. It also silently reset the posture on
        // every unrelated respawn (Stop-then-send, a model switch).
        spec.pi_posture = self.pi_posture_snapshot();
        spec
    }

    /// Pi's tool posture, read from the composer's feature picks. `None` for a
    /// non-Pi chat or when nothing was changed (restore then applies the
    /// deliberate default).
    ///
    /// Unlike Codex's, this posture is the session's ONLY tool gate — pi never
    /// asks before running anything — so it is snapshotted for persistence
    /// rather than left to be re-derived.
    fn pi_posture_snapshot(&self) -> Option<PiPosture> {
        if self.backend.transport != Transport::Rpc {
            return None;
        }
        let tools = match self.feature_values.get(pi_posture::FEATURE_TOOLS) {
            Some(FeatureValue::Choice(wire)) => Some(wire.clone()),
            _ => None,
        };
        let context_files = match self.feature_values.get(pi_posture::FEATURE_CONTEXT_FILES) {
            Some(FeatureValue::Bool(on)) => Some(*on),
            _ => None,
        };
        if tools.is_none() && context_files.is_none() {
            return None;
        }
        Some(PiPosture::from_parts(tools.as_deref(), context_files))
    }

    /// The Codex posture `(approval_policy, sandbox)` the user has selected, read
    /// from the composer's feature picks. `None` for a non-Codex chat or when the
    /// posture was never changed from the default (nothing to persist).
    fn codex_posture_snapshot(&self) -> Option<(String, String)> {
        if self.backend.transport != Transport::AppServer {
            return None;
        }
        let choice = |id: &str| match self.feature_values.get(id) {
            Some(FeatureValue::Choice(wire)) => Some(wire.clone()),
            _ => None,
        };
        match (choice("codex_approval_policy"), choice("codex_sandbox")) {
            (None, None) => None,
            (approval, sandbox) => Some((
                approval.unwrap_or_else(|| "on-request".to_string()),
                sandbox.unwrap_or_else(|| "workspace-write".to_string()),
            )),
        }
    }

    /// The chat's session id once Claude has minted one (after the first turn
    /// begins). Persisted in the tab's `PersistedTabKind::AgentChat` so restore
    /// can find the matching transcript blob and `--resume`.
    pub fn session_id(&self) -> Option<&str> {
        self.thread.session_id.as_deref()
    }

    /// Why this session is not running, when it isn't.
    ///
    /// Read by the remote on-demand open path, which can see only that a session
    /// failed to reach the registry and not why. The view holds the reason it is
    /// already showing the user — a missing binary, a refused resume — so a
    /// client that asked for the session gets that instead of a bare "unknown
    /// session" on whatever request follows.
    pub fn last_error(&self) -> Option<String> {
        self.thread.last_error.clone()
    }

    /// Whether this is still an unbound *New Agent* draft (no subprocess has
    /// spawned; the first send binds it). The host reads this to label the tab
    /// "New Agent" and to skip persisting the empty draft.
    pub fn is_unbound(&self) -> bool {
        self.unbound
    }

    /// Whether this is a transcript-only import bridge (OpenCode/Pi). The host
    /// reads this to skip persisting the tab — a bridge has no live session id,
    /// so restoring it through the normal resumed-chat path would spawn a live
    /// subprocess and drop the imported transcript; it re-opens from Session
    /// History instead (like Diff/Tasks tabs).
    pub fn is_import_bridge(&self) -> bool {
        self.import_bridge.is_some()
    }

    /// `(preset_id, session_id)` identity of an import bridge tab, for reopen
    /// dedup (a bridge thread carries no `session_id()` to key on). `None` for
    /// non-bridge chats.
    pub fn import_bridge_key(&self) -> Option<(&str, &str)> {
        self.import_bridge
            .as_ref()
            .map(|b| (b.preset_id.as_str(), b.session_id.as_str()))
    }

    /// The unsent composer draft text, for the layout snapshot (so a typed but
    /// unsent message survives a tab close / app quit).
    pub fn draft_text(&self, cx: &App) -> String {
        self.composer.read(cx).current_draft(cx)
    }

    /// The text of each queued-but-unsent message (oldest first), for the layout
    /// snapshot. Text-only — staged images/context aren't persisted.
    pub fn queued_texts(&self, cx: &App) -> Vec<String> {
        self.composer.read(cx).queued_texts()
    }

    /// Seed a restored draft + queued messages into the composer after
    /// construction. The draft seed is no-clobber-guarded; queued messages are
    /// re-shown as chips and NEVER auto-sent (a restored app must not fire billed
    /// sends without a user action).
    pub fn seed_draft_and_queue(
        &mut self,
        draft: Option<String>,
        queued: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.update(cx, |c, cx| {
            if let Some(text) = draft {
                c.seed_draft(text, window, cx);
            }
            if !queued.is_empty() {
                c.seed_queued(queued, cx);
            }
        });
    }

    /// The current view mode (chat vs. companion terminal).
    pub fn view_mode(&self) -> ChatViewMode {
        self.view_mode
    }

    /// Whether a companion terminal has already been spawned for this chat.
    pub fn has_companion_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// The companion terminal's daemon session id, so the host can reap it when
    /// the tab closes. `None` when no companion was ever spawned.
    pub fn companion_session_id(&self) -> Option<AgentSessionId> {
        self.companion_session
    }

    /// Parameters for spawning a companion terminal that resumes THIS chat's
    /// session interactively, or `None` when the chat can't be mirrored to a
    /// terminal: it's an unbound draft, hasn't minted a session id yet, or runs
    /// over a transport with no interactive `--resume` CLI wired (ACP presets).
    /// The host reads this because only it owns the runtime that spawns.
    pub fn terminal_launch_spec(&self) -> Option<ChatTerminalSpec> {
        if self.unbound {
            return None;
        }
        let session_id = self.thread.session_id.clone()?;
        let (adapter, adapter_id) = match self.backend.transport {
            Transport::StreamJson => (AgentAdapter::ClaudeCode, "claude-code"),
            Transport::AppServer => (AgentAdapter::Codex, "codex"),
            Transport::Acp => {
                // An ACP chat gets a companion terminal only when its preset has
                // a confirmed interactive-resume TUI (opencode today) AND the
                // agent-supplied session id is safe to place on a command line.
                // The resume runs through the generic `Custom` adapter, which
                // spawns `custom_command`'s argv verbatim.
                let cmd = self.backend.acp_command.as_deref()?;
                let preset = oximux_settings::ACP_PRESETS.iter().find(|p| p.command == cmd)?;
                if preset.interactive_resume.is_none()
                    || !is_safe_resume_session_id(&session_id)
                {
                    return None;
                }
                (AgentAdapter::Custom, preset.id)
            }
            // Pi resumes by session id in the session's own project — the same
            // `pi --session <id>` the chat itself spawns with. (An earlier note
            // here claimed a `--session <uuid>` "silently resumes nothing" and
            // that only the file path worked; probing the real binary showed the
            // reverse — the id resolves against the project's store and a miss
            // exits 1, while a stale *path* is what silently mints an empty
            // session.) `cwd` is the chat's cwd, which is the session's project,
            // so the id resolves.
            Transport::Rpc => {
                if !is_safe_resume_session_id(&session_id) {
                    return None;
                }
                (AgentAdapter::Custom, "pi")
            }
        };
        Some(ChatTerminalSpec {
            adapter,
            adapter_id,
            session_id,
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
        })
    }

    /// Why the companion-terminal toggle is (un)available, so the view-options
    /// hint can distinguish "send a message first" (no session yet) from "no
    /// interactive terminal for this agent" (a bound ACP chat). Keeps
    /// [`Self::terminal_launch_spec`]'s `Option` return stable for its other
    /// callers — this is the richer reason computed alongside it.
    pub fn terminal_availability(&self) -> TerminalAvailability {
        if self.terminal_launch_spec().is_some() {
            TerminalAvailability::Available
        } else if self.unbound || self.thread.session_id.is_none() {
            TerminalAvailability::NoSessionYet
        } else {
            // Bound with a session, but the transport has no interactive resume
            // CLI wired (ACP presets today).
            TerminalAvailability::NoInteractiveResume
        }
    }

    /// Focus the surface the active mode shows: the companion terminal in
    /// Terminal view, the composer in Chat view.
    fn focus_active_surface(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_focus_handle(cx).focus(window, cx);
    }

    /// The focus handle for whichever surface is currently shown — the host's
    /// `PaneContent::AgentChat` focus routing delegates here so keystrokes land in
    /// the terminal while it's up, and back in the composer otherwise.
    pub fn active_focus_handle(&self, cx: &App) -> FocusHandle {
        match (self.view_mode, &self.terminal) {
            (ChatViewMode::Terminal, Some(tv)) => tv.read(cx).focus_handle(cx),
            _ => self.composer.read(cx).focus_handle(cx),
        }
    }

    /// Render the companion terminal full-body with a slim header carrying a
    /// "return to chat" button (the click target for the ⌃⇧V toggle). Only reached
    /// when `view_mode` is Terminal and the companion exists.
    fn render_terminal_mode(
        &self,
        terminal: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .flex_none()
                    .px(px(density.pad_panel))
                    .py(px(density.pad_row))
                    .border_b_1()
                    .border_color(theme.border_inactive)
                    .child(
                        div()
                            .text_size(px(typo.t_body_sm))
                            .text_color(theme.fg_muted)
                            .child(SharedString::from(format!(
                                "{} · terminal",
                                self.provider_label()
                            ))),
                    )
                    .child(
                        div()
                            .id("chat-return-to-chat")
                            .flex_none()
                            .px(px(density.pad_panel))
                            .py(px(density.gap_inline * 0.5))
                            .rounded(px(8.0))
                            .cursor_pointer()
                            .bg(theme.bg_panel_alt)
                            .text_color(theme.fg_base)
                            .text_size(px(typo.t_body_sm))
                            .hover(|s| s.bg(theme.hover_overlay))
                            .child(SharedString::from("Chat  ⌃⇧V"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    this.set_view_mode(ChatViewMode::Chat, window, cx)
                                }),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(terminal))
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
        // A rewind in flight — or a pending ACP auth prompt (the session can't
        // accept input until the user signs in) — disables the composer just like
        // a disconnect until it resolves. A worktree create in flight (or one
        // that failed with a message still staged) folds in the same way: the
        // composer's own `submit()` short-circuits on `disconnected` before it
        // ever emits a second `Submit`, which is what keeps a second distinct
        // send from falling through `send_text`'s `bind_now` at the ORIGINAL
        // cwd while the worktree step is still pending (HIGH finding).
        let worktree_busy = !matches!(self.worktree_create_state, roster::WorktreeCreateState::Idle);
        let (disconnected, turn_active) = (
            self.disconnected || self.rewinding || self.auth.is_some() || worktree_busy,
            self.thread.turn_active,
        );
        // Advertise controls by capability, not by hard-coding the provider.
        let caps = self
            .connection
            .as_ref()
            .map(|c| c.capabilities())
            .unwrap_or_default();
        let mut vocab = control_vocab_of(self.connection.as_deref());
        // Overlay optimistic feature picks so a toggle/select reflects the user's
        // choice immediately, without waiting for the backend to echo it back.
        apply_feature_overrides(&mut vocab.features, &self.feature_values);
        let (model, permission_mode, effort) =
            (self.model.clone(), self.permission_mode.clone(), self.effort.clone());
        // The command palette is offered only when the backend advertises
        // commands (Claude does; others send an empty list, which disables it).
        let slash_commands =
            if caps.supports_slash { self.thread.slash_commands.clone() } else { Vec::new() };
        let slash_descriptions = if caps.supports_slash {
            self.thread.slash_command_descriptions.clone()
        } else {
            std::collections::HashMap::new()
        };
        let slash_hints = if caps.supports_slash {
            self.thread.slash_command_hints.clone()
        } else {
            std::collections::HashMap::new()
        };
        // The input placeholder follows the bound agent ("Message Codex…"); a New
        // Agent draft that just bound gets its real provider name here (it was
        // constructed with the generic "Agent" placeholder).
        let provider_label = self.provider_label().to_string();
        // Live context-meter inputs: prefer the mid-turn `live_usage`, fall back
        // to the settled `usage`; total token occupancy = input + cache + output
        // (ACP folds its whole "used" count into `input_tokens`). The window is
        // the cross-turn cached denominator; cost is the session accumulator.
        let meter_used = self
            .thread
            .live_usage
            .as_ref()
            .or(self.thread.usage.as_ref())
            .map(|u| {
                u.input_tokens + u.cache_read_tokens + u.cache_creation_tokens + u.output_tokens
            });
        let meter_window = self.thread.last_known_context_window;
        let meter_cost = self.thread.session_cost_usd;
        // An unbound draft has no `connection`, so `caps`/`vocab` above are the
        // *empty* defaults — pushing them would blank the draft's pre-bind model
        // list. Its picker shape is owned by `sync_unbound_composer` instead.
        let unbound = self.unbound;
        self.composer.update(cx, |c, cx| {
            c.set_state(disconnected, turn_active, cx);
            c.set_can_steer(caps.supports_steer, cx);
            c.set_usage_meter(meter_used, meter_window, meter_cost, cx);
            c.set_slash_commands(slash_commands, slash_descriptions, slash_hints, cx);
            c.set_provider_label(provider_label, cx);
            if !unbound {
                c.set_controls(model, permission_mode, effort, caps.supports_modes, caps.supports_config, vocab, cx);
                // A bound chat never shows the agent picker (its transport is
                // fixed) or the worktree pill (its cwd is fixed); clearing both
                // here is what hides them after `bind_now` (cheap no-op once
                // already cleared). The pill is pushed from
                // `sync_unbound_composer`, which stops running once bound — so
                // without this clear the composer would keep rendering the stale
                // draft against a live session.
                c.set_agent_picker(false, Vec::new(), None, cx);
                c.set_worktree_draft(None, cx);
            }
        });
        // The composer keeps its own `unbound` flag, and the agent picker, the
        // Import-session row and the placeholder's agent name all read it. Any
        // sync while the draft is still unbound must therefore re-assert the
        // draft's shape rather than the bound-chat shape, or flipping an
        // unrelated control (the worktree toggle syncs here) silently strips
        // those three from a New Agent draft with no way to get them back.
        //
        // This and the `if !unbound` guard above are INDEPENDENT safety nets:
        // either one alone repairs the symptom today, so neither is redundant in
        // the sense of being deletable. The guard stops a connection-less draft's
        // empty vocab being pushed at all; this re-asserts the real shape for
        // every one of `sync_composer`'s callers rather than just the ones that
        // happen to seed it. Removing either leaves the invariant resting on a
        // single accident of ordering.
        if unbound {
            self.sync_unbound_composer(cx);
        }
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
        // The picked agent's model vocab: a landed catalog probe (Codex/ACP dynamic
        // models) wins; otherwise the static roster list (Claude). A dynamic agent
        // still probing / failed / unprobed yields an empty list, so the model
        // picker simply stays hidden until its catalog lands.
        let vocab = self
            .unbound_agent_id
            .as_ref()
            .and_then(|id| {
                let entry = roster.iter().find(|e| &e.id == id)?;
                let (models, default_model) = match self.probed_catalogs.get(id) {
                    Some(ProbeState::Ready(catalog)) => {
                        (catalog.models.clone(), catalog.default_model.clone())
                    }
                    _ => (entry.models.clone(), entry.default_model().map(str::to_string)),
                };
                Some(ControlVocab {
                    models,
                    permission_modes: Vec::new(),
                    efforts: Vec::new(),
                    features: Vec::new(),
                    default_model,
                    default_mode: None,
                    default_effort: None,
                })
            })
            .unwrap_or_default();
        let model = self.model.clone();
        // The worktree pill is draft-only state, so it is pushed from here rather
        // than `sync_composer` — that one derives its vocab from `self.connection`,
        // which a draft doesn't have.
        let worktree_draft = self.worktree_draft_for_composer(cx);
        self.composer.update(cx, |c, cx| {
            c.set_agent_picker(true, agents, current, cx);
            // Pre-bind: only the model picker (no modes/effort until the live conn).
            c.set_controls(model, None, None, false, false, vocab, cx);
            c.set_worktree_draft(worktree_draft, cx);
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
        self.unbound_agent_id = Some(id.clone());
        // A dynamic-model agent (Codex/ACP) has no static roster models — fetch its
        // real catalog off-thread so the picker fills before the first send.
        self.maybe_probe_catalog(id, cx);
        self.sync_unbound_composer(cx);
        cx.notify();
    }

    /// Kick off a throwaway catalog probe for the picked agent so the draft's
    /// model picker can fill before the user commits. No-op for an agent with a
    /// static roster list (Claude), one already probed/probing, or a view with
    /// [`Self::probe_catalogs_live`] off (tests). The blocking
    /// probe runs on a dedicated thread — the connection spawns its own workers,
    /// so no GPUI executor or tokio reactor is touched — and its result is folded
    /// back on the UI thread, re-syncing the composer only if the pick still holds.
    fn maybe_probe_catalog(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.probe_catalogs_live {
            return; // a view built on a stub connection has no real binary to probe
        }
        if self.probed_catalogs.contains_key(&id) {
            return; // already probing, ready, or a settled failure — don't re-run
        }
        let roster = roster::chat_roster_from_cx(cx);
        match roster.iter().find(|e| e.id == id) {
            // A static model list (Claude) needs no probe; an unknown id is skipped.
            Some(entry) if entry.models.is_empty() => {}
            _ => return,
        }
        // Consult the process-wide catalog cache (seeded from disk at boot). A hit
        // paints the picker instantly — the difference between a ~5s cold spawn and
        // the models appearing on open. A this-session probe is trusted outright; a
        // disk seed is shown immediately but revalidated once in the background.
        let cache = cx.try_global::<crate::catalog_cache::CatalogCache>().cloned();
        if let Some(catalog) = cache.as_ref().and_then(|c| c.get(&id)) {
            self.probed_catalogs.insert(id.clone(), ProbeState::Ready(catalog));
            if cache.as_ref().is_some_and(|c| c.is_fresh(&id)) {
                // Already probed live this session — trust it, spawn nothing.
                self.sync_unbound_composer(cx);
                return;
            }
            // A stale disk seed: keep it painted, revalidate below without a
            // `Loading` flicker.
        } else {
            // Nothing cached — the picker stays hidden until the probe lands.
            self.probed_catalogs.insert(id.clone(), ProbeState::Loading);
        }
        self.sync_unbound_composer(cx);
        let spec = ConnectSpec::for_backend(&self.backend, self.cwd.clone(), None, None, None, None);
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(probe_catalog(spec));
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = &result {
                    tracing::warn!(agent = %id, error = %e, "pre-bind catalog probe failed");
                }
                let has_good_seed = matches!(
                    this.probed_catalogs.get(&id),
                    Some(ProbeState::Ready(c)) if !c.models.is_empty()
                );
                let (next, to_cache) = fold_probe_result(has_good_seed, result);
                // Warm the shared cache (only a non-empty success is worth caching —
                // an empty result would hide the picker and mask a transient failure).
                if let Some(catalog) = to_cache
                    && let Some(c) = cx.try_global::<crate::catalog_cache::CatalogCache>()
                {
                    c.record(&id, catalog);
                }
                if let Some(state) = next {
                    this.probed_catalogs.insert(id.clone(), state);
                }
                // Only refresh the picker if this agent is still the draft's pick.
                if this.unbound && this.unbound_agent_id.as_deref() == Some(id.as_str()) {
                    this.sync_unbound_composer(cx);
                    cx.notify();
                }
            });
        })
        .detach();
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
    ///
    /// `pub(crate)` so a tab opened with an initial prompt (a scheduled run) sends
    /// it down exactly this path rather than a parallel one — the guards, the
    /// deferred bind, the optimistic thread push and the remote tee all have to
    /// apply equally to a prompt nobody typed.
    pub(crate) fn send_text(
        &mut self,
        text: String,
        images: Vec<ChatImage>,
        cx: &mut Context<Self>,
    ) {
        // An import bridge has no live backend — its composer is swapped for
        // Resume-in-terminal, so no send path should ever construct here. Guard
        // defensively in case a stray Submit event slips through.
        if self.import_bridge.is_some() {
            return;
        }
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
        // Unbound draft with the worktree toggle armed: the FIRST send creates
        // the worktree (an async git op) before any subprocess spawns, then
        // resumes this exact send once it lands — see `start_worktree_then_send`.
        if self.unbound && self.worktree_draft_enabled {
            match self.worktree_create_state {
                roster::WorktreeCreateState::Idle => {
                    self.start_worktree_then_send(text, images, cx);
                    return;
                }
                roster::WorktreeCreateState::Creating | roster::WorktreeCreateState::Failed(_) => {
                    // A create is already in flight for an earlier staged
                    // message, or one failed and is awaiting Retry / "continue
                    // without a worktree". A second distinct Submit must NEVER
                    // fall through to `bind_now` below — that would silently
                    // bind at the ORIGINAL cwd (defeating the toggle) and, once
                    // the in-flight create landed, `on_worktree_create_outcome`
                    // would re-send the FIRST staged message into that
                    // now-wrongly-bound session — duplicated, out-of-order
                    // sends plus an orphaned worktree (HIGH finding). In normal
                    // use this is unreachable — `sync_composer` folds this state
                    // into the composer's own `disconnected`, so its `submit()`
                    // already refuses a second Submit before this method is
                    // even called. This is the defense-in-depth backstop: drop
                    // the new text/images rather than clobbering the message
                    // already staged in `pending_worktree_send`.
                    return;
                }
            }
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
        // The agent needs sign-in before it can accept a prompt — the auth card is
        // the only actionable state. Don't push a phantom user entry the parked
        // handshake would silently drop (which would wedge `turn_active` forever);
        // the composer is also gated on this in `sync_composer`.
        if self.auth.is_some() {
            return;
        }
        // Optimistically record the prompt; the reply streams in via `on_event`.
        self.thread.push_user_message_with_images(text.clone(), images.clone());
        self.note_chat_prompt_sent();
        // Snapshot the repo for this turn's rewind anchor (background — never
        // blocks the send). The user entry we just pushed is the last one.
        let user_index = self.thread.entries.len() - 1;
        self.take_checkpoint_for(user_index, cx);
        // After the FIRST user message on a Claude/Codex chat, kick off a one-shot
        // LLM title. ACP chats are skipped — their agents push a native title
        // through the same sink, which a haiku result would race/clobber.
        let auto_title = cx
            .try_global::<AgentLaunchSettings>()
            .map(|s| s.auto_title_enabled)
            .unwrap_or(true);
        if user_index == 0
            && !self.title_generated
            && auto_title
            && !matches!(self.backend.transport, Transport::Acp)
        {
            self.title_generated = true; // guard a fast double-send from re-firing
            self.spawn_title_generation(text.clone(), cx);
        }
        if let Some(conn) = &self.connection {
            match conn.send_user_message_with_images(&text, &images) {
                // Tee the prompt to remote subscribers only. No backend echoes the
                // user's own message, so without this a phone renders replies to
                // prompts it never showed. It is NOT applied to `self.thread` —
                // the optimistic push above already put the bubble there, and
                // folding it again here would duplicate it.
                Ok(()) => {
                    if let Some(binding) = &self.remote {
                        binding.ingest(ThreadEvent::UserMessage {
                            text: text.clone(),
                            images: images.clone(),
                        });
                    }
                }
                Err(e) => self.thread.last_error = Some(format!("Send failed: {e}")),
            }
        }
        // Jump to (and re-arm following of) the bottom for the new turn.
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Hand a message to the turn that is already streaming (a queued chip's ↑ on
    /// a `supports_steer` backend). The agent picks it up at the next turn
    /// boundary and changes course.
    ///
    /// Deliberately not routed through [`Self::send_text`]. That path is the
    /// *start* of a turn: it binds an unbound draft, respawns an interrupted
    /// child, generates the tab title and takes the pre-turn checkpoint. None of
    /// that applies to a message going into a turn that is already running — and
    /// the checkpoint especially must not: it anchors rewind to the repo as it
    /// stood *before* a turn, and taking one now would capture a tree the running
    /// turn's own tools are halfway through editing.
    fn steer_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() || !self.thread.turn_active {
            return;
        }
        let Some(conn) = &self.connection else { return };
        // The bubble goes in at the point the user sent it, which is also where
        // the agent will act on it — the turn's remaining output lands after.
        match conn.steer(&text) {
            Ok(()) => {
                self.thread.push_user_message(&text);
                self.note_chat_prompt_sent();
            }
            Err(e) => self.thread.last_error = Some(format!("Steer failed: {e}")),
        }
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Kick off a one-shot LLM title generation for this chat's first message.
    /// Owned on `title_task` so a tab close drops it. The generation runs a child
    /// process needing a tokio reactor, so it's handed to the tokio runtime and
    /// bridged back via a oneshot (the proven `source_control::ai_generation`
    /// pattern); a bounded 10s timeout + `kill_on_drop` cap any lingering child.
    /// Any failure (missing `claude`, timeout, non-JSON reply) silently keeps the
    /// counter label. On success the result rides the existing, already-safe
    /// `TitleChanged` sink (a manual rename still wins in the header render).
    fn spawn_title_generation(&mut self, first_message: String, cx: &mut Context<Self>) {
        let cwd = self.cwd.clone();
        self.title_task = Some(cx.spawn(async move |this, cx| {
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                return;
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            handle.spawn(async move {
                let title = oximux_agents::tab_title::generate_title(&first_message, &cwd, cancel).await;
                let _ = tx.send(title);
            });
            if let Ok(Some(title)) = rx.await {
                let _ = this.update(cx, |_view, cx| {
                    cx.emit(AgentChatEvent::TitleChanged(title));
                });
            }
        }));
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

    /// True when the latest turn looks like an auth failure the user can fix by
    /// signing in from a terminal. Some CLIs answer with a plain "Not logged in
    /// · Please run /login" reply that settles as an ordinary assistant turn
    /// (no error), so the last assistant text is scanned alongside `last_error`.
    /// Matched case-insensitively and kept broad on purpose across providers.
    fn is_signed_out(&self) -> bool {
        const SIGNATURES: &[&str] = &[
            "please run /login",
            "not logged in",
            "logged out",
            "signed out",
            "invalid api key",
            "authentication_error",
        ];
        let hit = |s: &str| {
            let l = s.to_ascii_lowercase();
            SIGNATURES.iter().any(|sig| l.contains(sig))
        };
        if self.thread.last_error.as_deref().is_some_and(hit) {
            return true;
        }
        // Only the most recent assistant reply counts — an old sign-in prompt
        // must not keep the banner up after a later turn succeeds.
        self.thread
            .entries
            .iter()
            .rev()
            .find_map(|e| match e {
                ThreadEntry::Assistant(m) => Some(hit(&m.text)),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// The CLI adapter id for this chat's transport when it has an interactive
    /// binary the user can sign into in a terminal. `None` for ACP presets,
    /// whose sign-in flow isn't a bundled CLI. Gates the signed-out banner.
    fn login_adapter_id(&self) -> Option<&'static str> {
        match self.backend.transport {
            Transport::StreamJson => Some("claude-code"),
            Transport::AppServer => Some("codex"),
            Transport::Acp => None,
            // Pi's protocol exposes no sign-in command — authentication happens
            // in the `pi` CLI itself, which is exactly what this banner opens.
            Transport::Rpc => Some("pi"),
        }
    }

    /// The signed-out banner's action: a link-styled control that asks the host
    /// to open a terminal running this agent's CLI so `/login` is reachable.
    fn open_login_terminal_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typo = &self.typography;
        div()
            .id("chat-open-login-terminal")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(theme.status_info.opacity(0.15))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_info)
            .hover(|s| s.bg(theme.status_info.opacity(0.28)))
            .child(
                Icon::default()
                    .path("icons/square-terminal.svg")
                    .size(px(13.0))
                    .text_color(theme.status_info),
            )
            .child(SharedString::from("Open terminal to sign in"))
            .on_click(cx.listener(|this, _e, _window, cx| this.request_open_login_terminal(cx)))
    }

    /// Ask the host to spawn a terminal tab running this agent's CLI at the
    /// chat's cwd. No-op for transports with no interactive login binary (ACP).
    fn request_open_login_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(adapter_id) = self.login_adapter_id() {
            cx.emit(AgentChatEvent::OpenLoginTerminalRequested {
                adapter_id,
                cwd: self.cwd.clone(),
            });
        }
    }

    /// Authenticate with the ACP method the user picked from the auth card: mark
    /// it pending (spinner), then call the connection's `authenticate`, which runs
    /// on the worker and retries the session open on the same connection. A
    /// terminal-kind method mounts its login terminal via a follow-up
    /// `AuthTerminal` event; success clears the card on `SessionInit`.
    fn request_authenticate(&mut self, method_id: String, cx: &mut Context<Self>) {
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id.clone());
            auth.error = None;
        }
        if let Some(conn) = self.connection.as_ref()
            && let Err(e) = conn.authenticate(&method_id)
            && let Some(auth) = self.auth.as_mut()
        {
            auth.pending = None;
            auth.error = Some(e.to_string());
        }
        cx.notify();
    }

    /// Begin a browser OAuth sign-in (Codex "Sign in with ChatGPT"): mark the
    /// method pending and kick the connection's login (fire-and-forget — the RPC
    /// runs on the worker, so this click never blocks the UI). The browser URL
    /// arrives asynchronously as [`ThreadEvent::AuthUrl`] and is opened there; the
    /// card stays pending until `account/login/completed` → [`ThreadEvent::AuthOutcome`]
    /// resolves it. A failure to even start surfaces on the card immediately.
    fn request_browser_login(&mut self, method_id: String, cx: &mut Context<Self>) {
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id);
            auth.error = None;
        }
        if let Some(Err(e)) = self.connection.as_ref().map(|c| c.begin_browser_login())
            && let Some(auth) = self.auth.as_mut()
        {
            auth.pending = None;
            auth.error = Some(e.to_string());
        }
        cx.notify();
    }

    /// A browser-OAuth sign-in pill (Codex). Distinct from [`Self::auth_pill`]
    /// (which runs ACP `authenticate`): clicking this opens a browser via
    /// [`Self::request_browser_login`]. Renders a muted "Opening browser…" while
    /// pending.
    fn browser_login_pill(
        &self,
        method_id: &str,
        label: &str,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Opening browser…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.request_browser_login(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("codex-oauth-{method_id}"),
            label.to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// One clickable auth pill (Agent/Terminal name, or an EnvVar "Retry"). While
    /// its method is authenticating it renders a muted "Authenticating…" label
    /// instead of a button.
    fn auth_pill(
        &self,
        method_id: &str,
        label: &str,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Authenticating…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.request_authenticate(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("acp-auth-{method_id}"),
            label.to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// Build the ACP auth card from the pending prompt: a pill per Agent/Terminal
    /// method, an instructions block + Retry for an EnvVar method, an optional
    /// inline login terminal, and a retry-state error note.
    fn render_auth_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        let provider = self.provider_label();
        let Some(auth) = self.auth.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        // `reconcile_env_inputs` builds the masked fields for the FIRST EnvVar
        // method only, so render the interactive form for that one and skip any
        // further EnvVar methods — otherwise a second would reuse the first's
        // fields and submit the wrong variable names. (An agent advertising two
        // simultaneous EnvVar methods is unheard of; this just fails safe.)
        let mut env_form_done = false;
        for m in &auth.methods {
            let is_pending = auth.pending.as_deref() == Some(m.id.as_str());
            match &m.kind {
                // EnvVar: an interactive secret form — a masked field per advertised
                // variable (built in `reconcile_env_inputs`, so the values never
                // touch the transcript) + a submit pill that respawns the agent WITH
                // those values in its env, then authenticates.
                AuthMethodKind::EnvVar { .. } if env_form_done => continue,
                AuthMethodKind::EnvVar { link, .. } => {
                    env_form_done = true;
                    let field_rows: Vec<AnyElement> = self
                        .env_inputs
                        .iter()
                        .map(|(name, input)| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .w_full()
                                .min_w_0()
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(typo.t_label_xs))
                                        .text_color(theme.fg_base)
                                        .child(SharedString::from(name.clone())),
                                )
                                .child(Input::new(input))
                                .into_any_element()
                        })
                        .collect();
                    let submit = self.env_submit_pill(&m.id, is_pending, cx);
                    rows.push(auth_card::env_var_form(
                        m.description.as_deref(),
                        link.as_deref(),
                        field_rows,
                        submit,
                        theme,
                        &typo,
                        density,
                    ));
                }
                // BrowserOauth (Codex ChatGPT) → a pill that opens the browser,
                // not the ACP `authenticate` path.
                AuthMethodKind::BrowserOauth => {
                    let pill = self.browser_login_pill(&m.id, &m.name, is_pending, cx);
                    let row = match m.description.as_deref() {
                        Some(desc) => div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(pill)
                            .child(
                                div()
                                    .text_size(px(typo.t_label_xs))
                                    .text_color(theme.fg_muted)
                                    .child(SharedString::from(desc.to_string())),
                            )
                            .into_any_element(),
                        None => pill,
                    };
                    rows.push(row);
                }
                // Agent / Terminal → a single labeled pill (+ its description).
                _ => {
                    let pill = self.auth_pill(&m.id, &m.name, is_pending, cx);
                    let row = match m.description.as_deref() {
                        Some(desc) => div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(pill)
                            .child(
                                div()
                                    .text_size(px(typo.t_label_xs))
                                    .text_color(theme.fg_muted)
                                    .child(SharedString::from(desc.to_string())),
                            )
                            .into_any_element(),
                        None => pill,
                    };
                    rows.push(row);
                }
            }
        }
        let terminal =
            auth.terminal_id.as_ref().and_then(|_| self.render_embedded_terminal(AUTH_TERMINAL_KEY));
        auth_card::auth_card(provider, auth.error.as_deref(), rows, terminal, theme, &typo, density)
    }

    /// The EnvVar-auth submit button: visually a [`Self::auth_pill`], but on click
    /// it reads the typed secrets and respawns the agent WITH them in its env
    /// (rather than authenticating the current, env-less process). Renders a muted
    /// "Connecting…" while that respawn is in flight.
    fn env_submit_pill(&self, method_id: &str, pending: bool, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Connecting…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.submit_env_auth(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("acp-env-auth-{method_id}"),
            "Sign in".to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// Ensure the masked secret fields match the current EnvVar-auth prompt: one
    /// [`InputState`] per advertised variable while an EnvVar method is up, torn
    /// down once the card clears or turns non-EnvVar. Lives here (not the event
    /// fold) because `InputState::new` needs the `Window`. Rebuilds only when the
    /// variable set actually changes, so a half-typed secret survives repaints.
    fn reconcile_env_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Variables the current card wants, in advertised order (first EnvVar method).
        let wanted: Vec<String> = self
            .auth
            .as_ref()
            .and_then(|a| {
                a.methods.iter().find_map(|m| match &m.kind {
                    AuthMethodKind::EnvVar { vars, .. } => Some(vars.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        // Already in sync (same vars, same order) → keep the live fields untouched.
        if self.env_inputs.len() == wanted.len()
            && self.env_inputs.iter().zip(&wanted).all(|((name, _), w)| name == w)
        {
            return;
        }
        self.env_inputs.clear();
        self.env_input_subs.clear();
        for (i, name) in wanted.iter().enumerate() {
            let input =
                cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(name.clone()));
            // Repaint the chat view on edits so the masked dots appear live (an
            // embedded `Input` doesn't self-repaint its owner).
            let sub = cx.subscribe(&input, |_this, _input, _ev: &InputEvent, cx| cx.notify());
            // Focus the first field so the user can type without clicking first. The
            // render-time composer-focus fallback only fires when the ROOT holds
            // focus, so this stays put.
            if i == 0 {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            self.env_inputs.push((name.clone(), input));
            self.env_input_subs.push(sub);
        }
    }

    /// Collect the typed EnvVar-auth secrets and respawn the agent WITH them in its
    /// environment (which then authenticates) — the only way an env-credentialed
    /// agent can sign in, since a running process can't gain env after it spawned.
    /// The values are read straight into the respawn's in-flight `ConnectSpec.env`
    /// and never persisted. Blank fields are skipped (the agent re-prompts if it
    /// actually needed them); an all-blank submit keeps the card up with a nudge.
    fn submit_env_auth(&mut self, method_id: String, cx: &mut Context<Self>) {
        let env: Vec<(String, String)> = self
            .env_inputs
            .iter()
            .filter_map(|(name, input)| {
                let value = input.read(cx).value().to_string();
                (!value.is_empty()).then(|| (name.clone(), value))
            })
            .collect();
        if env.is_empty() {
            if let Some(auth) = self.auth.as_mut() {
                auth.error = Some("enter the required value(s) to continue".to_string());
            }
            cx.notify();
            return;
        }
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id.clone());
            auth.error = None;
        }
        // The fresh connection emits SessionInit (→ card clears) on success, or
        // AuthRequired again (→ card re-shows) if the credential was wrong.
        self.respawn_with_env(env, Some(method_id), cx);
        cx.notify();
    }

    /// Start a fresh conversation in this tab without closing it (the CLI's
    /// `/clear`). Blanks the transcript, drops any transient UI bound to it, and
    /// respawns a **non-resumed** session (the cleared thread has no session id,
    /// so `respawn` starts clean and reaps the old child). A fresh session mints
    /// its own id on the first turn, so the tab persists empty until then.
    fn new_chat(&mut self, cx: &mut Context<Self>) {
        // A rewind in flight will, on completion, overwrite this tab's session id
        // with its forked id and respawn again — which would silently resurrect
        // the discarded conversation into the "blank" new chat. Refuse until it
        // settles (mirrors every other rewind-adjacent entry point).
        if self.rewinding {
            return;
        }
        // A never-bound *New Agent* draft is ALREADY a fresh conversation, so
        // there is nothing to clear: `bind_now` drops `unbound` before the first
        // message can land, which makes an empty transcript an invariant here,
        // and no child exists to reap.
        //
        // The reason this is a guard and not just an optimization: `respawn`
        // below would spawn a subprocess while `unbound` stayed true, leaving a
        // live connection the view still treats as a draft — it would keep
        // offering the pre-bind agent picker and static model list for a session
        // already advertising its real capabilities, and the spawn is thrown away
        // by `bind_now`'s own respawn on the first real send. Binding here
        // instead would be worse: the transport is deliberately choosable right
        // up until that first send, and `/clear` is not a request to commit to an
        // agent.
        if self.unbound {
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
        // A fresh conversation should re-title from its own first message — clear
        // the one-shot guard (and drop any in-flight generation) so the tab isn't
        // stuck on the previous topic's title.
        self.title_generated = false;
        self.title_task = None;
        // Respawn reads the now-`None` session id → a fresh session, and clears
        // `disconnected`/`interrupted`/`last_error` itself on success.
        self.respawn(cx);
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Interrupt the streaming turn (the composer's Stop button). Asks the agent
    /// to end the turn over its own protocol, finalizes the transcript, and
    /// fail-closes any pending approval, then marks the session
    /// **resumable-idle**: the next send respawns via `--resume`. Not marked
    /// `disconnected` — the stop was intentional, so no error banner is shown.
    ///
    /// The respawn is now belt-and-braces rather than required: a protocol
    /// interrupt leaves the process alive and the session usable, so a later
    /// change could send straight into the live child and skip it. Left in place
    /// because dropping it changes the resume path for every backend, not just
    /// Claude.
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
        let mut label = self
            .unbound_agent_display(cx)
            .unwrap_or_else(|| self.backend.provider_display_name().to_string());
        // A worktree-bound draft (see `start_worktree_then_send`) folds its
        // branch into the same label, read once here at bind time — no new
        // poller, matching the "read once at create + on workspace activation"
        // requirement.
        if let Some(branch) = &self.worktree_branch_label {
            label = format!("{label} · {branch}");
        }
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
        self.respawn_with_env(Vec::new(), None, cx);
    }

    /// Like [`Self::respawn`], but seeds the fresh connection with extra `env`
    /// overrides and (optionally) a method to auto-`authenticate` — the EnvVar-auth
    /// flow: the user's typed credentials reach the newly spawned agent's
    /// environment, then it signs in without re-prompting. Plain [`Self::respawn`]
    /// is this with no extra env and no auto-authenticate, so the Stop→resume and
    /// live-switch paths are unchanged. The `env` values are held only in the
    /// in-flight `ConnectSpec` — never written to the persisted transcript.
    fn respawn_with_env(
        &mut self,
        env: Vec<(String, String)>,
        auth_method: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Reap the old connection before replacing it — `Child`'s Drop neither
        // kills nor waits. A Stop now interrupts the turn over the protocol
        // rather than signalling the process, so after one the child is still
        // *alive* and this is what ends it; it also harvests a child that died
        // on its own.
        if let Some(old) = self.connection.take() {
            old.shutdown();
        }
        let spec = self.respawn_spec(env, auth_method);
        match computer_use::connect_declaring(spec, &self.screen_control, cx) {
            Ok((conn, rx)) => {
                self.connection = Some(conn);
                // Re-expose the respawned session to remote clients under the same
                // stable id (drops the old binding, registers the fresh connection).
                self.bind_remote(cx);
                // Reassigning drops the old drain task, cancelling its foreground
                // half; its forwarder thread then exits on the dead child's
                // stdout EOF. We're single-threaded here, so no stale
                // `on_disconnect` can interleave onto the fresh connection.
                self._drain_task = Some(Self::spawn_drain(rx, cx));
                self.interrupted = false;
                self.disconnected = false;
                self.thread.last_error = None;
                // Seed the context meter's denominator from a backend that
                // knows its window without a turn (Pi: at handshake). A
                // dormant restore first connects HERE, not in `assemble` —
                // without this the meter is empty until a turn completes.
                self.thread.last_known_context_window = self
                    .connection
                    .as_ref()
                    .and_then(|c| c.context_window())
                    .or(self.thread.last_known_context_window);
                // This is the FIRST connection for a deferred-bound *New Agent*
                // draft, so its palette metadata arrives here or not at all.
                let (composer, cwd) = (self.composer.clone(), self.cwd.clone());
                push_slash_catalog(self.connection.as_deref(), &composer, &cwd, cx);
            }
            Err(e) => {
                // The old connection was already shut down above and the respawn
                // failed, so this session is dead — drop its remote binding rather
                // than leave the registry advertising a session backed by a killed
                // connection (`on_disconnect` isn't on this path).
                self.unbind_remote();
                self.thread.last_error = Some(format!("Failed to resume agent: {e}"));
                self.disconnected = true;
                self.interrupted = false;
                // A synchronous spawn failure is terminal for this attempt, so drop
                // any auth card — otherwise the tail-card chain (disconnected before
                // auth) would render the error card while the auth prompt lingered in
                // state. The error card's Retry re-runs a plain respawn, which yields
                // a fresh AuthRequired if the agent still needs login. No-op for the
                // ordinary (non-auth) respawn, where `auth` is already `None`.
                self.auth = None;
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
        // Direct field write — outside the thread's revision counter.
        self.meta_dirty.set(true);
        self.thread.model = Some(model.clone());
        // On an unbound draft there's no subprocess to respawn — just record the
        // pick and re-seed so the picker's checkmark moves. The choice binds when
        // the first message spawns the agent.
        if self.unbound {
            self.sync_unbound_composer(cx);
            cx.notify();
            return;
        }
        // Prefer an in-session model switch (an ACP agent maps a model pick to
        // its `Model`-category config option); fall back to the resume-respawn
        // path when the backend fixes `--model` at spawn (Claude/Codex).
        // Respawning an ACP child would drop the live session.
        let switched_live = self
            .connection
            .as_ref()
            .is_some_and(|c| c.set_model(&model).is_ok());
        if !switched_live {
            self.respawn(cx);
        } else if let Some(w) = self.connection.as_ref().and_then(|c| c.context_window()) {
            // The window is per-model and can differ a lot (272K vs 128K), so a
            // live switch must move the meter's denominator with it — otherwise
            // it keeps measuring against the model the user just left.
            self.thread.last_known_context_window = Some(w);
        }
        self.sync_composer(cx); // reflect the new model in the toolbar label
        // Persist only for spawn-fixed backends: an ACP model is a session-local
        // config value the spawn ignores on restore (mirrors the mode path, which
        // also switches live and isn't persisted).
        if !switched_live {
            cx.emit(AgentChatEvent::ModelChanged(model));
        }
        // Same reason as the mode path: a remote picker re-reads immediately, and
        // an in-place switch produces no event to carry the new value out.
        self.publish_remote_meta();
        cx.notify();
    }

    /// Switch the permission mode for this chat tab **in place** — no respawn on
    /// either backend now: Claude writes a `set_permission_mode` control request
    /// on stdin (the Agent SDK's wire), ACP calls `session/set_mode`; both return
    /// `Ok` from `set_mode`, so the same PID/session keeps running. The
    /// resume-respawn is only the fallback when `set_mode` fails (an older CLI /
    /// a backend that NAKs the request). Not persisted (see the field note).
    /// No-op when the mode is unchanged.
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
        // The blob's `choices.current_mode` reads this pick.
        self.meta_dirty.set(true);
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
        // Push it out now rather than waiting for the next event batch: a remote
        // picker re-reads the session's choices the moment its change is
        // acknowledged, and an in-place switch produces no event to ride on — so
        // without this the phone re-reads the mode it just left.
        self.publish_remote_meta();
        cx.notify();
    }

    /// Switch the reasoning effort for this chat tab. Two backends, two paths:
    /// Claude fixes `--effort` at spawn, so a live switch respawns resumed on the
    /// new level; an ACP agent switches in-session via its `ThoughtLevel` config
    /// option, so its `set_effort` succeeds and we skip the respawn (respawning an
    /// ACP child would drop the live session). Not persisted, so no host event is
    /// raised. No-op when unchanged.
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
        self.effort = Some(effort.clone());
        // Prefer an in-session runtime switch (ACP); fall back to the resume-respawn
        // path when the backend fixes `--effort` at spawn (Claude's `set_effort` bails).
        let switched_live = self.connection.as_ref().is_some_and(|c| c.set_effort(&effort).is_ok());
        if !switched_live {
            self.respawn(cx);
        }
        self.sync_composer(cx); // reflect the new effort in the toolbar label
        cx.notify();
    }

    /// Apply a generic feature-control change (a toggle flip or a select pick).
    /// Prefers a live in-session write (ACP `set_config` via the backend's
    /// `set_feature`); a backend that fixes the feature at spawn falls back to a
    /// resume-respawn. No-op pre-bind. The new value surfaces on the next
    /// `sync_composer` — the backend re-advertises it through `features()`.
    fn change_feature(&mut self, id: String, value: FeatureValue, cx: &mut Context<Self>) {
        // Unreachable pre-bind (the feature cluster is hidden on an unbound
        // draft), but guard so a stray pick can't early-spawn the subprocess.
        if self.unbound {
            return;
        }
        // Remember the pick optimistically so the control reflects it at once,
        // even when the backend applies the change without echoing it back.
        // The blob's codex/pi posture snapshots read these picks.
        self.meta_dirty.set(true);
        self.feature_values.insert(id.clone(), value.clone());
        let switched_live = self
            .connection
            .as_ref()
            .is_some_and(|c| c.set_feature(&id, value.clone()).is_ok());
        if !switched_live {
            self.respawn(cx);
        }
        self.sync_composer(cx); // reflect the new value in the toolbar
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
        connection: Arc<dyn AgentConnection>,
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
        // Mirror `new`: seed the palette's command metadata from the backend, so
        // a test exercises the same path the real constructor takes.
        push_slash_catalog(Some(connection.as_ref()), &composer, std::path::Path::new(""), cx);
        // No thread yet in this constructor, so the placeholder is correct: a
        // test view has never run and so has no agent session id to key on.
        let remote_session_id = remote_session_id_for(None);
        let remote = cx
            .try_global::<RemoteControl>()
            .and_then(|rc| rc.bind(&remote_session_id, connection.clone()));

        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            remote_session_id,
            remote,
            backend: ChatBackend::stream_json(),
            composer,
            session_detail_open: false,
            last_notify: std::time::Instant::now(),
            flush_scheduled: false,
            probed_catalogs: HashMap::new(),
            // The injected StubConnection is the whole point: no subprocess.
            probe_catalogs_live: false,
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
            screen_control: ScreenControl::new(&PathBuf::new()),
            screen_prompts: HashMap::new(),
            cwd: PathBuf::new(),
            model: None,
            permission_mode: None,
            effort: None,
            feature_values: HashMap::new(),
            disconnected: false,
            interrupted: false,
            dormant: false,
            publish_throttle: publish_throttle::PublishThrottle::new(),
            last_saved_revision: std::cell::Cell::new(u64::MAX),
            meta_dirty: std::cell::Cell::new(false),
            // The test injects a live connection, so this chat is already bound.
            unbound: false,
            unbound_agent_id: None,
            view_mode: ChatViewMode::Chat,
            terminal: None,
            companion_session: None,
            chat_advanced_since_companion: false,
            _terminal_observer: None,
            expanded_thinking: HashSet::new(),
            collapsed_thinking: HashSet::new(),
            thinking_level: ThinkingLevel::default(),
            expanded_tool_calls: HashSet::new(),
            expanded_tool_runs: HashSet::new(),
            image_cache: ImageCache::new(),
            preview: None,
            open_tool_sheet: None,
            sheet_copied: false,
            _sheet_copy_task: None,
            _drain_task: None,
            _remote_prompt_task: None,
            _remote_choice_task: None,
            remote_choice_tx: None,
            _subscriptions: Vec::new(),
            question_cards: HashMap::new(),
            question_card_subs: HashMap::new(),
            embedded_terminals: HashMap::new(),
            embedded_terminal_subs: HashMap::new(),
            env_inputs: Vec::new(),
            env_input_subs: Vec::new(),
            auth: None,
            checkpoint_engine: None,
            pre_turn_checkpoint: None,
            rewind_confirm: None,
            rewinding: false,
            rewind_then_send: None,
            pending_edit: None,
            pane_group: None,
            remote_tab_title: None,
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
            title_generated: false,
            title_task: None,
            is_git_project: false,
            worktree_draft_enabled: false,
            worktree_slug_input: None,
            _worktree_slug_sub: None,
            worktree_create_state: roster::WorktreeCreateState::default(),
            pending_worktree_send: None,
            worktree_branch_label: None,
            import_bridge: None,
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

    /// Test-only: the inverse of [`Self::make_unbound_for_test`] — mark the draft
    /// as bound the way a successful first send does, without spawning anything.
    /// The stub connection the test harness injects stands in for the real one.
    #[cfg(test)]
    fn make_bound_for_test(&mut self) {
        self.unbound = false;
        self.unbound_agent_id = None;
    }

    /// Test-only: override `is_git_project` — the real constructor derives it
    /// from a `.git` stat on `cwd`, which a `#[gpui::test]`'s throwaway path
    /// never has, so tests that need the worktree-toggle to render set it here.
    #[cfg(test)]
    fn set_git_project_for_test(&mut self, is_git: bool) {
        self.is_git_project = is_git;
    }

    #[cfg(test)]
    fn worktree_draft_enabled_for_test(&self) -> bool {
        self.worktree_draft_enabled
    }

    #[cfg(test)]
    fn worktree_create_state_for_test(&self) -> &roster::WorktreeCreateState {
        &self.worktree_create_state
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

    /// Test-only: whether a subprocess connection exists at all — distinct from
    /// [`Self::is_bound_for_test`], which also requires the view to *know* it's
    /// bound. The gap between the two is exactly the bug `/clear` used to cause.
    #[cfg(test)]
    fn connection_is_live_for_test(&self) -> bool {
        self.connection.is_some()
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
                // Drain whatever else is already queued behind this event and
                // apply the lot as one batch: under a burst (a fast model's
                // token stream) this collapses N repaints into one for free,
                // with no added latency — these events had all arrived anyway.
                let mut batch = vec![ev];
                while let Ok(queued) = fwd_rx.try_recv() {
                    batch.push(queued);
                }
                if this.update(cx, |view, cx| view.apply_batch(batch, cx)).is_err() {
                    return; // view dropped
                }
            }
            let _ = this.update(cx, |view, cx| view.on_disconnect(cx));
        })
    }

    /// The sender for this tab's choice relay, starting the relay on first use.
    ///
    /// One relay per view for its whole life, rather than one per binding: a
    /// respawn rebinds while the relay is mid-change, and a relay that were
    /// replaced there would lose the reply for the pick that triggered it.
    fn choice_relay_sender(
        &mut self,
        cx: &mut Context<Self>,
    ) -> futures::channel::mpsc::UnboundedSender<RemoteChoice> {
        if let Some(tx) = &self.remote_choice_tx {
            return tx.clone();
        }
        let (tx, rx) = futures::channel::mpsc::unbounded();
        self._remote_choice_task = Some(Self::spawn_remote_choice_relay(rx, cx));
        self.remote_choice_tx = Some(tx.clone());
        tx
    }

    /// Drain model/permission-mode changes the backend refused in-session and
    /// apply each through this tab's own picker path, which respawns the child
    /// resumed on the new pick when the backend fixes the value at spawn.
    ///
    /// Routing through `change_model`/`change_permission_mode` rather than
    /// reimplementing the respawn is the point: a remote pick and a local one then
    /// cannot drift, and everything those paths already handle — persisting the
    /// choice, moving the context-window denominator, re-seeding the composer —
    /// happens either way.
    fn spawn_remote_choice_relay(
        mut rx: futures::channel::mpsc::UnboundedReceiver<RemoteChoice>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(RemoteChoice { kind, value, reply }) = rx.next().await {
                // `is_ok` is the honest answer this channel can give: the pick
                // reached a live view and was applied. A respawn that then fails
                // to start surfaces in the transcript as an agent error, which the
                // phone is already subscribed to — it is not something to report
                // here as a refused pick.
                let applied = this
                    .update(cx, |view, cx| match kind {
                        ChoiceKind::Model => view.change_model(value, cx),
                        ChoiceKind::PermissionMode => view.change_permission_mode(value, cx),
                    })
                    .is_ok();
                let _ = reply.send(applied);
            }
        })
    }

    /// Fold a prompt that arrived over remote control into this tab's transcript.
    /// The host already forwarded it to the backend and ingested a synthetic copy
    /// for other subscribers, so this only pushes the bubble locally — it must NOT
    /// re-tee (that would double the prompt on the phone), mirroring how a
    /// desktop-typed prompt bubbles optimistically without folding the echo again.
    /// Fold a drained batch into the thread, then repaint once for the whole
    /// batch instead of once per event.
    ///
    /// Every event is applied immediately — only the repaint is deferred — so
    /// the thread state, persistence and rewind see no difference. A batch of
    /// nothing but deltas is rate-limited (the user cannot read faster than
    /// [`NOTIFY_INTERVAL`], and each repaint re-parses the whole streaming
    /// message); anything else paints at once.
    fn apply_batch(&mut self, batch: Vec<ThreadEvent>, cx: &mut Context<Self>) {
        let all_delta = batch.iter().all(ThreadEvent::is_delta);
        for ev in batch {
            // Tee each event to any remote subscribers (gated: `remote` is `Some`
            // only while remote control is enabled, so this clone never runs on a
            // disabled desktop). The desktop UI keeps its own dedicated channel —
            // this fan-out is parallel and never in the UI's path.
            if let Some(binding) = &self.remote {
                binding.ingest(ev.clone());
            }
            self.apply_event(ev, cx);
        }
        // A fresh chat is registered under a placeholder id until the agent mints
        // its own; once the fold has one, move the session onto it. Checked after
        // the batch rather than at the event that carries the id, because which
        // event that is differs per backend and missing it would strand the
        // session under a placeholder — an id no later run can resolve.
        self.rekey_remote_session_if_needed(cx);
        // Republish title/model after the fold has applied the batch, so a remote
        // session list shows what this tab shows (a `TitleUpdated` or a model swap
        // lands in the same batch). Done here, at the one place every event passes
        // through, rather than at each site that can change them.
        self.publish_remote_meta();
        if all_delta {
            self.notify_throttled(cx);
        } else {
            // A settled event landed (message, tool result, turn end): refresh the
            // published transcript so a newly-opening remote client's authoritative
            // history is current. Gated with the non-delta repaint so it never runs
            // on the per-token hot path, and coalesced on top of that — a turn
            // making twenty tool calls otherwise re-serialized the whole fold
            // twenty times. See `publish_throttle` for why a revision gate (the
            // shape persistence uses) cannot skip anything here.
            self.publish_remote_transcript_throttled(cx);
            self.notify_now(cx);
        }
    }

    /// Repaint now, standing down any queued trailing repaint.
    fn notify_now(&mut self, cx: &mut Context<Self>) {
        self.last_notify = std::time::Instant::now();
        self.flush_scheduled = false;
        cx.notify();
    }

    /// Repaint at most once per [`NOTIFY_INTERVAL`] while streaming.
    ///
    /// When the budget isn't up yet, queue a single trailing repaint rather
    /// than skipping: the last few streamed characters would otherwise sit
    /// invisible until whatever event came next — and at the end of a turn's
    /// text that could be a long wait.
    fn notify_throttled(&mut self, cx: &mut Context<Self>) {
        let since = self.last_notify.elapsed();
        if since >= NOTIFY_INTERVAL {
            self.notify_now(cx);
            return;
        }
        if self.flush_scheduled {
            return; // a trailing repaint is already queued; it will show this too
        }
        self.flush_scheduled = true;
        let delay = NOTIFY_INTERVAL.saturating_sub(since);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |view, cx| {
                // Cleared if something painted in the meantime — that paint
                // already showed these deltas.
                if view.flush_scheduled {
                    view.notify_now(cx);
                }
            });
        })
        .detach();
    }

    /// Test-only: deliver a single event the way the drain would. Production
    /// always arrives via [`Self::apply_batch`], which this routes through so
    /// tests exercise the real path rather than a parallel one.
    #[cfg(test)]
    fn on_event(&mut self, ev: ThreadEvent, cx: &mut Context<Self>) {
        self.apply_batch(vec![ev], cx);
    }

    /// Fold one decoded event into the thread. Never repaints — the repaint is
    /// the batch's call to make ([`Self::apply_batch`]), once for all its
    /// events.
    fn apply_event(&mut self, mut ev: ThreadEvent, cx: &mut Context<Self>) {
        // Tag a screen-control request before the fold, so the card the fold
        // builds is already the consent one. Purely a classification — whether
        // it is *allowed* is the policy's business, below.
        if let ThreadEvent::PermissionRequested { tool_name, kind, .. } = &mut ev
            && oximux_agent_core::screen_tools::is_computer_use_tool(tool_name)
        {
            *kind = oximux_agents::thread::PermissionKind::Screen;
        }
        let was_active = self.thread.turn_active;
        self.thread.apply(&ev);
        self.note_screen_activity(&ev);
        // Screen-control calls are decided here because this is the only point
        // OxiMux is in their path at all — the driver is a separate process the
        // agent talks to directly. Runs after the fold so the card exists to be
        // resolved, and answers nothing that isn't a screen-control tool.
        if let ThreadEvent::PermissionRequested { request_id, tool_use_id, tool_name, input, .. } =
            &ev
        {
            self.enforce_screen_control(
                tool_name.clone(),
                input.clone(),
                request_id.clone(),
                tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                cx,
            );
        }
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
                // A provider-native title wins: mark titled so a later haiku
                // generation (if one ever races for this transport) can't clobber it.
                self.title_generated = true;
                cx.emit(AgentChatEvent::TitleChanged(title.clone()));
            }
            // The agent needs login: mount/refresh the auth card. A retained
            // terminal id (mid terminal-login) survives a re-emit carrying an
            // error note, so the login terminal keeps rendering. A retained
            // `pending` likewise survives a re-emit so an in-flight sign-in
            // (Codex's 401-retry burst re-emits AuthRequired several times per
            // turn) doesn't reset the "Opening browser…"/"Authenticating…"
            // spinner back to a clickable pill mid-login — but only when the
            // pending method is still advertised.
            ThreadEvent::AuthRequired { methods, error } => {
                let prev = self.auth.as_ref();
                let terminal_id = prev.and_then(|a| a.terminal_id.clone());
                let pending = prev
                    .and_then(|a| a.pending.clone())
                    .filter(|id| methods.iter().any(|m| &m.id == id));
                self.auth = Some(auth_card::AuthPrompt {
                    methods: methods.clone(),
                    // A fresh error note wins; else keep the prior one so a retry
                    // burst carrying `error: None` doesn't clear a real failure.
                    error: error.clone().or_else(|| prev.and_then(|a| a.error.clone())),
                    pending,
                    terminal_id,
                });
            }
            // A terminal-kind method launched its login command — bind the inline
            // terminal so `reconcile_embedded_terminals` mounts it in the card.
            ThreadEvent::AuthTerminal { terminal_id } => {
                if let Some(auth) = self.auth.as_mut() {
                    auth.terminal_id = Some(terminal_id.clone());
                }
            }
            // The worker produced the sign-in URL (Codex) → open it in the system
            // browser. The card stays pending until `AuthOutcome` resolves it.
            ThreadEvent::AuthUrl { url } => {
                crate::shell::open_url::open_url(url, cx);
            }
            // A browser OAuth sign-in resolved (Codex). Success → drop the card so
            // the composer re-enables (it's disabled while `self.auth.is_some()`)
            // and the user can send; the backend now has credentials. Failure →
            // re-show the card with the error so the user can retry.
            ThreadEvent::AuthOutcome { success, error } => {
                if *success {
                    self.auth = None;
                } else if let Some(auth) = self.auth.as_mut() {
                    auth.pending = None;
                    auth.error = error.clone().or(Some("Sign-in was not completed".into()));
                }
            }
            // The session opened → auth is done; drop the card.
            ThreadEvent::SessionInit { .. } => {
                self.auth = None;
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
        // Raise an attention signal for a live turn edge the user should hear
        // about while looking elsewhere. The host applies the focus/visibility/
        // per-kind gates — this only classifies the edge. Gated on `was_active`
        // for the finished/errored kinds so a stray no-turn result can't banner,
        // and suppressed for an intentional Stop (an interrupt isn't a failure).
        if let Some((kind, body)) = attention_for_event(&ev, was_active, self.interrupted) {
            cx.emit(AgentChatEvent::AttentionNeeded { kind, body });
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
    /// (Re)bind this session into the remote-control registry: drop any prior
    /// binding, then register the current connection under [`Self::remote_session_id`]
    /// — but only while remote control is enabled, so a disabled desktop registers
    /// nothing and clones no events. Called on connect and on respawn.
    /// Move this session onto the agent's own id once the agent has minted one.
    ///
    /// A chat that has never run is registered under a positional placeholder,
    /// which is worthless to a remote client the moment the desktop restarts.
    /// The agent's id names the conversation instead, so the session is re-keyed
    /// onto it at the first opportunity — which is also the first moment it has
    /// any history worth reaching from another device.
    ///
    /// Re-registering under the new id restarts `seq` at 1, so the old id is
    /// dropped only after the new one is live: a client subscribed to the
    /// placeholder sees its stream end and resubscribes, rather than sitting on a
    /// cursor no future event will ever exceed.
    fn rekey_remote_session_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(agent_id) = self.thread.session_id.clone() else {
            return;
        };
        if agent_id.is_empty() || agent_id == self.remote_session_id {
            return;
        }
        let previous = std::mem::replace(&mut self.remote_session_id, agent_id);
        // `bind_remote` registers under the field just replaced; the old entry is
        // removed afterwards so the two never coexist beyond this call.
        self.bind_remote(cx);
        if let Some(rc) = cx.try_global::<RemoteControl>() {
            rc.unregister(&previous);
        }
    }

    fn bind_remote(&mut self, cx: &mut Context<Self>) {
        // Deliberately does NOT unregister first. On a respawn this runs again for
        // the same session id, and `register` swaps the backend in place — keeping
        // `seq` monotonic so a subscribed phone keeps receiving. Unregistering here
        // would mint a fresh handle at seq 1, which every subscriber would silently
        // discard as already-seen. Teardown paths still call `unbind_remote`.
        let bound = self
            .connection
            .clone()
            .and_then(|conn| {
                cx.try_global::<RemoteControl>().and_then(|rc| rc.bind(&self.remote_session_id, conn))
            });
        match bound {
            Some(binding) => {
                self.remote = Some(binding);
                // Expose the current transcript at once — on a restart this fold was
                // restored from disk and never entered the event ring, so a phone
                // opening the session before the next event would otherwise see it
                // empty. Meta too, so the row is labelled from the first list.
                self.publish_remote_meta();
                self.publish_remote_transcript();
                // Re-arm the remote-prompt echo relay against the new binding.
                let (tx, rx) = futures::channel::mpsc::unbounded();
                let (event_tx, event_rx) = futures::channel::mpsc::unbounded();
                if let Some(binding) = &self.remote {
                    binding.set_prompt_sink(tx);
                    binding.set_event_sink(event_tx);
                }
                self._remote_prompt_task = Some(Self::spawn_remote_prompt_relay(rx, event_rx, cx));
                // Point the *existing* choice relay at the new binding rather than
                // starting a fresh one. This path runs inside `change_model` →
                // `respawn`, so the relay is mid-flight on the very pick that
                // caused it: replacing the task here would drop that future and
                // cancel its reply, telling the phone a change it just made had
                // failed. Re-registering the same sender is enough — a rebind may
                // mint a new handle (after an unbind), and this points it back.
                let choice_tx = self.choice_relay_sender(cx);
                if let Some(binding) = &self.remote {
                    binding.set_choice_sink(choice_tx);
                }
            }
            // No connection, or remote is disabled — drop any prior binding.
            None => self.unbind_remote(),
        }
    }

    /// Drop this session's registry binding (on disconnect / teardown). No-op when
    /// unbound. Explicit because the registry retains its own handle `Arc`, so
    /// dropping the view's handle alone would not evict the session.
    fn unbind_remote(&mut self) {
        if let Some(binding) = self.remote.take() {
            binding.unregister(&self.remote_session_id);
        }
    }

    /// Push this tab's title + effective model into the registry so a remote
    /// session list renders them instead of the raw `agent-N` id. No-op when remote
    /// is disabled (`remote` is `None`); the registry skips unchanged values, which
    /// is the common case on a per-batch call.
    fn publish_remote_meta(&self) {
        let Some(binding) = &self.remote else {
            return;
        };
        binding.set_meta(SessionMeta {
            // The tab's visible title (a manual rename, else the running label) is
            // what the desktop shows, so it wins; fall back to the thread's
            // provider-native title until the pane group has synced one.
            title: self.remote_tab_title.clone().or_else(|| self.thread.title.clone()),
            model: self.effective_model(),
            permission_mode: self.effective_permission_mode(),
            // Git RPCs resolve their repository from this.
            cwd: Some(self.cwd.clone()),
        });
    }

    /// The model this tab is actually running under.
    ///
    /// The same shape as [`Self::effective_permission_mode`], and for the same
    /// reason: `model` is `None` until the user picks one, so a session that has
    /// never had a pick would otherwise publish nothing and a remote picker would
    /// render every model unselected — as if the session were running on no model
    /// at all. The desktop's own composer resolves it exactly this way, falling
    /// back to the connection's default when the tab holds no pick.
    ///
    /// The thread's negotiated model still wins: once the backend reports what it
    /// actually loaded, that is the truth, and it can differ from the default the
    /// child was launched with.
    fn effective_model(&self) -> Option<String> {
        self.thread
            .model
            .clone()
            .or_else(|| self.model.clone())
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_model()))
    }

    /// The permission mode this tab is actually running under.
    ///
    /// `permission_mode` is `None` for the backend's baseline — that is what makes
    /// `respawn` omit the flag — so the field alone cannot say what is in force.
    /// A remote picker needs the resolved answer: it holds no connection of its
    /// own to ask for the baseline.
    fn effective_permission_mode(&self) -> Option<String> {
        self.permission_mode
            .clone()
            .or_else(|| self.connection.as_ref().and_then(|c| c.default_mode()))
    }

    /// Record the visible tab title (a manual rename, else the running `Chat N` /
    /// agent label) the desktop shows for this chat, so a remote session list
    /// renders the same name instead of the raw `agent-N` id. Pushed by the owning
    /// pane group on tab create, rename, and ambient title change; re-publishes the
    /// registry meta only when the title actually changed.
    pub fn set_remote_tab_title(&mut self, title: Option<String>) {
        if self.remote_tab_title == title {
            return;
        }
        self.remote_tab_title = title;
        self.publish_remote_meta();
    }

    fn on_disconnect(&mut self, cx: &mut Context<Self>) {
        // The live process is gone: drop the remote binding so the phone's session
        // list reflects only live sessions (a resume respawns + re-binds).
        self.unbind_remote();
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

    /// The expander for a collapsed run: what the hidden cards did ("··· Edited 3
    /// files · ran 2 commands"), with any failures called out in the error tint so
    /// a broken call behind the fold isn't invisible. Falls back to the bare count
    /// when there is nothing to summarize.
    fn render_tool_run_expander(
        &self,
        run_start: usize,
        hidden: usize,
        summary: GroupSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let label = if summary.label.is_empty() {
            format!("··· {hidden} more tool calls")
        } else {
            format!("··· {}", summary.label)
        };
        let mut row = div()
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
            .child(SharedString::from(label));
        if summary.failed > 0 {
            row = row.child(
                div()
                    .flex_none()
                    .text_color(t.status_error)
                    .child(SharedString::from(format!("· {} failed", summary.failed))),
            );
        }
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _e, _w, cx| this.expand_tool_run(run_start, cx)),
        )
        .into_any_element()
    }

    /// Cycle the chat-wide thinking level (Hidden → Auto → Expanded → …), from
    /// the pill above the composer. Persisted via `transcript_snapshot`.
    fn cycle_thinking_level(&mut self, cx: &mut Context<Self>) {
        self.thinking_level = self.thinking_level.next();
        // Persisted on the transcript blob (view-held, outside the thread).
        self.meta_dirty.set(true);
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
        mut decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        // Idempotency guard: only answer a tool that is STILL awaiting. Once
        // answered its status leaves `WaitingForConfirmation` (below) and the
        // buttons drop on re-render, but this closes the sub-frame window where
        // a stray second click could send a second control_response for an
        // already-decided request_id.
        let awaiting = self.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc)
                if tc.id == tool_id
                    && matches!(&tc.status,
                        ToolCallStatus::WaitingForConfirmation(r) if r.request_id == request_id) =>
            {
                Some((tc.name.clone(), tc.input.clone()))
            }
            _ => None,
        });
        let Some((tool_name, tool_input)) = awaiting else {
            return;
        };
        // Answered, so the resolved target is dead weight — and leaving it would
        // make a later card for the same id name the wrong app.
        self.screen_prompts.remove(&tool_id);
        // Approving a screen-control call is what grants its target, and the
        // policy has the last word — a card can sit open a long time, and a
        // target another chat claimed meanwhile is refused however this one is
        // answered.
        if matches!(
            decision,
            PermissionDecision::Allow { .. } | PermissionDecision::AllowWithSuggestion { .. }
        ) && let Err(reason) = self.screen_control.approve(&tool_name, &tool_input)
        {
            decision = PermissionDecision::Deny { message: reason };
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

    /// Approve a plan-mode `ExitPlanMode` request: allow it (echoing the request
    /// input, required by the transport) plus a `setMode` suggestion so the CLI
    /// exits plan mode into `mode` and continues the same turn, then optimistically
    /// reflect the new mode in the composer chip. Claude sends no mode echo on the
    /// wire, so the chip is the source of truth until the next respawn. `mode` is
    /// `acceptEdits` (auto-accept edits) or `default` (ask before each edit).
    fn approve_plan(
        &mut self,
        tool_id: String,
        request_id: String,
        input: serde_json::Value,
        mode: &str,
        cx: &mut Context<Self>,
    ) {
        let suggestion = PermissionSuggestion {
            kind: "setMode".to_string(),
            label: format!("Always ({mode})"),
            raw: serde_json::json!({ "type": "setMode", "mode": mode, "destination": "session" }),
        };
        self.resolve_permission(
            tool_id,
            request_id,
            PermissionDecision::AllowWithSuggestion { updated_input: input, suggestion },
            cx,
        );
        self.set_mode_chip(mode, cx);
    }

    /// Reject a plan-mode `ExitPlanMode` request → the agent keeps planning (stays
    /// in plan mode; the turn continues without exiting).
    fn reject_plan(&mut self, tool_id: String, request_id: String, cx: &mut Context<Self>) {
        self.resolve_permission(
            tool_id,
            request_id,
            PermissionDecision::Deny { message: "Keep planning".into() },
            cx,
        );
    }

    /// Optimistically reflect a permission-mode change in the composer chip WITHOUT
    /// respawning — used when the backend flips the mode itself in-session (Claude's
    /// ExitPlanMode approve applies the `setMode` suggestion server-side and
    /// continues the same turn, so a respawn would needlessly drop it). Mirrors the
    /// baseline-normalization in `change_permission_mode` but skips the switch.
    fn set_mode_chip(&mut self, mode: &str, cx: &mut Context<Self>) {
        let default_mode = self
            .connection
            .as_ref()
            .and_then(|c| c.default_mode())
            .unwrap_or_default();
        self.permission_mode = (mode != default_mode).then(|| mode.to_string());
        self.sync_composer(cx);
        // A backend-driven flip is still a mode change a remote picker must see.
        self.publish_remote_meta();
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

    /// Mount / reap the inline `TerminalView`s for ACP tool calls that embed a
    /// terminal (`tc.terminal_id`), mirroring [`Self::reconcile_question_cards`].
    /// Runs each render (needs `window` to build the view) and is idempotent once
    /// a terminal is mounted. A tool call that leaves the transcript (e.g.
    /// `/clear`) drops its view and releases the PTY on the host so nothing leaks.
    fn reconcile_embedded_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut live: Vec<(String, String)> = self
            .thread
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::ToolCall(tc) => {
                    tc.terminal_id.as_ref().map(|t| (tc.id.clone(), t.clone()))
                }
                _ => None,
            })
            .collect();
        // The ACP auth login terminal mounts through the same path, under a
        // synthetic key so it's reaped when the auth card clears (or the tab does).
        if let Some(term_id) = self.auth.as_ref().and_then(|a| a.terminal_id.clone()) {
            live.push((AUTH_TERMINAL_KEY.to_string(), term_id));
        }
        let live_ids: HashSet<&String> = live.iter().map(|(id, _)| id).collect();
        // Reap terminals whose tool call is gone: release the PTY (by the host's
        // terminal id, NOT the tool id) + drop the view.
        let dropped: Vec<String> =
            self.embedded_terminals.keys().filter(|id| !live_ids.contains(id)).cloned().collect();
        for tool_id in dropped {
            if let Some((terminal_id, _view)) = self.embedded_terminals.remove(&tool_id) {
                acp_terminal_host::release_embedded(&terminal_id);
            }
            self.embedded_terminal_subs.remove(&tool_id);
        }
        // Mount any newly-embedded terminal on the PTY its host spawned. Use the
        // background mount variant so this render-time mount never yanks keyboard
        // focus off the composer (it fires mid-turn, with no user click).
        let (theme, density, typo) = (self.theme, self.density, self.typography.clone());
        let cwd_label = self.cwd.to_string_lossy().into_owned();
        for (tool_id, terminal_id) in live {
            if self.embedded_terminals.contains_key(&tool_id) {
                continue;
            }
            let Some((backend, term_id)) =
                acp_terminal_host::embedded_terminal_backend(&terminal_id)
            else {
                // Host not installed, or the terminal was already released.
                continue;
            };
            let ids = SurfaceIds::fresh(cwd_label.clone());
            let terminal = cx.new(|cx| {
                TerminalView::mount_background(
                    backend, term_id, ids, theme, density, typo.clone(), window, cx,
                )
            });
            let sub = cx.observe(&terminal, |_this, _tv, cx| cx.notify());
            self.embedded_terminals.insert(tool_id.clone(), (terminal_id, terminal));
            self.embedded_terminal_subs.insert(tool_id, sub);
        }
    }

    /// The inline terminal element for a tool call that embeds one, bounded to a
    /// fixed height (its own scrollback scrolls inside). `None` when the tool has
    /// no mounted terminal.
    fn render_embedded_terminal(&self, tool_id: &str) -> Option<AnyElement> {
        let (_terminal_id, terminal) = self.embedded_terminals.get(tool_id)?;
        let (theme, density) = (self.theme, self.density);
        Some(
            div()
                .mt(px(density.pad_row))
                .w_full()
                .h(px(EMBEDDED_TERMINAL_HEIGHT))
                .overflow_hidden()
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.border_inactive)
                .bg(theme.bg_base)
                .child(terminal.clone())
                .into_any_element(),
        )
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
            // The reply carries the REAL answer — the backend asked for it, and a
            // masked/redacted value would just fail whatever it feeds.
            let _ = conn.answer_question(&request_id, &questions, &answers);
        }
        // …but from here on the value is a credential we refuse to keep: flag the
        // call so the fold redacts the result the backend echoes back, before it
        // can reach the persisted transcript. Must happen before the status change
        // below, which drops the `AwaitingAnswer` that carries `is_secret`.
        if questions.iter().any(|q| q.is_secret) {
            self.thread.mark_secret_answer(&tool_id);
        }
        self.thread.set_tool_status(&tool_id, ToolCallStatus::InProgress);
        self.question_cards.remove(&tool_id);
        self.question_card_subs.remove(&tool_id);
        cx.notify();
    }

    /// How many of an entry's attachments could not be decoded — drawn as
    /// placeholder tiles so a picture that cannot be shown is visibly missing
    /// rather than absent. Reads the same memo [`Self::decoded_images`] fills,
    /// so it costs nothing beyond the decode already done.
    fn undecodable_images(&self, idx: usize, images: &[ChatImage]) -> usize {
        images.len() - self.decoded_images(idx, images).len()
    }

    /// A tile standing in for an attachment this build cannot draw (an encoding
    /// with no decoder, or bytes that do not match their declared type). Sized
    /// like a real thumbnail and deliberately not clickable — there is nothing
    /// to open.
    fn undecodable_tile(&self, entry_idx: usize, i: usize) -> AnyElement {
        let theme = self.theme;
        div()
            .id(SharedString::from(format!("img-undecodable-{entry_idx}-{i}")))
            .w(px(200.0))
            .h(px(150.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(self.density.r_card))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .child(
                div()
                    .text_size(px(self.typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("Image can't be displayed"),
            )
            .into_any_element()
    }

    /// Decoded thumbnails for a user entry's attached images, memoized in
    /// [`Self::image_cache`] by the stable (entry, image) position so a streaming
    /// repaint never re-decodes base64. Attachments that cannot be decoded are
    /// skipped — this list is what the lightbox pager indexes into, so it must
    /// hold only images that can actually be opened; the gap is drawn separately
    /// by [`Self::undecodable_images`].
    fn decoded_images(&self, idx: usize, images: &[ChatImage]) -> Vec<Arc<Image>> {
        let mut out = Vec::with_capacity(images.len());
        for (i, chat) in images.iter().enumerate() {
            if let Some(arc) =
                self.image_cache.get_or_decode((idx, i), || image_attach::decode_render(chat))
            {
                out.push(arc);
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
        let undecodable = self.undecodable_images(idx, images);
        let mut col = div().flex().flex_col().items_end().w_full().gap(px(6.0));
        if !decoded.is_empty() || undecodable > 0 {
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
            for i in 0..undecodable {
                thumbs = thumbs.child(self.undecodable_tile(idx, i));
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
        // Fork-to-new-tab is client-side (file-fork) only; a server-side rewind
        // backend (Codex) hides it (it still supports in-place Rewind + Edit).
        let fork_to_tab_server_side =
            self.connection.as_ref().is_some_and(|c| c.rewind_is_server_side());
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
                    // Rewind cancels the turn first. Client-side (Claude) only: a
                    // server-side backend (Codex) has no on-disk session log to
                    // fork into a separate tab.
                    .when(can_rewind && !self.thread.turn_active && !fork_to_tab_server_side, |row| {
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
            // A tool result that returned images (a `Read` of an image file, a
            // screenshot tool) — same lightbox pager as user-prompt images.
            Some(ThreadEntry::ToolCall(tc)) if !tc.images.is_empty() => {
                self.decoded_images(entry_idx, &tc.images)
            }
            _ => Vec::new(),
        }
    }

    /// Inline thumbnails for a tool result's images, each clickable to open the
    /// full-size lightbox (reusing the user-image preview path). `None` when the
    /// tool returned no images or none decoded.
    fn render_tool_result_images(
        &self,
        idx: usize,
        images: &[ChatImage],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let decoded = self.decoded_images(idx, images);
        if decoded.is_empty() {
            return None;
        }
        let theme = self.theme;
        let density = self.density;
        let mut thumbs = div().flex().flex_row().flex_wrap().gap(px(6.0)).mt(px(4.0));
        for (i, im) in decoded.iter().enumerate() {
            thumbs = thumbs.child(
                div()
                    .id(SharedString::from(format!("tool-img-{idx}-{i}")))
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
                        cx.listener(move |this, _e, _w, cx| this.open_image_preview(idx, i, cx)),
                    )
                    .child(
                        img(ImageSource::Image(im.clone()))
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    ),
            );
        }
        Some(thumbs.into_any_element())
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

    /// Open the fullscreen payload sheet on a tool call. The image lightbox and
    /// the sheet are mutually exclusive overlays — opening one closes the other.
    /// Focus moves to the view root so Escape dispatches from here (the overlay
    /// handlers' context) instead of being eaten by the composer input's IME —
    /// the same "focus the dialog on open" rule the find bar follows.
    fn open_tool_sheet(&mut self, tool_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = None;
        self.sheet_copied = false;
        self.open_tool_sheet = Some(tool_id);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the tool sheet (backdrop click, the ✕, or Escape) and return focus
    /// to the composer so typing resumes immediately (mirrors the find bar).
    fn close_tool_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_tool_sheet.take().is_some() {
            self.sheet_copied = false;
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    /// Flash the sheet's Copy control to "Copied ✓" for a beat.
    fn flash_sheet_copied(&mut self, cx: &mut Context<Self>) {
        self.sheet_copied = true;
        cx.notify();
        self._sheet_copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.sheet_copied {
                    view.sheet_copied = false;
                    cx.notify();
                }
            });
        }));
    }

    /// The live tool call backing the open sheet, looked up by id across the
    /// thread's tool calls each render (so a still-running tool grows in place).
    /// `None` if no sheet is open or the id is gone (e.g. after a rewind).
    fn open_sheet_tool_call(&self) -> Option<&ToolCall> {
        let id = self.open_tool_sheet.as_deref()?;
        self.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    /// The fullscreen tool-payload sheet, rendered over everything when
    /// [`Self::open_tool_sheet`] names a still-present tool call.
    fn render_tool_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tc = self.open_sheet_tool_call()?;
        Some(tool_sheet::render_tool_sheet(
            tc,
            self.sheet_copied,
            self.theme,
            self.density,
            &self.typography,
            cx,
        ))
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

    /// The width every transcript child is built at: the reading measure
    /// ([`CONTENT_MAX_W`]) on a roomy pane, or the pane itself once it is
    /// narrower (a split pane, a dragged-in window edge).
    ///
    /// This is resolved here, in the view, rather than left to
    /// `max_w(px(CONTENT_MAX_W))` on the children, because the children's text
    /// must be *measured* at the width it will *paint* at — see
    /// [`transcript_column`]. The scroll box's own width is the only reading of
    /// "how much room is there", and it is last frame's: a fresh view has no
    /// bounds yet and a resize lands one frame late, so fall back to the cap and
    /// let the next frame settle it. No feedback loop — the scroll box is
    /// full-width regardless of what its children ask for.
    fn content_width(&self) -> f32 {
        let painted = f32::from(self.list_scroll.bounds().size.width) - self.density.pad_panel * 2.0;
        if painted <= 0.0 { CONTENT_MAX_W } else { painted.min(CONTENT_MAX_W) }
    }

    /// The scrollable transcript column. Entries stack in a centered reading
    /// column ([`CONTENT_MAX_W`]) so wide windows don't stretch text edge-to-
    /// edge; the outer element only scrolls and centers.
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let content_w = self.content_width();
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
            // Same reasoning on the cross axis — see the note in `wrap_scroll`.
            .min_w_0()
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
            //
            // A sign-in requirement is surfaced BEFORE any session opens, so the
            // transcript is still empty when it lands — render the auth card here
            // too, or it would never reach the tail-card chain below (which only
            // runs once there are entries) and the empty greeting would shadow it.
            let body = if self.auth.is_some() {
                let card = self.render_auth_card(cx);
                transcript_column(content_w)
                    .child(card)
                    .into_any_element()
            } else {
                self.render_empty_hint(&theme, &typo)
            };
            return self.wrap_scroll(scroll.child(body)).into_any_element();
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
            .map(|e| matches!(e, ThreadEntry::ToolCall(tc) if must_stay_visible(tc)))
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
                        Some(question_card::render_settled(tc, theme, density, &typo).into_any_element())
                    } else if plan_panel::is_plan(tc) {
                        Some(plan_panel::render_plan_card(tc, theme, density, &typo).into_any_element())
                    } else {
                        let expanded = self.expanded_tool_calls.contains(&tc.id);
                        let card = tool_card::render_tool_card(
                            tc,
                            expanded,
                            self.provider_label(),
                            self.screen_context(tc),
                            theme,
                            density,
                            &typo,
                            cx,
                        );
                        // Append inline result-image thumbnails (a Read of an
                        // image, a screenshot) and/or an ACP embedded terminal
                        // below the card. Both are optional; a plain tool renders
                        // the bare card.
                        let thumbs = self.render_tool_result_images(idx, &tc.images, cx);
                        let terminal = self.render_embedded_terminal(&tc.id);
                        if thumbs.is_some() || terminal.is_some() {
                            let mut col = div().flex().flex_col().w_full().child(card);
                            if let Some(thumbs) = thumbs {
                                col = col.child(thumbs);
                            }
                            if let Some(terminal) = terminal {
                                col = col.child(terminal);
                            }
                            Some(col.into_any_element())
                        } else {
                            Some(card.into_any_element())
                        }
                    }
                }
                ThreadEntry::ContextCompaction { summary } => {
                    Some(compaction_divider(summary, theme, &typo).into_any_element())
                }
                // What the turn changed on disk, closing the turn. Review opens the
                // turn's own diff; it is offered only when the backend reported
                // one, since a derived summary has no hunks to show.
                ThreadEntry::TurnDiff { files, diff } => {
                    let on_review = diff.clone().map(|d| {
                        // Key the tab by the DIFF ITSELF, not by anything
                        // positional. An entry index is not an identity: it is
                        // scoped to one transcript, so two chats' first editing
                        // turn would both key "2" and one would silently
                        // reactivate the other's tab; and rewind/edit-resend
                        // truncate and repopulate from the same index, so a
                        // post-rewind turn would reactivate the pre-rewind tab.
                        // Both show the WRONG diff under the right label.
                        //
                        // Content-addressing makes a collision mean the content is
                        // identical, in which case reusing the tab is correct.
                        let key = diff_tab_key(&d);
                        Box::new(cx.listener(move |_this, _e: &ClickEvent, _w, cx| {
                            cx.emit(AgentChatEvent::ReviewTurnDiffRequested {
                                key: key.clone(),
                                diff: d.clone(),
                            });
                        })) as Box<_>
                    });
                    Some(turn_summary_card::render(files, theme, density, &typo, on_review))
                }
            };
            // A collapsed tool-run expander follows its anchor entry as its own child.
            let expander = match group_plan[idx] {
                EntryDisplay::ShowThenExpander { run_start, hidden } => {
                    // Summarize the cards the collapse HIDES — what's behind the
                    // fold is exactly what the user can't see for themselves. The
                    // run is the consecutive tool block from `run_start`, the same
                    // extent `plan_tool_grouping` collapsed.
                    let collapsed: Vec<GroupedTool> = (run_start..is_tool.len())
                        .take_while(|&i| is_tool[i])
                        .filter(|&i| matches!(group_plan[i], EntryDisplay::Hide))
                        .filter_map(|i| match self.thread.entries.get(i) {
                            Some(ThreadEntry::ToolCall(tc)) => Some(GroupedTool {
                                kind: ToolDetail::classify(&tc.name, tc.kind.as_deref(), &tc.input),
                                failed: matches!(tc.status, ToolCallStatus::Failed(_)),
                                target: bubble::tool_target(tc),
                                screen: screen_card::is_screen_call(&tc.name),
                            }),
                            _ => None,
                        })
                        .collect();
                    let summary = summarize_tool_run(&collapsed);
                    Some(self.render_tool_run_expander(run_start, hidden, summary, cx))
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
                    transcript_column(content_w).child(el);
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
                    transcript_column(content_w).child(expander),
                );
            }
        }
        // The agent's execution plan (ACP `Plan`) as a pinned checklist at the tail
        // of the transcript — one card, full-replaced on each `PlanUpdated`, kept
        // across turns until cleared. Reuses the `TodoWrite` checklist renderer.
        if let Some(entries) = self.thread.plan.as_ref().filter(|e| !e.is_empty()) {
            scroll = scroll.child(
                transcript_column(content_w)
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
                transcript_column(content_w)
                    .child(error_card::error_card(&msg, theme, &typo, retry)),
            );
        } else if self.auth.is_some() {
            // The agent needs login before a session can open — the auth card is
            // the only actionable state, so it takes precedence over the working
            // indicator and the plain error/signed-out cards BELOW it. (The
            // `disconnected` error card above wins over it, but a failed
            // EnvVar-auth respawn clears `self.auth`, so the two never coexist.)
            let card = self.render_auth_card(cx);
            scroll = scroll.child(
                transcript_column(content_w).child(card),
            );
        } else if self.thread.turn_active {
            // While a question card is pending, the agent isn't working — it's
            // blocked on the user's answer — so don't show the "working…" spinner
            // (it would also add height that pushes the card's controls down).
            if self.thread.pending_question().is_none() {
                // While compacting, show the specific "Compacting context…"
                // spinner instead of the generic "…is working…" so a long
                // compaction reads as progress, not a hang.
                let indicator = if self.thread.compacting {
                    compacting_indicator(theme, &typo)
                } else {
                    working_indicator(self.provider_label(), theme, &typo)
                };
                scroll = scroll.child(
                    transcript_column(content_w).child(indicator),
                );
            }
        } else if self.is_signed_out() && self.login_adapter_id().is_some() {
            // The turn settled (or errored) on an auth failure whose fix is a
            // terminal sign-in. Turn the dead-end reply into an action: a banner
            // that opens a terminal running the agent CLI, where `/login` works.
            // Takes precedence over the plain error card below since it's the
            // actionable version of the same state.
            let action = self.open_login_terminal_button(cx);
            scroll = scroll.child(
                transcript_column(content_w)
                    .child(login_card::login_card(self.provider_label(), theme, &typo, action)),
            );
        } else if let Some(err) = self.thread.last_error.clone() {
            // An idle turn that ended in error: surface it inline at the tail
            // with a Retry. This is the ONLY place a failure after the first
            // message becomes visible — the empty-state hint that also renders
            // `last_error` only paints when the transcript is empty.
            let retry = self.retry_button(cx);
            scroll = scroll.child(
                transcript_column(content_w)
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
                    transcript_column(content_w)
                        .child(summary_line(summary, theme, &typo)),
                );
            }
            if let Some(usage) = self.thread.usage.as_ref() {
                scroll = scroll.child(
                    transcript_column(content_w)
                        .child(usage_footer(usage, theme, &typo)),
                );
            }
        }
        // Trailing clearance INSIDE the scrollable content, above the composer:
        // a plain breathing margin below the last line. It used to carry a second,
        // much larger term estimating a reply's "under-counted tail", back when a
        // multi-paragraph reply painted past the height it reported and
        // `scroll_to_bottom` — which pins to gpui's `scroll_max`, derived from the
        // measured content height — could not reach the end of it. Transcript
        // children are now measured at the width they paint at (see
        // [`transcript_column`]), so the measured content height is the real one
        // and no extra reveal room is needed. A pending question keeps a roomier
        // margin so its Allow/Reject controls clear the composer.
        let tail_gap = if self.thread.pending_question().is_some() {
            px(160.0)
        } else {
            px(density.pad_panel * 4.0)
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
            .children(self.render_session_detail(cx))
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
            // The horizontal twin of `min_h(0)`, and just as load-bearing. A flex
            // item defaults to `min-width: auto`, i.e. "never shrink below my
            // content's min-content width" — and the transcript's children are
            // sized to a definite width taken from THIS box's measured width
            // ([`AgentChatView::content_width`]). Leave the default on and the two
            // feed each other: the children pin the box open at the reading
            // measure, the box reports that width back, and a pane narrower than
            // the measure never shrinks — it just clips the text. Zeroing the
            // min-width breaks the cycle, so the box always reports the room it
            // actually has.
            .min_w_0()
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
        // Evict this session from the remote registry so a closed tab doesn't leave
        // a stale entry the phone would still list (the registry holds its own
        // handle `Arc`, so this must be explicit — a `Drop` has no `cx`).
        self.unbind_remote();
        // Kill + reap the `claude` child so closing the tab doesn't leak it.
        if let Some(conn) = &self.connection {
            conn.shutdown();
        }
        // Reap any ACP embedded terminals (kill their PTYs + stop watchers) so a
        // closed tab doesn't leave orphaned processes. Release by the host's
        // terminal id (the value), not the tool id (the key).
        for (terminal_id, _view) in self.embedded_terminals.values() {
            acp_terminal_host::release_embedded(terminal_id);
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
        // Once per frame, before anything decodes: attachment images are cached
        // by the render path, which has no `Window` to release them with, so the
        // cache is allowed over budget until here. Overshooting by one frame of
        // newly-visible images is the cheap direction to be wrong in.
        self.image_cache.evict(window, cx);
        // A dormant restored chat connects on its first render — rendering is
        // the visibility signal (hidden tabs never render). Deferred off the
        // paint pass: the connect forks the agent process (plus a `codesign`
        // check for computer-use chats), which would blow the frame budget.
        if self.dormant {
            let view = cx.entity().downgrade();
            window.defer(cx, move |_window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |v, cx| v.ensure_connected(false, cx));
                }
            });
        }
        // Terminal view: show the companion terminal full-body (the headless chat
        // process keeps running underneath). Returns early so none of the chat's
        // transcript/composer setup runs while the terminal is up.
        if self.view_mode == ChatViewMode::Terminal
            && let Some(terminal) = self.terminal.clone()
        {
            return self.render_terminal_mode(terminal, cx).into_any_element();
        }
        let theme = self.theme;
        // Create/drop the interactive question cards to match the thread before
        // the (immutable) transcript render reads them. Needs `window` for the
        // cards' text inputs, so it lives here rather than in `render_transcript`.
        self.reconcile_question_cards(window, cx);
        // Same reconcile for ACP embedded terminals: mount a live inline
        // `TerminalView` for any tool call that bound one, reap ones that left.
        self.reconcile_embedded_terminals(window, cx);
        // Build (or tear down) the masked secret fields for an EnvVar-auth card —
        // here because `InputState::new` needs the `Window` the event fold lacks.
        self.reconcile_env_inputs(window, cx);
        // Same reconcile-on-demand pattern for the *New Agent* draft's worktree
        // slug field (needs `Window` too).
        self.reconcile_worktree_slug_input(window, cx);
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
                // The fullscreen sheet is the topmost overlay — Escape closes it
                // first. Then the image lightbox, then the find bar / staged edit.
                if this.open_tool_sheet.is_some() {
                    // If the backing tool call vanished (a rewind truncated the
                    // transcript), the sheet is already invisible — clear the
                    // stale pointer but DON'T consume Escape, so it still reaches
                    // whatever overlay is actually showing.
                    let showing = this.open_sheet_tool_call().is_some();
                    this.close_tool_sheet(window, cx);
                    if showing {
                        cx.stop_propagation();
                        return;
                    }
                }
                if this.preview.is_some() {
                    this.close_image_preview(cx);
                    cx.stop_propagation();
                } else if this.find_bar.is_some() {
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
            // the composer input holds focus (the common case). Capture it here
            // (ancestor-first, so it runs before the composer's own InputEscape)
            // and dismiss the topmost overlay; fall through otherwise so the
            // composer keeps owning its Escape.
            .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                // Same overlay priority as the DismissOverlay handler: sheet, then
                // lightbox, then find bar. A stale sheet id (backing tool call gone
                // after a rewind) is cleared without consuming Escape.
                if this.open_tool_sheet.is_some() {
                    let showing = this.open_sheet_tool_call().is_some();
                    this.close_tool_sheet(window, cx);
                    if showing {
                        cx.stop_propagation();
                        return;
                    }
                }
                if this.preview.is_some() {
                    this.close_image_preview(cx);
                    cx.stop_propagation();
                } else if this.find_bar_focused(window, cx) {
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
            // *New Agent* draft only: "Run in a fresh worktree" toggle + slug
            // field + create-state feedback, hidden once bound or non-git.
            .children(self.render_worktree_status_banner(cx))
            // An import bridge swaps the live composer for a Resume-in-terminal
            // footer (no in-app backend to send to); every other chat renders the
            // real composer.
            .child(if self.import_bridge.is_some() {
                self.render_import_bridge_footer(cx).into_any_element()
            } else {
                self.composer.clone().into_any_element()
            })
            // The image lightbox overlays everything when a thumbnail is opened.
            .children(self.render_image_preview(cx))
            // The fullscreen tool-payload sheet overlays everything when open.
            .children(self.render_tool_sheet(cx))
            .into_any_element()
    }
}

/// A live "<provider> is working…" row shown at the tail of the transcript while
/// a turn streams — a stepped rotating spinner (the reused rail cadence: 12
/// mechanical ticks/sec) plus muted text. Keeping it here rather than above the
/// composer means the input never resizes when a turn starts or ends.
fn working_indicator(label: &str, theme: Theme, typo: &Typography) -> AnyElement {
    spinner_row(&format!("{label} is working…"), theme, typo)
}

/// The compaction spinner — shown in place of the generic working indicator while
/// the backend reclaims context (Claude `system/status status="compacting"`), so
/// a long compaction reads as progress instead of a hang. Clears when the
/// boundary lands or the turn ends.
fn compacting_indicator(theme: Theme, typo: &Typography) -> AnyElement {
    spinner_row("Compacting context…", theme, typo)
}

/// A stepped rotating spinner + muted `text` — the shared body of the working /
/// compacting tail indicators.
fn spinner_row(text: &str, theme: Theme, typo: &Typography) -> AnyElement {
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
                .child(SharedString::from(text.to_string())),
        )
        .into_any_element()
}

/// A muted one-line turn summary (the backend's `post_turn_summary` detail),
/// shown under a settled turn like a subtle status caption.
/// A dedup key identifying a turn diff by its CONTENT, for the Review tab.
///
/// Only ever compared against other keys live in this process — turn-diff tabs
/// are not persisted into the saved layout — so a hasher with no cross-run
/// stability guarantee is fine here. It must never be written to disk or
/// compared across runs.
fn diff_tab_key(diff: &str) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    diff.hash(&mut h);
    format!("{:016x}", h.finish())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::connection::AgentCapabilities;
    use oximux_agents::thread::StubConnection;
    use serde_json::json;

    /// An optimistic feature pick overlays the backend-advertised value so the
    /// control reflects the user's choice immediately (a toggle flips, a select
    /// re-points) — and an override for an id the backend no longer advertises is
    /// harmlessly ignored.
    #[test]
    fn apply_feature_overrides_overlays_picks() {
        use oximux_agents::thread::{FeatureControl, FeatureKind, FeatureSelectOption};
        let mut features = vec![
            FeatureControl {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                icon: None,
                kind: FeatureKind::Toggle { on: false },
            },
            FeatureControl {
                id: "mode".into(),
                label: "Session Mode".into(),
                description: None,
                icon: None,
                kind: FeatureKind::Select {
                    options: vec![
                        FeatureSelectOption { wire: "a".into(), label: "A".into(), description: None },
                        FeatureSelectOption { wire: "b".into(), label: "B".into(), description: None },
                    ],
                    selected: Some("a".into()),
                },
            },
        ];
        let overrides = HashMap::from([
            ("fast".to_string(), FeatureValue::Bool(true)),
            ("mode".to_string(), FeatureValue::Choice("b".into())),
            ("stale".to_string(), FeatureValue::Bool(true)), // no matching feature → ignored
        ]);
        apply_feature_overrides(&mut features, &overrides);
        assert!(matches!(features[0].kind, FeatureKind::Toggle { on: true }));
        match &features[1].kind {
            FeatureKind::Select { selected, .. } => assert_eq!(selected.as_deref(), Some("b")),
            _ => panic!("expected select"),
        }
    }

    /// A completed catalog probe must never blank a good seed: an empty or failed
    /// revalidation of a disk-seeded picker keeps the seed; only a non-empty
    /// success is adopted and cached. (Regression: the `Ok(empty)` arm once
    /// clobbered a good seed, hiding the picker mid-draft.)
    #[test]
    fn fold_probe_result_preserves_a_good_seed() {
        use oximux_agents::thread::ModelChoice;
        let full = ProbedCatalog {
            models: vec![ModelChoice { wire: "m".into(), label: "m".into(), description: None }],
            default_model: None,
        };
        let empty = ProbedCatalog::default();

        // Non-empty success → adopt it AND hand it back for caching.
        let (state, cache) = fold_probe_result(false, Ok(full.clone()));
        assert!(matches!(state, Some(ProbeState::Ready(ref c)) if !c.models.is_empty()));
        assert_eq!(cache, Some(full));

        // Empty success WITH a good seed → keep the seed (no change, not cached).
        let (state, cache) = fold_probe_result(true, Ok(empty.clone()));
        assert!(state.is_none(), "empty revalidation must not clobber a good seed");
        assert!(cache.is_none());

        // Empty success WITHOUT a seed → adopt empty (agent has no models); not cached.
        let (state, cache) = fold_probe_result(false, Ok(empty));
        assert!(matches!(state, Some(ProbeState::Ready(ref c)) if c.models.is_empty()));
        assert!(cache.is_none(), "an empty catalog is never cached");

        // Error WITH a good seed → keep the seed.
        let (state, _) = fold_probe_result(true, Err(anyhow::anyhow!("boom")));
        assert!(state.is_none(), "a probe error must not clobber a good seed");

        // Error WITHOUT a seed → Failed.
        let (state, _) = fold_probe_result(false, Err(anyhow::anyhow!("boom")));
        assert!(matches!(state, Some(ProbeState::Failed)));
    }

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

    #[gpui::test]
    async fn disconnect_fails_closed_pending_permission(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
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
                    kind: oximux_agents::thread::PermissionKind::Tool,
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

    /// The gap this closes: a session's remote id used to be a per-process
    /// counter, so `agent-3` named a different conversation on every launch and a
    /// phone holding one after a restart pointed at whatever was built third.
    /// Once the agent mints its own id, the session moves onto it.
    #[gpui::test]
    async fn a_session_is_rekeyed_onto_the_agent_id_it_is_given(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = crate::remote_control::RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let placeholder = window
            .update(cx, |view, _window, _cx| view.remote_session_id().to_string())
            .unwrap();
        assert!(
            placeholder.starts_with("agent-"),
            "a chat that has never run has only a placeholder, got {placeholder}",
        );

        window
            .update(cx, |view, _window, cx| {
                // The agent mints its id, then anything at all arrives.
                view.thread.session_id = Some("11111111-2222-3333-4444-555555555555".into());
                view.on_event(ThreadEvent::AssistantText("hi".into()), cx);

                assert_eq!(
                    view.remote_session_id(),
                    "11111111-2222-3333-4444-555555555555",
                    "the session moved onto the id that names the conversation",
                );
            })
            .unwrap();

        cx.update(|cx| {
            let rc = cx.global::<crate::remote_control::RemoteControl>();
            assert!(
                rc.registry().get("11111111-2222-3333-4444-555555555555").is_some(),
                "and is reachable under it",
            );
            assert!(
                rc.registry().get(&placeholder).is_none(),
                "while the placeholder is gone, so the list shows one session not two",
            );
        });
    }

    /// A normal streamed turn folds into user + assistant entries via `on_event`.
    #[gpui::test]
    async fn on_event_builds_transcript(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                        is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(view.thread.entries.len(), 2, "user + assistant");
                assert!(!view.thread.turn_active, "turn ended");
            })
            .expect("window update");
    }

    /// A burst of streamed deltas folds into the transcript in full while
    /// costing one throttled repaint, not one per token.
    #[gpui::test]
    async fn delta_batch_concatenates_text_and_defers_one_repaint(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                // A fresh view has just painted, so the budget isn't up yet and
                // this batch must defer rather than paint.
                view.last_notify = std::time::Instant::now();
                let batch: Vec<ThreadEvent> = (0..10)
                    .map(|i| ThreadEvent::AssistantTextDelta(format!("tok{i} ")))
                    .collect();
                view.apply_batch(batch, cx);

                assert!(
                    view.flush_scheduled,
                    "an all-delta batch inside the interval queues a trailing repaint"
                );
                // Every delta is applied regardless — only the paint waits.
                let text = match view.thread.entries.last() {
                    Some(oximux_agents::thread::ThreadEntry::Assistant(m)) => m.text.clone(),
                    other => panic!("expected a streaming assistant entry, got {other:?}"),
                };
                assert_eq!(
                    text, "tok0 tok1 tok2 tok3 tok4 tok5 tok6 tok7 tok8 tok9 ",
                    "every delta lands, in order — throttling the paint must not drop or reorder text"
                );
            })
            .expect("window update");

        // The trailing repaint lands on its own, without another event to carry
        // it — otherwise a turn's final characters would sit invisible.
        cx.executor().advance_clock(NOTIFY_INTERVAL * 2);
        cx.run_until_parked();
        window
            .update(cx, |view, _window, _cx| {
                assert!(!view.flush_scheduled, "the trailing repaint fired and cleared the flag");
            })
            .expect("window update");
    }

    /// Anything the user can act on paints immediately — a tool card must not
    /// wait behind the streaming throttle.
    #[gpui::test]
    async fn non_delta_in_a_batch_repaints_immediately(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.last_notify = std::time::Instant::now();
                view.apply_batch(
                    vec![
                        ThreadEvent::AssistantTextDelta("thinking".into()),
                        ThreadEvent::ToolCallStarted {
                            id: "t1".into(),
                            name: "Bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                    cx,
                );
                assert!(
                    !view.flush_scheduled,
                    "a batch carrying a non-delta paints now, leaving nothing queued"
                );
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
                Arc::new(StubConnection::default()),
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
                Arc::new(StubConnection::default()),
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
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(count_users(view), 2, "queued message sent on turn end");
                assert!(view.thread.turn_active, "the flushed message started a fresh turn");

                // Queue now empty → a second turn end sends nothing more.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                assert_eq!(count_users(view), 2, "no phantom re-send when the queue is empty");
            })
            .expect("window update");
    }

    /// Steering hands a message to the turn that is already streaming: the stub
    /// records a `steer` (not a fresh send), the bubble appears at once, and the
    /// turn keeps running — unlike a normal send, which starts one.
    #[gpui::test]
    async fn steering_feeds_the_live_turn_instead_of_starting_a_new_one(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default().with_capabilities(AgentCapabilities {
            supports_steer: true,
            ..AgentCapabilities::default()
        });
        let recorder = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let count_users = |view: &AgentChatView| {
            view.thread.entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count()
        };

        window
            .update(cx, |view, _window, cx| {
                view.send_text("first".into(), Vec::new(), cx);
                assert!(view.thread.turn_active);

                view.steer_text("actually, stop".into(), cx);
                assert_eq!(count_users(view), 2, "the steered message is in the transcript now");
                assert!(view.thread.turn_active, "the turn it steers is still running");
                assert_eq!(view.thread.last_error, None);

                // Nothing to steer once the turn is over — that message would be
                // an ordinary send, and this path must not fake one.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None },
                    cx,
                );
                view.steer_text("too late".into(), cx);
                assert_eq!(count_users(view), 2, "no bubble for a message that went nowhere");
            })
            .expect("window update");

        let sent = recorder.sent();
        assert_eq!(sent.len(), 2, "the idle steer sent nothing");
        assert_eq!(sent[0]["message"]["content"], "first", "the send that started the turn");
        assert_eq!(sent[1]["type"], "steer", "steered rather than starting a turn");
        assert_eq!(sent[1]["message"], "actually, stop");
    }

    /// A backend with no mid-turn queue refuses the steer, and the refusal is
    /// surfaced rather than swallowed into a bubble the agent never received.
    #[gpui::test]
    async fn a_refused_steer_surfaces_and_pushes_no_bubble(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default(); // supports_steer: false
        let recorder = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
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
                view.send_text("first".into(), Vec::new(), cx);
                view.steer_text("second".into(), cx);
                assert_eq!(
                    view.thread.entries.iter().filter(|e| matches!(e, ThreadEntry::User { .. })).count(),
                    1,
                    "a message the backend rejected must not render as sent"
                );
                assert!(view.thread.last_error.as_deref().unwrap_or_default().contains("Steer failed"));
            })
            .expect("window update");

        assert_eq!(recorder.sent().len(), 1, "only the send that started the turn");
    }

    /// Picking Read-only must reach the SPAWN, because pi's gating is a
    /// spawn-time allowlist and a respawn is the only thing that applies it.
    ///
    /// This is the round's load-bearing safety property, and it shipped broken:
    /// `respawn` carried `codex_posture` but not `pi_posture`, so the pill read
    /// "Read-only" while pi ran wide open. Live, it wrote `breach.txt` on demand.
    /// The posture is only real if this spec carries it.
    #[gpui::test]
    async fn picking_a_pi_posture_reaches_the_respawn_spec(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, _cx| {
                // A Pi chat that has had its tools pill set to read-only.
                view.backend = ChatBackend::from(Transport::Rpc);
                view.feature_values.insert(
                    pi_posture::FEATURE_TOOLS.to_string(),
                    FeatureValue::Choice(pi_posture::TOOLS_READ_ONLY.to_string()),
                );

                let spec = view.respawn_spec(Vec::new(), None);
                let posture = spec
                    .pi_posture
                    .expect("the respawn must carry the posture, or the pill is decoration");
                assert_eq!(posture.tools, pi_posture::TOOLS_READ_ONLY);
                // And it reaches the child as real argv, not just a struct field.
                let args = oximux_agents::thread::pi::build_args(None, &posture, None)
                    .expect("build argv");
                assert!(
                    args.windows(2).any(|w| w[0] == "--tools"),
                    "read-only must arrive as pi's own allowlist flag: {args:?}"
                );

                // A non-Pi chat carries nothing here (the field is Rpc-only).
                view.backend = ChatBackend::stream_json();
                assert_eq!(view.respawn_spec(Vec::new(), None).pi_posture, None);
            })
            .expect("window update");
    }

    /// A backend that describes its own commands drives the palette's grouping,
    /// descriptions and attribution — no on-disk scan of another CLI's config.
    #[gpui::test]
    async fn a_backends_own_command_metadata_reaches_the_palette(cx: &mut TestAppContext) {
        use oximux_agents::thread::connection::SlashCommandInfo;

        cx.update(gpui_component::init);
        let stub = StubConnection::default()
            .with_capabilities(AgentCapabilities {
                supports_slash: true,
                ..AgentCapabilities::default()
            })
            .with_slash_commands(vec![SlashCommandInfo {
                name: "skill:verify-notes".into(),
                description: Some("Summarize the notes file.".into()),
                is_skill: true,
                source_label: Some("user".into()),
            }]);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
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
                view.composer.update(cx, |c, _cx| {
                    let cat = c.slash_catalog_for_test();
                    let meta = cat
                        .get("skill:verify-notes")
                        .expect("the backend's own command is in the palette's catalog");
                    assert_eq!(meta.group, super::slash_command_catalog::CommandGroup::Skill);
                    assert_eq!(meta.description.as_deref(), Some("Summarize the notes file."));
                    assert_eq!(meta.source_label.as_deref(), Some("user"));
                    // Nothing from another CLI's on-disk catalog leaked in.
                    assert!(!cat.contains_key("compact"));
                });
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
                Arc::new(StubConnection::default()),
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
                    // A backend advertising `git`, enriched with an argument hint
                    // from the on-disk catalog (no backend-advertised hint here).
                    c.set_slash_commands(
                        vec!["git".into(), "compact".into()],
                        std::collections::HashMap::new(),
                        std::collections::HashMap::new(),
                        cx,
                    );
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

                    // A backend-advertised hint (ACP `AvailableCommand.input`) wins
                    // over the on-disk catalog's argument-hint.
                    c.set_slash_commands(
                        vec!["git".into(), "compact".into()],
                        std::collections::HashMap::new(),
                        std::collections::HashMap::from([("git".to_string(), "<subcommand>".to_string())]),
                        cx,
                    );
                    c.recompute_overlays_for_test(cx);
                    assert_eq!(
                        c.usage_hint_for_test(cx),
                        Some(("git".to_string(), "<subcommand>".to_string())),
                        "ACP-advertised hint wins over the catalog argument-hint",
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
                Arc::new(stub),
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
                        kind: oximux_agents::thread::PermissionKind::Tool,
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
                Arc::new(stub),
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
                Arc::new(stub),
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
                    kind: oximux_agents::thread::PermissionKind::Tool,
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
                Arc::new(StubConnection::default()),
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
                    kind: oximux_agents::thread::PermissionKind::Tool,
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
                        is_error: true, turn_diff: None },
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
                Arc::new(StubConnection::default()),
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
                    ThreadEvent::TurnEnded { result: None, usage: None, is_error: true, turn_diff: None },
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
                Arc::new(StubConnection::default()),
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
                Arc::new(StubConnection::default()),
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

    /// With remote control enabled, a chat view registers its session into the
    /// shared registry, tees each applied event to a live subscriber in order, and
    /// evicts the session on disconnect. (The disabled path — no global → no
    /// binding → no clone — is what every other view test exercises implicitly.)
    #[gpui::test]
    async fn remote_enabled_registers_tees_and_unregisters(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let rc = RemoteControl::new();
            rc.set_enabled(true);
            cx.set_global(rc);
        });
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // The session registered under the view's stable remote id — subscribe.
        let mut rx = window
            .update(cx, |view, _window, cx| {
                cx.global::<RemoteControl>()
                    .registry()
                    .subscribe(&view.remote_session_id)
                    .expect("session registered while remote is enabled")
            })
            .expect("window update");

        // An applied event is teed to the remote subscriber with its assigned seq.
        window
            .update(cx, |view, _window, cx| {
                view.apply_batch(vec![ThreadEvent::AssistantText("hi".into())], cx);
            })
            .expect("window update");
        let (seq, ev) = rx.try_recv().expect("event teed to the remote subscriber");
        assert_eq!(seq, 1, "first teed event gets seq 1");
        assert_eq!(ev, ThreadEvent::AssistantText("hi".into()));

        // A respawn re-binds the SAME id. `seq` must keep climbing and the
        // subscriber must survive — a reset to 1 would look like duplicates to a
        // phone already past that cursor, and it would silently show nothing more.
        window
            .update(cx, |view, _window, cx| {
                view.connection = Some(Arc::new(StubConnection::default()));
                view.bind_remote(cx);
                view.apply_batch(vec![ThreadEvent::AssistantText("after".into())], cx);
            })
            .expect("window update");
        let (seq, ev) = rx.try_recv().expect("the subscription survived the respawn");
        assert_eq!(seq, 2, "seq continues across a respawn instead of resetting");
        assert_eq!(ev, ThreadEvent::AssistantText("after".into()));

        // Disconnect evicts the session from the registry.
        let id = window
            .update(cx, |view, _window, cx| {
                let id = view.remote_session_id.clone();
                view.on_disconnect(cx);
                id
            })
            .expect("window update");
        window
            .update(cx, |_view, _window, cx| {
                assert!(
                    cx.global::<RemoteControl>().registry().get(&id).is_none(),
                    "session unregistered on disconnect",
                );
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
                Arc::new(StubConnection::default().with_capabilities(
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
                    is_error: false, turn_diff: None });

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
                Arc::new(StubConnection::default().with_capabilities(
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
                    is_error: false, turn_diff: None });
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
                Arc::new(StubConnection::default()),
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
    fn a_review_tab_is_keyed_by_diff_content_not_by_position() {
        // The Review tab dedups on this key across the WHOLE pane group, and a
        // match just reactivates the existing tab without reloading it. So a key
        // that two different diffs can share means showing the wrong diff under
        // the right label — silently.
        let turn_a = "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+a\n";
        let turn_b = "diff --git a/b.rs b/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+b\n";

        // Two chats' first editing turn both sit at entry index 2; keying on the
        // index would collide here. Keying on content does not.
        assert_ne!(
            diff_tab_key(turn_a),
            diff_tab_key(turn_b),
            "two different turn diffs must never share a Review tab"
        );
        // A rewind repopulates the same index with a different diff — likewise
        // must not reactivate the pre-rewind tab.
        let after_rewind = "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+a2\n";
        assert_ne!(diff_tab_key(turn_a), diff_tab_key(after_rewind));
        // Reviewing the SAME diff twice reuses its tab — a collision here means
        // the content is identical, so the tab already shows the right thing.
        assert_eq!(diff_tab_key(turn_a), diff_tab_key(turn_a));
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
                Arc::new(stub),
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
                    is_error: true, turn_diff: None });

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
                Arc::new(StubConnection::default()),
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

                // Pick Codex: transport flips to app-server. Codex carries no
                // static pre-bind model (its real catalog arrives from the
                // `model/list` handshake), so the draft holds no model until bound
                // — and still no subprocess is spawned.
                view.change_agent("codex".into(), cx);
                assert_eq!(view.backend_transport_for_test(), Transport::AppServer);
                assert_eq!(view.model_for_test(), None);
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

    /// Picking a dynamic-model agent on a test view must NOT start a live catalog
    /// probe. The probe spawns the real agent binary on a raw `std::thread`, which
    /// reaches past the injected `StubConnection` and — being owned by no executor
    /// — outlives this test; its completion then lands mid-way through a LATER
    /// test and gpui aborts the whole run for scheduler non-determinism. An empty
    /// `probed_catalogs` is the observable proof no probe was started.
    #[gpui::test]
    async fn draft_agent_pick_does_not_start_a_live_catalog_probe(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                // Codex/ACP are exactly the dynamic-model agents a live probe targets.
                view.change_agent("codex".into(), cx);
                view.change_agent("opencode".into(), cx);
                assert!(
                    view.probed_catalogs.is_empty(),
                    "a stub-connection view must not spawn a live catalog probe"
                );
            })
            .expect("window update");
    }

    /// An import bridge captions its bubbles with the provider the transcript
    /// actually came from, not the inert stream-json placeholder it assembles on
    /// — otherwise an OpenCode transcript reads as Claude's.
    ///
    /// Uses OpenCode deliberately: Pi used to be the other bridge provider, but
    /// it now opens as a live chat, so a Pi fixture here would assert a route
    /// that no longer exists.
    #[gpui::test]
    async fn import_bridge_labels_bubbles_with_its_own_provider(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::new_import_bridge(
                PathBuf::from("/tmp/oximux-bridge-label"),
                vec![ThreadEntry::Assistant(AssistantMessage {
                    text: "hi from opencode".into(),
                    thinking: String::new(),
                })],
                ImportBridge {
                    preset_id: "opencode".into(),
                    session_id: "ses-1".into(),
                    resume_handle: "ses-1".into(),
                    cwd: PathBuf::from("/tmp/oximux-bridge-label"),
                    provider_display: "OpenCode".into(),
                },
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, _cx| {
                assert!(view.is_import_bridge());
                assert_eq!(
                    view.provider_label(),
                    "OpenCode",
                    "an imported transcript must not be captioned with the placeholder backend's name"
                );
            })
            .expect("window update");
    }

    /// The companion-terminal launch spec is offered only for a bound chat that
    /// has minted a session on a resumable transport; a draft, a session-less
    /// chat, and (implicitly) an unbound draft all decline. `set_view_mode` to
    /// Terminal is a no-op until the host attaches a companion.
    #[gpui::test]
    async fn terminal_launch_spec_gates_on_bound_session(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                // Bound (stub) Claude chat, but no session id yet → no terminal,
                // and the availability reason is "no session yet".
                assert!(view.terminal_launch_spec().is_none(), "no session → no terminal");
                assert_eq!(view.terminal_availability(), TerminalAvailability::NoSessionYet);
                assert_eq!(view.view_mode(), ChatViewMode::Chat);

                // A session id makes the resume terminal available.
                view.thread.session_id = Some("sid-1".into());
                let spec = view.terminal_launch_spec().expect("bound + session → resumable");
                assert_eq!(spec.adapter_id, "claude-code");
                assert_eq!(spec.session_id, "sid-1");
                assert_eq!(view.terminal_availability(), TerminalAvailability::Available);

                // A bound ACP chat WITH a session but NO resolved preset command
                // has no interactive resume CLI wired — the toggle stays disabled,
                // but the reason is distinct from "no session yet" (the GUI-found
                // misleading-hint bug).
                view.backend.transport = Transport::Acp;
                assert!(view.terminal_launch_spec().is_none(), "ACP w/o preset → no terminal");
                assert_eq!(
                    view.terminal_availability(),
                    TerminalAvailability::NoInteractiveResume,
                    "sent a message but ACP has no resume CLI — not 'send a message first'"
                );

                // opencode: a wired preset with a confirmed interactive-resume TUI
                // → the companion terminal is offered via the Custom adapter.
                view.backend.acp_command = Some("opencode".into());
                let spec = view.terminal_launch_spec().expect("opencode → resumable");
                assert_eq!(spec.adapter, AgentAdapter::Custom);
                assert_eq!(spec.adapter_id, "opencode");
                assert_eq!(spec.session_id, "sid-1");
                assert_eq!(view.terminal_availability(), TerminalAvailability::Available);

                // amp: not confirmed → no toggle (distinct binary + unverified id).
                view.backend.acp_command = Some("amp-acp".into());
                assert!(view.terminal_launch_spec().is_none(), "amp preset unwired → no terminal");
                assert_eq!(view.terminal_availability(), TerminalAvailability::NoInteractiveResume);

                // A wired preset but an UNSAFE agent-supplied session id (leading
                // dash could be parsed as a flag) is rejected — toggle disabled.
                view.backend.acp_command = Some("opencode".into());
                view.thread.session_id = Some("-boom".into());
                assert!(view.terminal_launch_spec().is_none(), "unsafe session id → no terminal");
                view.thread.session_id = Some("sid-1".into());

                view.backend.acp_command = None;
                view.backend.transport = Transport::StreamJson;

                // Switching to Terminal is a no-op until the host attaches one.
                view.set_view_mode(ChatViewMode::Terminal, window, cx);
                assert_eq!(view.view_mode(), ChatViewMode::Chat, "no companion → stays chat");

                // An unbound draft never offers a terminal, even with a session.
                view.make_unbound_for_test();
                view.thread.session_id = Some("sid-2".into());
                assert!(view.terminal_launch_spec().is_none(), "unbound draft → no terminal");
            })
            .expect("window update");
    }

    /// The ACP session id is an external, agent-supplied string; only ids safe to
    /// place on a resume command line are accepted (the rest leave the toggle off).
    #[test]
    fn resume_session_id_charset_is_validated() {
        // Real opencode ids (alnum + `_`) and dashed ids pass.
        assert!(is_safe_resume_session_id("ses_0aea7d2e3ffeBkIyWpDmBmZ93W"));
        assert!(is_safe_resume_session_id("sid-1"));
        assert!(is_safe_resume_session_id("abc123"));
        // Empty, leading-dash (flag injection), and shell metacharacters reject.
        assert!(!is_safe_resume_session_id(""));
        assert!(!is_safe_resume_session_id("-boom"));
        assert!(!is_safe_resume_session_id("a b"));
        assert!(!is_safe_resume_session_id("a;rm -rf"));
        assert!(!is_safe_resume_session_id("$(whoami)"));
        assert!(!is_safe_resume_session_id("a/b"));
    }

    /// The signed-out banner tracks only the LATEST assistant reply (or a turn
    /// error), and offers a terminal sign-in only on a transport with an
    /// interactive login CLI.
    #[gpui::test]
    async fn signed_out_detection_tracks_latest_turn(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                // A fresh Claude chat with no auth-failure reply isn't signed out,
                // and Claude has an interactive login CLI.
                assert!(!view.is_signed_out());
                assert_eq!(view.login_adapter_id(), Some("claude-code"));

                // A "Please run /login" reply settles as an ordinary assistant
                // turn (no error) yet must trip detection.
                view.thread.entries.push(ThreadEntry::Assistant(AssistantMessage {
                    text: "Not logged in · Please run /login".into(),
                    thinking: String::new(),
                }));
                assert!(view.is_signed_out(), "login-prompt reply → signed out");

                // A later successful reply clears it — only the latest turn counts.
                view.thread.entries.push(ThreadEntry::User {
                    text: "retry".into(),
                    images: Vec::new(),
                    checkpoint: None,
                });
                view.thread.entries.push(ThreadEntry::Assistant(AssistantMessage {
                    text: "Hello! How can I help?".into(),
                    thinking: String::new(),
                }));
                assert!(!view.is_signed_out(), "a later good reply clears the banner");

                // A login-flavored turn error also trips detection.
                view.thread.last_error = Some("API Error: authentication_error".into());
                assert!(view.is_signed_out(), "auth error text → signed out");

                // ACP presets carry no bundled login CLI → no terminal sign-in.
                view.make_unbound_for_test();
                view.change_agent("opencode".into(), cx);
                assert_eq!(view.login_adapter_id(), None);
            })
            .expect("window update");
    }

    /// The *New Agent* draft's worktree control only offers itself once unbound
    /// AND for a git project — never on a bound chat, never on a non-git one.
    ///
    /// Asserted on `worktree_draft_for_composer` because that is what now carries
    /// the choice (the composer renders the pill from it). The gate itself is
    /// unchanged from when a checkbox rendered it in this view; only its owner
    /// moved.
    #[gpui::test]
    async fn worktree_control_hidden_unless_unbound_and_git(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                // Bound (the constructor's default) + git: still hidden — the
                // control is a pre-bind-only affordance.
                view.set_git_project_for_test(true);
                assert!(
                    view.worktree_draft_for_composer(cx).is_none(),
                    "bound chats never show it"
                );

                // Unbound but non-git: hidden.
                view.make_unbound_for_test();
                view.set_git_project_for_test(false);
                assert!(
                    view.worktree_draft_for_composer(cx).is_none(),
                    "non-git projects never show it"
                );

                // Unbound + git: offered.
                view.set_git_project_for_test(true);
                assert!(
                    view.worktree_draft_for_composer(cx).is_some(),
                    "unbound + git offers the control"
                );

                // The status banner is a different thing: it carries only the
                // in-flight/failure state, so at rest it stays out of the way
                // rather than reserving an empty strip above the composer.
                assert!(
                    view.render_worktree_status_banner(cx).is_none(),
                    "no banner while the create state is Idle"
                );
            })
            .expect("window update");
    }

    /// The pill emits the DESIRED isolation, not a flip, so re-picking the row
    /// that is already active must be a no-op rather than silently toggling the
    /// choice to the opposite of what the user clicked.
    #[gpui::test]
    async fn worktree_isolation_pick_is_idempotent_and_reaches_the_draft(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                assert!(!view.worktree_draft_enabled_for_test());

                // Pick "New worktree" → armed, and the slug field materializes.
                view.set_worktree_isolation(true, window, cx);
                assert!(view.worktree_draft_enabled_for_test());
                let draft = view.worktree_draft_for_composer(cx).expect("offered");
                assert!(draft.enabled);
                assert!(draft.slug_input.is_some(), "arming creates the slug field");
                assert!(draft.hint.starts_with("oximux/"), "hint previews the branch: {}", draft.hint);

                // Re-picking the SAME row must not flip it back off.
                view.set_worktree_isolation(true, window, cx);
                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "re-picking the active row is a no-op, not a toggle"
                );

                // Picking the other row disarms.
                view.set_worktree_isolation(false, window, cx);
                assert!(!view.worktree_draft_enabled_for_test());
            })
            .expect("window update");
    }

    /// `/clear` on a never-bound *New Agent* draft must do nothing — above all it
    /// must not spawn.
    ///
    /// A draft is already a fresh conversation: `bind_now` drops `unbound` before
    /// the first message can land, so an empty transcript is an invariant here.
    /// `new_chat` used to respawn unconditionally, which spawned a subprocess
    /// while `unbound` stayed true — a live connection the view still treated as
    /// a draft, so the composer kept offering the pre-bind agent picker and
    /// static model list for a session already advertising real capabilities.
    ///
    /// Catching a stray spawn takes BOTH assertions below, because `respawn` can
    /// fail as well as succeed and the two leave different traces:
    /// - it succeeded → `connection` is `Some`
    /// - it failed → `disconnected` + `last_error` are set (`respawn_with_env`'s
    ///   `Err` arm)
    ///
    /// Only the second bites in a unit test, where `connect()` cannot succeed (no
    /// Tokio runtime). Asserting on `connection` alone looks right and proves
    /// nothing — it stays `None` whether or not the guard exists.
    #[gpui::test]
    async fn clear_on_an_unbound_draft_does_not_spawn_or_bind(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.sync_unbound_composer(cx);
                assert!(
                    !view.connection_is_live_for_test(),
                    "precondition: a draft has no subprocess"
                );

                view.send_text("/clear".into(), Vec::new(), cx);

                assert!(
                    !view.connection_is_live_for_test(),
                    "/clear on a draft must not spawn — the transport is choosable \
                     until the first real send"
                );
                // The one that actually bites here: a *failed* spawn attempt is
                // still an attempt, and it leaves these behind.
                assert!(
                    !view.disconnected,
                    "/clear must not even attempt a spawn — a draft is already fresh"
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "a draft that was never sent to cannot have failed to resume: {:?}",
                    view.thread.last_error
                );
                assert!(view.is_unbound(), "/clear must not bind the draft");
                assert!(view.thread.entries.is_empty(), "a draft has nothing to clear");
                // The draft's composer shape is untouched: the user can still pick
                // an agent after typing /clear.
                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "the draft keeps its pre-bind picker shape"
                );
            })
            .expect("window update");
    }

    // NOTE: `/clear` on a BOUND chat is deliberately not unit-tested. `new_chat`
    // reaches `respawn` → `connect()`, which starts a real subprocess — a unit
    // test must not do that (the same hazard that made the catalog probe SIGABRT
    // the suite). Covering it needs a spawn seam on `respawn`, which every other
    // respawn path shares (Stop-resume, model switch, auth, rewind), so that is a
    // design change on its own merits rather than a rider on this fix. Splitting
    // the transcript-reset into a helper and asserting on that instead would only
    // prove the helper works, not that `new_chat` still calls it.

    /// Binding must clear the worktree pill from the composer: a live session's
    /// cwd is fixed, so offering to change it is a lie.
    ///
    /// This is the mirror of `sync_while_unbound_keeps_the_draft_picker_shape`.
    /// The pill is pushed from `sync_unbound_composer`, which stops running once
    /// bound — so the *bound* sync has to clear it explicitly, exactly as it
    /// already clears the agent picker. Caught live (the dimmed pill lingered
    /// beside a bound chat's real controls), not by the unit tests above: none of
    /// them bind, which is precisely the gap this closes.
    #[gpui::test]
    async fn binding_clears_the_worktree_pill(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.set_worktree_isolation(true, window, cx);
                assert!(
                    view.composer.read(cx).worktree_draft_is_some_for_test(),
                    "precondition: an armed draft shows the pill"
                );

                // Bind, as a successful worktree create + send would.
                view.make_bound_for_test();
                view.sync_composer(cx);

                assert!(
                    !view.composer.read(cx).worktree_draft_is_some_for_test(),
                    "a bound chat's cwd is fixed — the pill must not linger"
                );
            })
            .expect("window update");
    }

    /// The pill must carry the parent's refusal to change the pick while a create
    /// is in flight / has failed with a message staged — otherwise it would
    /// render enabled and swallow clicks, which reads as a broken control.
    #[gpui::test]
    async fn worktree_draft_reports_busy_while_create_is_not_idle(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.set_worktree_isolation(true, window, cx);
                assert!(!view.worktree_draft_for_composer(cx).expect("offered").busy);

                view.send_text("hello".into(), Vec::new(), cx);

                let draft = view.worktree_draft_for_composer(cx).expect("offered");
                assert!(draft.busy, "an in-flight create freezes the pick");
                // And the underlying rule still holds: the pick cannot change.
                view.set_worktree_isolation(false, window, cx);
                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "the pick must not change while a message is staged"
                );
            })
            .expect("window update");
    }

    /// Toggling on creates the lazily-built slug `InputState` (and the toggle
    /// keeps rendering with it); toggling back off drops it and resets any
    /// stale create-state — mirroring `reconcile_env_inputs`'s create-on-demand
    /// pattern for the EnvVar-auth fields.
    #[gpui::test]
    async fn toggling_worktree_draft_creates_and_drops_the_slug_input(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                assert!(!view.worktree_draft_enabled_for_test());

                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test(), "first toggle enables it");
                assert!(view.worktree_slug_input.is_some(), "slug input created on enable");

                view.toggle_worktree_draft(window, cx);
                assert!(!view.worktree_draft_enabled_for_test(), "second toggle disables it");
                assert!(view.worktree_slug_input.is_none(), "slug input dropped on disable");
            })
            .expect("window update");
    }

    /// The first send on an armed draft is gated on the (async) worktree
    /// create landing first — `start_worktree_then_send` validates the slug,
    /// marks the create in-flight (`Creating`), stages the message, and emits
    /// `WorktreeWorkspaceRequested` for the host to run the DB-backed create.
    /// It does NOT bind/spawn or push the message to the transcript yet. With no
    /// host subscriber wired in this unit harness the request is inert, so the
    /// state parks at `Creating` — letting this assert the staging invariants
    /// (nothing bound, nothing in the transcript) without any git/process side
    /// effects.
    #[gpui::test]
    async fn send_on_armed_draft_stages_the_message_instead_of_binding(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                view.send_text("hello".into(), Vec::new(), cx);

                // The roster stages the send and hands the create to the host —
                // the state is in-flight (`Creating`), never bound, transcript
                // still empty.
                assert!(
                    matches!(
                        view.worktree_create_state_for_test(),
                        roster::WorktreeCreateState::Creating
                    ),
                    "armed send marks the worktree create in-flight"
                );
                assert!(!view.is_bound_for_test(), "must not bind before the worktree step lands");
                assert!(
                    view.thread.entries.is_empty(),
                    "the message stays staged, never pushed to the transcript"
                );

                // Retry re-enters the same path with the same staged text —
                // still in-flight, still nothing pushed.
                view.retry_worktree_create(cx);
                assert!(matches!(
                    view.worktree_create_state_for_test(),
                    roster::WorktreeCreateState::Creating
                ));
                assert!(view.thread.entries.is_empty());
            })
            .expect("window update");
    }

    /// Regression: syncing the composer while the draft is still UNBOUND must
    /// not push the bound-chat shape into it. The composer keeps its own
    /// `unbound` flag, and the agent picker, the Import-session row and the
    /// placeholder's agent name all read that one — so a `sync_composer` that
    /// unconditionally cleared it stripped all three from a live New Agent
    /// draft, with no way to restore them.
    ///
    /// The worktree toggle is the trigger that made this reachable (it syncs the
    /// composer to reflect its own busy state), but the bug is in the sync, not
    /// the toggle: every one of `sync_composer`'s ~23 callers could hit it while
    /// unbound. Asserted through the toggle because that is the path a user
    /// actually walks.
    #[gpui::test]
    async fn sync_while_unbound_keeps_the_draft_picker_shape(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                // Seed the draft's picker shape the way the real constructor does.
                view.sync_unbound_composer(cx);
                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "precondition: the draft seeds the composer as unbound"
                );
                let seeded_agents = view.composer.read(cx).agent_options_len_for_test();
                assert!(seeded_agents > 0, "precondition: the draft offers agents to pick");
                let seeded_models = view.composer.read(cx).vocab_models_len_for_test();
                assert!(seeded_models > 0, "precondition: the draft offers models to pick");

                // Flipping the worktree toggle syncs the composer. Before the fix
                // this reached `set_agent_picker(false, vec![], None)` and blanked
                // the draft.
                view.toggle_worktree_draft(window, cx);

                assert!(
                    view.composer.read(cx).unbound_for_test(),
                    "the toggle must not bind the composer — the Import row and the \
                     placeholder's agent name are gated on this flag"
                );
                assert_eq!(
                    view.composer.read(cx).agent_options_len_for_test(),
                    seeded_agents,
                    "the agent picker must survive an unbound sync"
                );
                // Asserted separately: the model picker reads `vocab.models`, not
                // `unbound`, so the two assertions above would both hold while the
                // model list was blanked on its own.
                assert_eq!(
                    view.composer.read(cx).vocab_models_len_for_test(),
                    seeded_models,
                    "the model picker must survive an unbound sync — a draft has no \
                     connection, so the caps-derived vocab is empty"
                );
            })
            .expect("window update");
    }

    /// HIGH regression: a SECOND, distinct Submit while a worktree create is
    /// already in flight (or one failed with a message still staged) must
    /// NEVER fall through `send_text`'s `bind_now` — that would bind at the
    /// ORIGINAL cwd (silently defeating the toggle), and once the in-flight
    /// create landed, the FIRST staged message would be re-sent into that
    /// now-wrongly-bound session: duplicated/out-of-order sends plus an
    /// orphaned worktree. `worktree_create_state` is set to `Creating`
    /// directly (mirroring what `start_worktree_then_send` does before the
    /// git op resolves) so this is exercised without a real async race.
    #[gpui::test]
    async fn second_submit_during_worktree_creating_does_not_bind_or_duplicate(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                // Simulate the async create landing mid-flight: the first
                // message is already staged and the create is running.
                view.worktree_create_state = roster::WorktreeCreateState::Creating;
                view.pending_worktree_send = Some(("first message".to_string(), Vec::new()));
                view.sync_composer(cx);

                // A second, distinct Submit arrives (e.g. a stray dispatch —
                // the composer itself is already disabled for this via
                // `sync_composer`'s fold into `disconnected`, so this is the
                // defense-in-depth path `send_text` itself must also close).
                view.send_text("second message".into(), Vec::new(), cx);

                assert!(
                    !view.is_bound_for_test(),
                    "must not bind at the original cwd while the worktree create is in flight"
                );
                assert!(
                    view.thread.entries.is_empty(),
                    "no message should reach the transcript until the worktree step lands"
                );
                assert_eq!(
                    view.pending_worktree_send.as_ref().map(|(t, _)| t.as_str()),
                    Some("first message"),
                    "the original staged message must survive untouched, not be clobbered"
                );
                assert!(
                    matches!(view.worktree_create_state_for_test(), roster::WorktreeCreateState::Creating),
                    "state is unchanged by the dropped second submit"
                );
            })
            .expect("window update");
    }

    /// MEDIUM regression: the toggle checkbox must refuse to flip while a
    /// worktree create has failed with a message still staged — otherwise
    /// unchecking it silently discards `pending_worktree_send`. The user must
    /// go through Retry / "continue without a worktree" instead, both of
    /// which route the staged message onward.
    #[gpui::test]
    async fn toggle_is_inert_while_worktree_create_failed_so_the_staged_message_survives(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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
                view.make_unbound_for_test();
                view.set_git_project_for_test(true);
                view.toggle_worktree_draft(window, cx);
                assert!(view.worktree_draft_enabled_for_test());

                // Simulate a landed failure with the message still staged.
                view.worktree_create_state =
                    roster::WorktreeCreateState::Failed("slug already exists".to_string());
                view.pending_worktree_send = Some(("keep me".to_string(), Vec::new()));
                view.sync_composer(cx);

                // A click on the checkbox while Failed must be a no-op.
                view.toggle_worktree_draft(window, cx);

                assert!(
                    view.worktree_draft_enabled_for_test(),
                    "the toggle must stay on — it must not flip while Failed"
                );
                assert!(
                    matches!(
                        view.worktree_create_state_for_test(),
                        roster::WorktreeCreateState::Failed(_)
                    ),
                    "the failed state is untouched by the ignored toggle"
                );
                assert_eq!(
                    view.pending_worktree_send.as_ref().map(|(t, _)| t.as_str()),
                    Some("keep me"),
                    "the staged message must survive the ignored toggle"
                );
                // The only sanctioned way out is Retry / `send_without_worktree`
                // (the failure banner's buttons) — not exercised here since
                // both ultimately reach `bind_now`'s real subprocess spawn,
                // which this pure state-transition test intentionally avoids
                // (mirrors every other `make_unbound_for_test` test in this
                // file never calling into a real connect()).
            })
            .expect("window update");
    }
}
