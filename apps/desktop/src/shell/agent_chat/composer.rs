//! The bottom composer row of the agent chat — a single-line input and a Send
//! button — isolated into its OWN entity/view.
//!
//! Why a separate entity: a text input only repaints its typed characters when
//! the view that OWNS it calls `cx.notify()` on each `Change` (gpui-component's
//! `InputState` does not self-repaint when embedded via `Input::new`). If that
//! `notify` lived on `AgentChatView`, every keystroke would rebuild the entire
//! transcript (every bubble + tool card) — visible typing lag. By owning the
//! input here, a keystroke dirties only THIS view; the transcript above stays
//! cached. Submit is surfaced to the parent as a [`ComposerEvent`].

use std::ops::Range;
use std::time::Instant;

use gpui::{
    Anchor, AnyElement, App, AppContext, ClipboardEntry, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, ImageSource, InteractiveElement, IntoElement,
    MouseButton,
    ObjectFit, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, img, prelude::FluentBuilder, px,
};
use gpui::StyledImage as _;
use gpui_component::Icon;
use gpui_component::WindowExt as _;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{
    IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp, Paste, Escape as InputEscape,
};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::Disableable as _;
use gpui_component::popover::Popover;
use gpui_component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_component::select::{Select, SelectEvent, SelectState};
use oximux_agents::thread::{
    prepend_context, ChatImage, ContextChip, EffortChoice, FeatureControl, FeatureKind,
    FeatureValue, ModeChoice, ModelChoice,
};
use oximux_dictation::{DictationEvent, ModelPaths, Readiness};
use oximux_settings::{DictationMode, DictationSettings};

use crate::actions::ToggleDictation;
use super::dictation_service::{self, DictationTarget, StartDecision};
use super::dictation_ui::{DictationUiState, WaveformBuffer, dictation_spacing, format_elapsed};
use super::dictation_waveform::{WaveformStyle, render_waveform};

/// A transcript waiting to be inserted at the cursor. Stashed by
/// [`ComposerView::on_dictation_event`] (which has no `Window`) and applied on
/// the next `render` (which does). `send_after` = the user pressed Enter to
/// stop, so submit once inserted.
struct PendingTranscript {
    text: String,
    send_after: bool,
}

/// The model/mode/effort options plus their "current when unset" defaults,
/// sourced from the live [`oximux_agents::thread::AgentConnection`] and pushed
/// into the composer so the bottom-toolbar pickers render whatever the backend
/// advertises — no hardcoded provider vocabulary lives in the view.
#[derive(Clone, Default, PartialEq)]
pub struct ControlVocab {
    pub models: Vec<ModelChoice>,
    pub permission_modes: Vec<ModeChoice>,
    pub efforts: Vec<EffortChoice>,
    /// Generic backend-advertised controls (fast/plan/auto-accept/agent-profile)
    /// rendered in the composer's feature cluster. Each carries its own current
    /// value (toggle on/off, select selected), so the view renders them straight
    /// from the vocab — the backend is the source of truth.
    pub features: Vec<FeatureControl>,
    pub default_model: Option<String>,
    pub default_mode: Option<String>,
    pub default_effort: Option<String>,
}
use oximux_settings::{Density, Theme, Typography};

use super::composer_history::PromptHistory;
use super::context_meter;
use super::context_providers::{ContextRequest, ContextSource};
use super::image_attach::{self, PendingImage, pending_from_bytes, pending_from_path};
use super::slash_command_catalog::{CommandCatalog, CommandGroup};
use super::slash_palette::{completed_command, detect_slash_trigger, rank_commands};
use crate::shell::agent_ui::agent_presentation::adapter_icon_path;
use crate::shell::compose_bar::mention_parser::pending_mention;
use crate::shell::compose_bar::mention_resolver::{MAX_SUGGESTIONS, rank as rank_mentions};

/// Open state of the slash-command palette: the in-progress query, the byte
/// range of the `/token` it replaces on accept, the ranked command indices
/// (into [`ComposerView::slash_commands`]), and which match is highlighted
/// (index into `matches`).
struct SlashPaletteState {
    range: Range<usize>,
    matches: Vec<usize>,
    highlight: usize,
}

/// Open state of the `@` mention overlay: the byte range of the `@query` it
/// replaces on accept, the ranked candidates, and which is highlighted (index
/// into `matches`). Mutually exclusive with [`SlashPaletteState`] — a leading `/`
/// opens the slash palette, which takes precedence.
struct MentionState {
    range: Range<usize>,
    /// Context providers first (a small curated set), then file paths. The
    /// highlight is a flat index across both.
    matches: Vec<MentionMatch>,
    highlight: usize,
}

/// One ranked row in the `@` overlay: either a context provider (index into
/// [`ComposerView::context_sources`]) or a project file path. Context matches sort
/// ahead of files so the three providers are always reachable at the top.
#[derive(Clone, PartialEq)]
enum MentionMatch {
    Context(usize),
    File(String),
}

/// The compact model label for the toolbar trigger: strip any `provider/`
/// namespace so `openai/gpt-5.5` reads as `gpt-5.5`. The full label still shows
/// in the dropdown menu.
fn short_model_label(label: &str) -> &str {
    match label.rfind('/') {
        Some(i) => &label[i + 1..],
        None => label,
    }
}

/// One row in the searchable model `Select`. `wire` is the value handed back on
/// pick; `label` is the full human name the dropdown shows and search matches
/// against; the trigger renders the namespace-stripped short form. `description`
/// is an optional capability blurb rendered muted beneath the name (and also
/// searched), so a query like "fastest" or "1M" surfaces the right model.
#[derive(Clone)]
struct ModelItem {
    wire: String,
    label: String,
    description: Option<String>,
}

impl SearchableListItem for ModelItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(
            div()
                .child(SharedString::from(short_model_label(&self.label).to_string()))
                .into_any_element(),
        )
    }

    /// A two-line row: the model name on top, its capability blurb
    /// muted beneath. Rows without a blurb render the name alone. The blurb is
    /// clipped with an ellipsis so a long description never widens the menu.
    fn render(&self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui_component::ActiveTheme as _;
        let name = short_model_label(&self.label).to_string();
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(SharedString::from(name))
            .when_some(self.description.clone(), |this, desc| {
                this.child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(desc)),
                )
            })
    }

    /// Match the model name *or* its description, so a capability search
    /// ("fastest", "reasoning") finds a model even when the query isn't in the name.
    fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.label.to_lowercase().contains(&q)
            || self
                .description
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains(&q))
    }

    fn value(&self) -> &Self::Value {
        &self.wire
    }
}

/// Rank the context providers against the `@query` (case-insensitive): prefix
/// matches first, then substring matches, preserving source order within each
/// tier. An empty query returns all sources, so a bare `@` reveals the providers.
/// Returns indices into the passed `sources`.
fn rank_context_sources(sources: &[ContextSource], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return (0..sources.len()).collect();
    }
    let mut prefix = Vec::new();
    let mut substr = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        if s.match_key.starts_with(&q) {
            prefix.push(i);
        } else if s.match_key.contains(&q) {
            substr.push(i);
        }
    }
    prefix.extend(substr);
    prefix
}

/// A message the user submitted while a turn was still streaming: parked here
/// and sent (in order) as each turn completes, so they can line up follow-ups
/// without waiting. Keeps the fully-staged [`PendingImage`]s (not just the wire
/// [`ChatImage`]) so pulling the message back to edit (↑) restores its
/// attachments without re-decoding; the wire form is extracted only on send.
struct QueuedMessage {
    text: String,
    images: Vec<PendingImage>,
    /// Context chips staged with this message. Held structured (not serialized)
    /// so pulling the message back to edit (↑) restores the chips, mirroring
    /// `images`; the `<context>` blocks are rendered onto the wire only at drain.
    context: Vec<ContextChip>,
}

/// Upper bound on parked messages, so a stuck turn can't let the queue grow
/// without bound. Generous — a user rarely lines up more than a few.
const MAX_QUEUED: usize = 20;

/// Raised by the composer for the parent [`super::AgentChatView`] to act on.
/// The parent performs the actual send / interrupt / model+mode switch; on
/// `Submit` the composer has already cleared its input by the time the event
/// fires.
pub enum ComposerEvent {
    Submit {
        text: String,
        images: Vec<ChatImage>,
    },
    /// The user pressed a queued chip's ↑ **during** a live turn, on a backend
    /// that can take it (`supports_steer`). The parent hands it to the running
    /// turn rather than parking it until the turn ends. Text-only: the chip
    /// reorders instead when it carries attachments.
    SteerNow {
        text: String,
    },
    /// The user pressed Stop while a turn was streaming — interrupt it.
    Stop,
    /// The user picked a model in the bottom toolbar (a Claude alias).
    ModelPicked(String),
    /// The user picked a permission mode in the bottom toolbar (a wire value).
    PermissionModePicked(String),
    /// The user picked a reasoning-effort level in the bottom toolbar.
    EffortPicked(String),
    /// The user picked a thinking-visibility level in the bottom toolbar
    /// (`off` / `auto` / `shown`) — a transcript view preference the parent
    /// view owns and persists; nothing is sent to the backend.
    ThinkingDisplayPicked(String),
    /// The user changed a generic feature control in the bottom toolbar (a
    /// toggle flip or a select pick). Carries the control's stable `id` and its
    /// new value. The parent applies it live (ACP `set_config`) or via respawn.
    FeaturePicked { id: String, value: FeatureValue },
    /// The user picked a coding agent in the unbound *New Agent* draft's agent
    /// dropdown (an adapter id, e.g. `codex`/`opencode`). The parent rebuilds the
    /// backend + default model and re-pushes the picker. Only fired while unbound.
    AgentPicked(String),
    /// The user picked an isolation mode in the unbound *New Agent* draft's
    /// worktree pill: `true` = run the first send in a fresh git worktree,
    /// `false` = run in the project itself. The parent owns the flip (it also
    /// creates/drops the slug input), so this carries the DESIRED state rather
    /// than a toggle — picking the already-active row is a no-op there.
    WorktreeIsolationPicked(bool),
    /// The `@` overlay just opened. The parent refreshes the composer's context
    /// sources (esp. the live sibling-terminal list) in response, so the "Context"
    /// section is current without the composer reaching into the pane group.
    MentionOpened,
    /// The user picked a context provider in the `@` overlay. The parent captures
    /// the content (clipboard / git diff / terminal scrollback) and hands the
    /// resulting chip back via [`ComposerView::add_context_chip`].
    CaptureContext(ContextRequest),
}

/// The *New Agent* draft's worktree-isolation state, projected by the parent for
/// the composer's pill. The parent owns every field; this is a render-time
/// snapshot, not a second source of truth — `slug_input` is a shared handle to
/// the parent's own `InputState`, so the text itself lives in exactly one place.
#[derive(Clone)]
pub struct WorktreeDraft {
    /// Whether the first send runs in a fresh worktree.
    pub enabled: bool,
    /// The parent's slug field, present only while `enabled`. Rendered inside the
    /// popover; edits flow straight back to the parent's `InputState`.
    pub slug_input: Option<Entity<InputState>>,
    /// A create is in flight (or failed with a message still staged) — the pick
    /// is frozen until the banner resolves it. Mirrors the parent's refusal to
    /// flip in that state rather than duplicating the rule.
    pub busy: bool,
    /// The live `oximux/<slug>` preview, or the validation error when the slug is
    /// malformed. Computed by the parent (it owns `validate_slug`).
    pub hint: String,
    /// Whether `hint` is an error (drives its color).
    pub hint_is_error: bool,
}

impl PartialEq for WorktreeDraft {
    /// Entity handles compare by identity, which is what the change-guard in
    /// [`ComposerView::set_worktree_draft`] wants: a re-push carrying the same
    /// input handle and the same rendered strings is a no-op, but the slug's
    /// *text* changing is picked up via `hint` (recomputed by the parent on every
    /// keystroke) rather than by reaching into the shared `InputState`.
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.busy == other.busy
            && self.hint == other.hint
            && self.hint_is_error == other.hint_is_error
            && match (&self.slug_input, &other.slug_input) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            }
    }
}

/// The composer's `auto_grow` input grows one row per WRAPPED line of the draft
/// up to this many rows, then holds that height and scrolls internally — so a
/// long message never pushes the transcript off-screen. Tuned to Claude
/// Desktop's feel: generous but bounded.
const MAX_COMPOSER_ROWS: usize = 10;

/// What a footer dropdown does when a row is picked, given the row's wire value.
///
/// Named because the permission / effort / feature-select controls all share the
/// shape: `render_labeled_dropdown` takes one and each caller builds one, so the
/// two spellings have to stay in step.
type DropdownPick = std::rc::Rc<dyn Fn(&mut ComposerView, String, &mut Context<ComposerView>)>;

pub struct ComposerView {
    input: Entity<InputState>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// The bound agent's display name, driving the input placeholder ("Message
    /// {label}…"). Pushed by the parent on bind via [`Self::set_provider_label`];
    /// while unbound the placeholder follows the picked agent instead.
    provider_label: String,
    /// The placeholder string currently applied to the input, so `render` only
    /// re-pushes it (which needs a `Window`) when the derived value changes.
    applied_placeholder: Option<String>,
    /// Persistent searchable model dropdown (filter-as-you-type),
    /// held for the composer's life and re-seeded only when the advertised model
    /// set changes. A Confirm pick routes to `pick_model`. Rendered in place of a
    /// flat menu so long ACP model lists (OpenCode) stay navigable.
    model_select: Entity<SelectState<SearchableVec<ModelItem>>>,
    /// The `(wire, label, description)` set last pushed into `model_select`, so
    /// `render` only rebuilds its items when the advertised models actually change
    /// (a new blurb re-seeds too, so descriptions land as soon as they arrive).
    model_select_sig: Vec<(String, String, Option<String>)>,
    /// The model wire last applied as `model_select`'s selection, so `render`
    /// re-syncs it only when it drifts (not every paint).
    model_select_current: Option<String>,
    /// Routes the model dropdown's Confirm event to `pick_model`.
    _model_select_sub: Subscription,
    /// Mirrors the parent's connection state, for the status line + Send button.
    disconnected: bool,
    turn_active: bool,
    /// Whether the backend can take a message *during* a live turn
    /// (`AgentCapabilities::supports_steer`). Changes only what a queued chip's ↑
    /// does mid-turn: with it, "send now" means now; without it, the message can
    /// only be moved to the front of the queue.
    can_steer: bool,
    /// Mirrors of the parent's session controls, for the bottom toolbar pickers.
    /// The parent owns the truth (it respawns on a change) and pushes updates via
    /// [`Self::set_controls`]; the composer only renders them and emits a pick.
    model: Option<String>,
    permission_mode: Option<String>,
    effort: Option<String>,
    /// Whether the backend honors a permission-mode switch (hides the mode picker
    /// when it doesn't). Model is always offered.
    supports_modes: bool,
    /// Thinking-visibility chip state (wire `off`/`auto`/`shown`), pushed by
    /// the parent view. `None` hides the chip — the transcript holds no
    /// thinking block yet, so there is nothing the control could change.
    thinking_display: Option<String>,
    /// Whether the backend accepts a reasoning-effort setting (hides the effort
    /// picker when it doesn't).
    supports_effort: bool,
    /// The model/mode/effort options the live backend advertises, pushed in via
    /// [`Self::set_controls`]. The pickers render from this (no hardcoded
    /// provider vocab); empty until a connection is attached.
    vocab: ControlVocab,
    /// Id of the footer dropdown whose menu is currently open (e.g. `"chat-effort"`),
    /// or `None` when all are closed. Driven by each dropdown's `on_open_change`.
    /// Used to suppress a control's hover tooltip while its menu is open, so the
    /// tooltip never paints over the options.
    open_dropdown: Option<SharedString>,
    /// True while this is an unbound *New Agent* draft — the agent picker is
    /// shown so the user can choose the coding agent before the first send binds
    /// it. Cleared once bound (a live session's transport can't be hot-swapped).
    /// Pushed by the parent via [`Self::set_agent_picker`].
    unbound: bool,
    /// The pickable coding agents for the unbound draft's dropdown, `(id,
    /// display)` in roster order. Empty when bound.
    agent_options: Vec<(String, String)>,
    /// The currently-picked agent `(id, display)`, for the agent-picker button
    /// label + the menu checkmark. `None` when bound / not yet seeded.
    current_agent: Option<(String, String)>,
    /// The *New Agent* draft's worktree-isolation state, or `None` when the pill
    /// doesn't apply (bound chat, or a non-git project). Pushed by the parent via
    /// [`Self::set_worktree_draft`]; the parent owns the truth, this only renders
    /// the pill and emits [`ComposerEvent::WorktreeIsolationPicked`].
    worktree_draft: Option<WorktreeDraft>,
    /// Images staged for the next send (via the paperclip, ⌘V, or drag-drop).
    /// Each holds both its wire/persist [`ChatImage`] and a pre-decoded thumbnail
    /// so the chip row doesn't re-decode on every keystroke repaint. Cleared on
    /// submit.
    pending_images: Vec<PendingImage>,
    /// Voice-dictation UI state for this composer (mic button + recording bar).
    /// Driven by [`dictation_service`] events via [`Self::on_dictation_event`].
    dictation: DictationUiState,
    /// Recent RMS levels driving the recording bar's scrolling waveform. Reset
    /// on start; pushed on each `Level` event.
    dictation_waveform: WaveformBuffer,
    /// Set when a Hold-mode press is released before an async mic-permission
    /// grant resolves, so the pending start aborts instead of going hot after
    /// the gesture ended (first-ever-use race). Cleared on each hold press.
    dictation_hold_released: bool,
    /// True when the current recording was stopped via Enter (submit once the
    /// transcript lands), vs the mic/Cmd+E toggle (insert only). Set at stop,
    /// consumed on the final transcript.
    dictation_send_after: bool,
    /// A finished transcript waiting to be inserted at the cursor on the next
    /// render (the event that produced it had no `Window`). See
    /// [`PendingTranscript`].
    pending_transcript: Option<PendingTranscript>,
    /// A transient message to surface as a toast on the next render — used by the
    /// event path (Capped / Error), which has no `Window` to push one directly.
    pending_toast: Option<String>,
    /// Command names the backend advertised at session init (from `SessionInit`),
    /// offered in the slash-command palette. Empty when the backend advertises
    /// none — which also disables the palette.
    slash_commands: Vec<String>,
    /// On-disk enrichment (description, group, source) for the advertised names,
    /// discovered off the main thread and pushed in when ready. Empty until then;
    /// names without an entry render bare under Built-in.
    slash_catalog: CommandCatalog,
    /// Descriptions the backend itself advertised for its commands (ACP
    /// `available_commands_update`), keyed by name. Preferred over the on-disk
    /// `slash_catalog` in the palette (the agent knows its own commands best);
    /// empty for backends that advertise names only (Claude/Codex).
    slash_descriptions: std::collections::HashMap<String, String>,
    /// Argument hints the backend advertised for its commands (ACP
    /// `AvailableCommand.input`), keyed by name — shown as trailing muted text in
    /// the palette row. Empty for backends that advertise no argument spec.
    slash_hints: std::collections::HashMap<String, String>,
    /// The open slash-command palette, or `None` when the caret isn't inside a
    /// `/token` (or the palette is otherwise suppressed).
    palette: Option<SlashPaletteState>,
    /// Project file paths for `@file` autocomplete, scanned once off the main
    /// thread and pushed in by the parent. Empty until the scan lands — the
    /// overlay shows a "scanning" hint while a mention is in progress.
    mention_candidates: Vec<String>,
    /// Whether the file scan has completed (distinguishes "scanning…" from "no
    /// matching files" in the overlay).
    mention_candidates_loaded: bool,
    /// The open `@` mention overlay, or `None` when the caret isn't inside an
    /// `@query`. Mutually exclusive with `palette`.
    mention: Option<MentionState>,
    /// Context providers offered in the `@` overlay's "Context" section (`@diff`,
    /// `@clipboard`, one `@terminal` per sibling tab). Pushed by the parent —
    /// refreshed each time the overlay opens so the terminal list stays live.
    context_sources: Vec<ContextSource>,
    /// Context chips staged for the next send (captured terminal output / diff /
    /// clipboard), shown as removable chips above the input. Serialized into the
    /// outgoing message as `<context>` blocks and cleared on submit — mirrors
    /// `pending_images`.
    context_chips: Vec<ContextChip>,
    /// Shell-style recall of previously-sent prompts (↑/↓). Seeded from the
    /// restored transcript and appended on every send; pure state lives in
    /// [`PromptHistory`].
    history: PromptHistory,
    /// Messages the user lined up while a turn was streaming, oldest first. The
    /// parent drains one per completed turn via [`Self::take_next_queued`].
    queued: Vec<QueuedMessage>,
    /// Context tokens used this turn (input + cache + output), pushed by the
    /// parent from live usage. `None` until any usage has arrived this session —
    /// the meter stays hidden so a brand-new chat doesn't flash an empty meter.
    meter_used_tokens: Option<u64>,
    /// The model's context-window size (the meter denominator), cached across
    /// turns. `None` when not yet known (Claude's first live turn) — the meter
    /// then shows a raw token count, no percentage.
    meter_window: Option<u64>,
    /// Cumulative USD spend this app-session ("cost since open"). Shown in the
    /// meter tooltip only when > 0 (Codex reports no cost, so it stays hidden).
    meter_cost: f64,
    /// Repaints this view (only) on each keystroke so the draft stays visible.
    _sub: Subscription,
}

impl EventEmitter<ComposerEvent> for ComposerView {}

impl ComposerView {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        provider_label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = format!("Message {provider_label}…  (↵ send · ⇧↵ newline)");
        let initial_placeholder = placeholder.clone();
        let input = cx.new(|cx| {
            // MULTI-LINE field that GROWS with the draft up to MAX_COMPOSER_ROWS,
            // then holds height and scrolls (Claude-Desktop feel). ↵ submits; ⇧↵
            // inserts a newline. `render` sets an explicit `.h()` from the row
            // count because the field's text element lays out
            // `position: absolute; height: 100%` and can't size the pill from its
            // content (the circular-height trap). The ↵-vs-⇧↵ split is decided at
            // the parent root `capture_action` from the live shift modifier —
            // both keys map to the same Enter action.
            InputState::new(window, cx).auto_grow(1, MAX_COMPOSER_ROWS).placeholder(placeholder)
        });
        let sub = cx.subscribe(&input, |this, _input, ev: &InputEvent, cx| {
            // Repaint ONLY the composer on edits — the transcript is untouched.
            // Focus/Blur repaint too so the pill's border can track focus (a
            // brighter ring while typing), like a native chat field.
            match ev {
                InputEvent::Change => {
                    // A genuine edit while recalling history detaches back to the
                    // live draft (so ↑ restarts from the newest entry). Ignore the
                    // echo of our own programmatic reload: the intermediate empty
                    // value from clear+insert and the final value that still equals
                    // the shown entry are both us, not the user.
                    if this.history.is_navigating() {
                        let v = this.input.read(cx).value().to_string();
                        if !v.is_empty() && this.history.current() != Some(v.as_str()) {
                            this.history.detach();
                        }
                    }
                    this.recompute_overlays(cx);
                    cx.notify();
                }
                // Losing focus closes both overlays so they can't linger over the
                // transcript while the field is inactive.
                InputEvent::Blur => {
                    this.palette = None;
                    this.mention = None;
                    cx.notify();
                }
                InputEvent::Focus => cx.notify(),
                _ => {}
            }
        });
        // The searchable model dropdown is created once (it needs a `Window`) with
        // an empty list and re-seeded lazily in `render` when models arrive. A
        // Confirm pick routes to `pick_model` exactly like the old menu did.
        let model_select = cx.new(|cx| {
            // Filter-as-you-type is on; the search box only shows inside the open
            // dropdown, so short lists (Claude/Codex) stay uncluttered while long
            // ACP lists (OpenCode) become searchable.
            SelectState::new(SearchableVec::new(Vec::<ModelItem>::new()), None, window, cx)
                .searchable(true)
        });
        let model_select_sub = cx.subscribe(
            &model_select,
            |this, _state, ev: &SelectEvent<SearchableVec<ModelItem>>, cx| {
                // Programmatic `set_selected_value` (the render-time sync) does not
                // emit Confirm, so this only fires on a real user pick — no loop.
                if let SelectEvent::Confirm(Some(wire)) = ev {
                    this.pick_model(wire.clone(), cx);
                }
            },
        );
        Self {
            input,
            theme,
            density,
            typography,
            provider_label: provider_label.to_string(),
            applied_placeholder: Some(initial_placeholder),
            model_select,
            model_select_sig: Vec::new(),
            model_select_current: None,
            _model_select_sub: model_select_sub,
            open_dropdown: None,
            disconnected: false,
            turn_active: false,
            model: None,
            permission_mode: None,
            effort: None,
            thinking_display: None,
            supports_modes: false,
            supports_effort: false,
            vocab: ControlVocab::default(),
            unbound: false,
            agent_options: Vec::new(),
            current_agent: None,
            worktree_draft: None,
            pending_images: Vec::new(),
            dictation: DictationUiState::Idle,
            dictation_waveform: WaveformBuffer::default(),
            dictation_hold_released: false,
            dictation_send_after: false,
            pending_transcript: None,
            pending_toast: None,
            slash_commands: Vec::new(),
            slash_catalog: CommandCatalog::new(),
            slash_descriptions: std::collections::HashMap::new(),
            slash_hints: std::collections::HashMap::new(),
            palette: None,
            mention_candidates: Vec::new(),
            mention_candidates_loaded: false,
            mention: None,
            context_sources: Vec::new(),
            context_chips: Vec::new(),
            history: PromptHistory::new(),
            queued: Vec::new(),
            can_steer: false,
            meter_used_tokens: None,
            meter_window: None,
            meter_cost: 0.0,
            _sub: sub,
        }
    }

    /// Stage already-decoded attachments (from the file picker / drag-drop task
    /// or a clipboard paste) and repaint the chip row.
    pub fn add_pending_images(&mut self, images: Vec<PendingImage>, cx: &mut Context<Self>) {
        if images.is_empty() {
            return;
        }
        self.pending_images.extend(images);
        cx.notify();
    }

    /// Append `@path ` mentions to the end of the draft — the same inline form
    /// [`Self::mention_accept`] produces when a file is picked from the `@`
    /// overlay, so a dropped file and a typed mention reach the agent
    /// identically. A separating space is inserted when the draft doesn't
    /// already end in whitespace, so a drop can't fuse onto the last word.
    ///
    /// Deliberately NOT a [`ContextChip`]: a chip inlines the file's *content*
    /// into the message, and dropping a 5000-line source file should hand the
    /// agent a reference it can choose to read, not silently spend the context
    /// window on it.
    pub fn append_mentions(
        &mut self,
        paths: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let next = with_mentions_appended(&self.input.read(cx).value(), paths);
        self.set_draft_end(next, window, cx);
        cx.notify();
    }

    /// Attach image files chosen from the native file dialog. `rfd`'s async
    /// dialog runs off the main thread; the read + decode also happens on a
    /// background executor (decoding a large image is not cheap), then the staged
    /// results are handed back to this view on the foreground.
    fn attach_from_picker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let files = rfd::AsyncFileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"])
                .pick_files()
                .await;
            let Some(files) = files else { return };
            let paths: Vec<_> = files.into_iter().map(|f| f.path().to_path_buf()).collect();
            let staged = cx
                .background_spawn(async move {
                    paths.iter().filter_map(|p| pending_from_path(p)).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| this.add_pending_images(staged, cx));
        })
        .detach();
    }

    /// Handle ⌘V: if the clipboard holds an image, stage it and report `true`
    /// (so the caller consumes the paste); otherwise `false` to let the text
    /// field paste normally. Decoding a pasted screenshot is done inline — it's a
    /// one-shot user action, not a hot path.
    fn try_paste_image(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut staged = Vec::new();
        for entry in item.into_entries() {
            if let ClipboardEntry::Image(image) = entry
                && let Some(p) = pending_from_bytes(image.bytes, Some(image.format))
            {
                staged.push(p);
            }
        }
        if staged.is_empty() {
            return false;
        }
        self.add_pending_images(staged, cx);
        true
    }

    /// Remove a staged attachment (its chip's ✕).
    fn remove_image(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.pending_images.len() {
            self.pending_images.remove(idx);
            cx.notify();
        }
    }

    /// Replace the context providers offered in the `@` overlay (pushed by the
    /// parent when the overlay opens). Refreshes an in-progress mention so the
    /// "Context" section reflects the new list. Cheap — labels only, no captured
    /// content.
    pub fn set_context_sources(&mut self, sources: Vec<ContextSource>, cx: &mut Context<Self>) {
        self.context_sources = sources;
        if self.mention.is_some() {
            self.recompute_mention(cx);
            cx.notify();
        }
    }

    /// Stage a captured context chip (handed back by the parent after it captured
    /// clipboard / diff / terminal content) and repaint the chip row.
    pub fn add_context_chip(&mut self, chip: ContextChip, cx: &mut Context<Self>) {
        self.context_chips.push(chip);
        cx.notify();
    }

    /// Remove a staged context chip (its chip's ✕).
    fn remove_context_chip(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.context_chips.len() {
            self.context_chips.remove(idx);
            cx.notify();
        }
    }

    /// The inner input's focus handle — the parent focuses this on activate so
    /// keystrokes land in the draft without a click first.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// Sync the parent's connection/turn state (drives the status line + whether
    /// Send is enabled). Only repaints when something actually changed.
    pub fn set_state(&mut self, disconnected: bool, turn_active: bool, cx: &mut Context<Self>) {
        if self.disconnected != disconnected || self.turn_active != turn_active {
            self.disconnected = disconnected;
            self.turn_active = turn_active;
            cx.notify();
        }
    }

    /// Whether the backend accepts a mid-turn message ([`Self::can_steer`]).
    pub fn set_can_steer(&mut self, can_steer: bool, cx: &mut Context<Self>) {
        if self.can_steer != can_steer {
            self.can_steer = can_steer;
            cx.notify();
        }
    }

    /// Mirror the parent's session controls (current model, permission mode,
    /// effort, and which the backend supports) so the bottom toolbar renders the
    /// right labels + pickers. Only repaints when something actually changed.
    #[allow(clippy::too_many_arguments)]
    pub fn set_controls(
        &mut self,
        model: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
        supports_modes: bool,
        supports_effort: bool,
        vocab: ControlVocab,
        cx: &mut Context<Self>,
    ) {
        if self.model != model
            || self.permission_mode != permission_mode
            || self.effort != effort
            || self.supports_modes != supports_modes
            || self.supports_effort != supports_effort
            || self.vocab != vocab
        {
            self.model = model;
            self.permission_mode = permission_mode;
            self.effort = effort;
            self.supports_modes = supports_modes;
            self.supports_effort = supports_effort;
            self.vocab = vocab;
            cx.notify();
        }
    }

    /// Push the live context-meter inputs from the parent: tokens used this turn
    /// (`None` until any usage has arrived — keeps the meter hidden on a fresh
    /// chat), the cached window size (`None` = no denominator yet → count-only
    /// state), and cumulative session cost. Repaints only on a real change.
    pub fn set_usage_meter(
        &mut self,
        used_tokens: Option<u64>,
        context_window: Option<u64>,
        session_cost_usd: f64,
        cx: &mut Context<Self>,
    ) {
        if self.meter_used_tokens != used_tokens
            || self.meter_window != context_window
            || self.meter_cost != session_cost_usd
        {
            self.meter_used_tokens = used_tokens;
            self.meter_window = context_window;
            self.meter_cost = session_cost_usd;
            cx.notify();
        }
    }

    /// Build the context meter for the controls row, or `None` when no usage has
    /// arrived yet this session (hidden state). Computes the percent + tooltip
    /// from the pushed-down live usage; falls back to a count-only state when the
    /// window size isn't known.
    fn render_context_meter(&self) -> Option<impl IntoElement> {
        let used = self.meter_used_tokens?;
        let fraction = self
            .meter_window
            .filter(|w| *w > 0)
            .map(|w| used as f32 / w as f32);
        let label = match fraction {
            Some(f) => format!("{}%", (f * 100.0).round() as u32),
            None => context_meter::compact_tokens(used),
        };
        // Tooltip: exact %, used/max tokens, and "cost since open" (only when a
        // cost has actually accrued — Codex/most ACP report none, and a $0.00
        // line would mislead).
        let mut tip = match self.meter_window.filter(|w| *w > 0) {
            Some(w) => format!(
                "Context: {} of {} used ({}%)",
                context_meter::compact_tokens(used),
                context_meter::compact_tokens(w),
                fraction.map(|f| (f * 100.0).round() as u32).unwrap_or(0),
            ),
            None => format!("Context: {} tokens (window size unknown)", context_meter::compact_tokens(used)),
        };
        if self.meter_cost > 0.0 {
            tip.push_str(&format!("\nCost since open: ${:.2}", self.meter_cost));
        }
        Some(context_meter::context_meter(fraction, label, tip, &self.theme, &self.typography, &self.density))
    }

    /// Push the bound agent's display name so the input placeholder reads "Message
    /// {label}…". Called by the parent on bind (and new-chat); the actual
    /// placeholder swap happens in `render`, which has the `Window` that
    /// `InputState::set_placeholder` requires. Repaints on change.
    pub fn set_provider_label(&mut self, label: String, cx: &mut Context<Self>) {
        if self.provider_label != label {
            self.provider_label = label;
            cx.notify();
        }
    }

    /// Push the unbound *New Agent* draft's agent roster + current selection. The
    /// parent owns the truth (it rebuilds the backend on a pick and re-pushes);
    /// this view only renders the dropdown and emits [`ComposerEvent::AgentPicked`].
    /// `unbound = false` hides the agent picker (a bound chat never shows it).
    /// Only repaints when something actually changed.
    pub fn set_agent_picker(
        &mut self,
        unbound: bool,
        agents: Vec<(String, String)>,
        current: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        if self.unbound != unbound
            || self.agent_options != agents
            || self.current_agent != current
        {
            self.unbound = unbound;
            self.agent_options = agents;
            self.current_agent = current;
            cx.notify();
        }
    }

    /// Push the *New Agent* draft's worktree-isolation state, or `None` to hide
    /// the pill (bound chat / non-git project). The parent owns the flip and the
    /// slug's `InputState`; this view only renders. Only repaints when something
    /// actually changed.
    pub fn set_worktree_draft(&mut self, draft: Option<WorktreeDraft>, cx: &mut Context<Self>) {
        if self.worktree_draft != draft {
            self.worktree_draft = draft;
            cx.notify();
        }
    }

    /// Push the backend's advertised slash-command names (from `SessionInit`).
    /// A non-empty list enables the palette; an empty one disables it. Recomputes
    /// in case the caret already sits in a `/token` when the list arrives.
    pub fn set_slash_commands(
        &mut self,
        commands: Vec<String>,
        descriptions: std::collections::HashMap<String, String>,
        hints: std::collections::HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.slash_commands != commands
            || self.slash_descriptions != descriptions
            || self.slash_hints != hints
        {
            self.slash_commands = commands;
            self.slash_descriptions = descriptions;
            self.slash_hints = hints;
            self.recompute_slash_palette(cx);
            cx.notify();
        }
    }

    /// Push the on-disk command enrichment (descriptions + grouping), discovered
    /// off the main thread. Recomputes so an open palette regroups + gains
    /// descriptions the moment it lands.
    pub fn set_command_catalog(&mut self, catalog: CommandCatalog, cx: &mut Context<Self>) {
        self.slash_catalog = catalog;
        self.recompute_slash_palette(cx);
        cx.notify();
    }

    /// Push the project file list for `@file` autocomplete (scanned off the main
    /// thread by the parent). Marks the list loaded so the overlay can tell
    /// "scanning" from "no match", and refreshes an in-progress mention so it
    /// gains results the moment the scan lands.
    pub fn set_mention_candidates(&mut self, candidates: Vec<String>, cx: &mut Context<Self>) {
        self.mention_candidates = candidates;
        self.mention_candidates_loaded = true;
        // Only the mention overlay depends on this list; leave the slash palette.
        if self.palette.is_none() {
            self.recompute_mention(cx);
        }
        cx.notify();
    }

    /// The palette group for an advertised command name (defaults to Built-in
    /// when the catalog has no entry, e.g. before the scan lands or for a name
    /// with no on-disk definition).
    fn group_of(&self, name: &str) -> CommandGroup {
        self.slash_catalog.get(name).map(|m| m.group).unwrap_or(CommandGroup::BuiltIn)
    }

    /// Recompute both composer overlays after an edit. The slash palette wins
    /// when both could apply (a leading `/` vs an `@query`): compute it first and
    /// only try the mention overlay when the palette stayed closed, so at most one
    /// overlay is ever open.
    fn recompute_overlays(&mut self, cx: &mut Context<Self>) {
        // While browsing prompt history, don't pop the slash/mention overlays for
        // the recalled text — ↑/↓ belong to history until the user edits, which
        // detaches (and this then runs again with overlays enabled). Otherwise a
        // recalled `@file`/`/cmd` prompt would open an overlay that hijacks ↑/↓
        // and traps the user on one history entry.
        if self.history.is_navigating() {
            self.palette = None;
            self.mention = None;
            return;
        }
        self.recompute_slash_palette(cx);
        if self.palette.is_some() {
            self.mention = None;
        } else {
            self.recompute_mention(cx);
        }
    }

    /// Recompute the `@` overlay from the draft + caret. Opens whenever the caret
    /// sits inside an `@query` and the composer can send; ranks the context
    /// providers first (curated set) then file paths, and shows a scanning /
    /// no-match hint until something ranks. Preserves the highlighted row across a
    /// re-filter when it still matches. On a fresh open it raises
    /// [`ComposerEvent::MentionOpened`] so the parent can refresh the context
    /// sources (esp. the live terminal list).
    fn recompute_mention(&mut self, cx: &mut Context<Self>) {
        if self.turn_active || self.disconnected {
            self.mention = None;
            return;
        }
        let (text, cursor) = {
            let s = self.input.read(cx);
            (s.value().to_string(), s.cursor())
        };
        let Some(pm) = pending_mention(&text, cursor) else {
            self.mention = None;
            return;
        };
        let was_open = self.mention.is_some();
        let mut matches: Vec<MentionMatch> = rank_context_sources(&self.context_sources, &pm.query)
            .into_iter()
            .map(MentionMatch::Context)
            .collect();
        matches.extend(
            rank_mentions(&self.mention_candidates, &pm.query, MAX_SUGGESTIONS)
                .into_iter()
                .map(MentionMatch::File),
        );
        // Keep the same row highlighted across a re-filter (highlight-by-value),
        // else fall back to the top match. A `Context(i)` compares by its index
        // into `context_sources`; that's stable because the source list has a
        // fixed base order (diff, clipboard, then terminals in tab order), so a
        // refresh keeps each provider at the same index. A terminal closing while
        // the menu is open could at most drift the highlight one row — never
        // change what an accept captures (accept always reads the live list).
        let prev = self.mention.as_ref().and_then(|m| m.matches.get(m.highlight).cloned());
        let highlight = prev
            .and_then(|p| matches.iter().position(|m| *m == p))
            .unwrap_or(0);
        self.mention = Some(MentionState { range: pm.range, matches, highlight });
        if !was_open {
            cx.emit(ComposerEvent::MentionOpened);
        }
    }

    /// Move the mention highlight (`-1` up / `+1` down), wrapping. Returns whether
    /// the overlay was open (so the caller consumes the key — even over a
    /// scanning/no-match hint, so arrows don't move the text caret behind it).
    fn mention_move(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(m) = self.mention.as_mut() else { return false };
        let n = m.matches.len() as isize;
        if n > 0 {
            m.highlight = (((m.highlight as isize + delta) % n + n) % n) as usize;
            cx.notify();
        }
        true
    }

    /// Close the mention overlay (Esc) keeping the typed text. Returns whether it
    /// was open.
    fn mention_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.mention.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Accept the row at flat index `idx`. A file path replaces the `@query` with
    /// `@path ` (trailing space) and rides to the agent as ordinary text. A
    /// context provider instead *deletes* the `@query` (it becomes a chip, not
    /// inline text) and asks the parent to capture its content via
    /// [`ComposerEvent::CaptureContext`].
    fn mention_accept(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.mention.take() else { return };
        let range = m.range.clone();
        // What replaces the `@query` span, and whether to also capture context.
        // A file keeps its `@path ` inline; a provider deletes the token (it
        // becomes a chip) and asks the parent to capture its content.
        let (replacement, capture): (Option<String>, Option<ContextRequest>) =
            match m.matches.get(idx) {
                Some(MentionMatch::File(path)) => (Some(format!("@{path} ")), None),
                Some(MentionMatch::Context(src_idx)) => (
                    Some(String::new()),
                    self.context_sources.get(*src_idx).map(|s| s.request.clone()),
                ),
                None => (None, None),
            };
        if let Some(replacement) = replacement {
            let text = self.input.read(cx).value().to_string();
            if range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end)
            {
                let next =
                    format!("{}{replacement}{}", &text[..range.start], &text[range.end..]);
                self.set_draft_end(next, window, cx);
            }
        }
        if let Some(request) = capture {
            cx.emit(ComposerEvent::CaptureContext(request));
        }
        cx.notify();
    }

    /// Replace the whole draft with `next` and park the caret at its END. A bare
    /// `set_value` on this multi-line field parks the caret at the START, so
    /// after rewriting a `/command ` or `@path ` the user would be typing back at
    /// the top of the box. Rebuilding via clear + `insert` moves the caret to the
    /// end of the inserted text instead — right after the accepted token's
    /// trailing space — and, unlike `set_cursor_position`, never forces a focus
    /// relayout (which would need the window's `Root`).
    fn set_draft_end(&self, next: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.insert(next, window, cx);
        });
    }

    // ---- Voice dictation ------------------------------------------------

    /// The live dictation settings global (default when unset).
    fn dictation_settings(cx: &App) -> DictationSettings {
        cx.try_global::<DictationSettings>().cloned().unwrap_or_default()
    }

    /// Toggle dictation: start when idle (after enabled/model/permission checks),
    /// stop-and-insert when already recording. Bound to Cmd+E and the mic button.
    /// The shared pre-flight lives in [`dictation_service::prepare_start`] so the
    /// composer and the global HUD (terminal/editor dictation) behave identically.
    pub fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Already recording → stop (insert only; Enter path sets send_after).
        if self.dictation.is_active() {
            self.dictation_send_after = false;
            dictation_service::stop(cx);
            return;
        }
        match dictation_service::prepare_start(cx, window) {
            StartDecision::Ready { paths, device } => self.begin_recording(paths, device, cx),
            StartDecision::NeedsPermission { paths, device } => {
                // Request access; when granted, start on the main thread via a
                // oneshot the foreground task awaits (the block fires on an
                // arbitrary queue).
                let (tx, rx) = futures::channel::oneshot::channel::<bool>();
                crate::mic_permission::request(move |granted| {
                    let _ = tx.send(granted);
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(true) = rx.await {
                        let _ = this.update(cx, |this, cx| {
                            // A Hold-mode press that was released before the grant
                            // resolved must NOT start recording after the fact.
                            if std::mem::take(&mut this.dictation_hold_released) {
                                return;
                            }
                            this.begin_recording(paths, device, cx);
                        });
                    }
                })
                .detach();
            }
            StartDecision::Blocked => {}
        }
    }

    /// Hand the resolved model + device off to the service, moving to the
    /// Starting state. No `Window` needed (the transcript inserts later, on
    /// render).
    fn begin_recording(
        &mut self,
        paths: ModelPaths,
        device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.entity().downgrade();
        if !dictation_service::start(cx, DictationTarget::Composer(weak), paths, device) {
            self.pending_toast = Some("Dictation is busy in another composer".into());
            cx.notify();
            return;
        }
        self.dictation = DictationUiState::Starting;
        self.dictation_send_after = false;
        self.dictation_waveform.clear();
        cx.notify();
        self.spawn_recording_ticker(cx);
    }

    /// Repaint ~2×/sec while recording so the mm:ss timer advances (level events
    /// drive the meter). Self-terminates when the session ends.
    fn spawn_recording_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.dictation.is_active() {
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Fold a dictation event into the UI state. Called by [`dictation_service`]
    /// on the main thread; has no `Window`, so a final transcript is stashed for
    /// the next render.
    pub fn on_dictation_event(&mut self, ev: DictationEvent, cx: &mut Context<Self>) {
        match ev {
            DictationEvent::Started => {
                self.dictation = DictationUiState::Recording {
                    started_at: Instant::now(),
                };
            }
            DictationEvent::Level(level) => {
                self.dictation_waveform.push(level);
                cx.notify();
                return;
            }
            DictationEvent::Transcribing => {
                self.dictation = DictationUiState::Transcribing;
                self.dictation_waveform.clear();
            }
            DictationEvent::Capped => {
                self.pending_toast = Some("Recording stopped at the 2-minute limit".into());
            }
            DictationEvent::Cancelled => {
                self.dictation = DictationUiState::Idle;
                self.dictation_send_after = false;
            }
            DictationEvent::Final(text) => {
                // Either the explicit Enter-while-recording gesture, or the
                // auto-submit setting, sends once the transcript lands. Both ride
                // the same `send_after` path, so the empty-transcript guard below
                // covers auto-submit too — a silent recording never sends.
                let send_after = std::mem::take(&mut self.dictation_send_after) || dictation_service::auto_submit(cx);
                self.dictation = DictationUiState::Idle;
                if !text.trim().is_empty() {
                    self.pending_transcript = Some(PendingTranscript { text, send_after });
                }
            }
            DictationEvent::Error(msg) => {
                self.dictation = DictationUiState::Idle;
                self.dictation_send_after = false;
                self.pending_toast = Some(format!("Dictation error: {msg}"));
            }
        }
        cx.notify();
    }

    /// Insert a stashed transcript at the cursor and flush any pending toast.
    /// Called at the top of `render` (which owns the `Window`).
    fn apply_pending_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(msg) = self.pending_toast.take() {
            window.push_notification(msg, cx);
        }
        if let Some(pending) = self.pending_transcript.take() {
            let before = self.input.read(cx).value().to_string();
            let insert = dictation_spacing(&before, &pending.text, dictation_service::append_trailing_space(cx));
            if !insert.is_empty() {
                self.input.update(cx, |s, cx| s.insert(insert, window, cx));
            }
            if pending.send_after {
                self.submit(window, cx);
            }
        }
    }

    /// Escape while recording cancels + discards. Returns true when consumed.
    fn dictation_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dictation.is_active() {
            self.dictation_send_after = false;
            dictation_service::cancel(cx);
            return true;
        }
        false
    }

    /// Enter while recording stops + transcribes + submits. Returns true when
    /// consumed (so the field doesn't also insert a newline).
    fn dictation_enter(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dictation.is_active() {
            self.dictation_send_after = true;
            dictation_service::stop(cx);
            return true;
        }
        false
    }

    /// Accept the highlighted row (keyboard Enter/Tab). Returns `false` when the
    /// overlay is closed OR shows no real match, so Enter still submits / Tab
    /// still indents rather than being swallowed by an empty hint.
    fn mention_accept_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(m) = self.mention.as_ref() else { return false };
        if m.matches.is_empty() {
            return false;
        }
        let idx = m.highlight;
        self.mention_accept(idx, window, cx);
        true
    }

    /// Recompute the palette from the current draft + caret. Stateless: the
    /// palette opens only when the caret sits inside a `/token`, commands are
    /// advertised, and the composer can send (no in-flight turn / disconnect —
    /// which also covers a pending permission or question card, since the turn
    /// stays active until it resolves). Preserves the highlighted command across
    /// a re-filter when it still matches.
    fn recompute_slash_palette(&mut self, cx: &mut Context<Self>) {
        if self.slash_commands.is_empty() || self.turn_active || self.disconnected {
            self.palette = None;
            return;
        }
        let (text, cursor) = {
            let s = self.input.read(cx);
            (s.value().to_string(), s.cursor())
        };
        let Some(trigger) = detect_slash_trigger(&text, cursor) else {
            self.palette = None;
            return;
        };
        let mut matches = rank_commands(&trigger.query, &self.slash_commands);
        if matches.is_empty() {
            self.palette = None;
            return;
        }
        // Cluster the ranked results by group while keeping the group that holds
        // the best overall match first (and rank order within each group). A
        // stable sort keyed on each group's first appearance does exactly that.
        let mut first_seen: std::collections::HashMap<CommandGroup, usize> =
            std::collections::HashMap::new();
        for (i, &cmd) in matches.iter().enumerate() {
            first_seen.entry(self.group_of(&self.slash_commands[cmd])).or_insert(i);
        }
        matches.sort_by_key(|&cmd| first_seen[&self.group_of(&self.slash_commands[cmd])]);
        // Keep the same command highlighted across re-filter (highlight-by-id),
        // else fall back to the top match.
        let prev = self.palette.as_ref().and_then(|p| p.matches.get(p.highlight).copied());
        let highlight = prev
            .and_then(|cmd| matches.iter().position(|&m| m == cmd))
            .unwrap_or(0);
        self.palette = Some(SlashPaletteState { range: trigger.range, matches, highlight });
    }

    /// Move the palette highlight (`-1` up / `+1` down), wrapping. Returns
    /// whether a palette was open (so the caller consumes the key).
    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.palette.as_mut() else { return false };
        let n = p.matches.len() as isize;
        if n > 0 {
            p.highlight = (((p.highlight as isize + delta) % n + n) % n) as usize;
            cx.notify();
        }
        true
    }

    /// Close the palette (Esc) keeping the typed text. Returns whether it was open.
    fn palette_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.palette.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Accept a specific ranked match: replace the `/token` with `/name ` and a
    /// trailing space, ready for arguments. The command still rides to the agent
    /// as ordinary user text on submit. The caret is parked right after the
    /// trailing space (via [`Self::set_draft_with_caret`]) so the user keeps
    /// typing arguments there — a bare `set_value` on this multi-line field would
    /// instead jump the caret to the start of the box.
    fn palette_accept_match(&mut self, match_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.palette.take() else { return };
        if let Some(&cmd) = p.matches.get(match_idx)
            && let Some(name) = self.slash_commands.get(cmd)
        {
            let text = self.input.read(cx).value().to_string();
            // Guard against a stale range if the buffer moved underneath.
            if p.range.end <= text.len()
                && text.is_char_boundary(p.range.start)
                && text.is_char_boundary(p.range.end)
            {
                let next = format!("{}/{name} {}", &text[..p.range.start], &text[p.range.end..]);
                self.set_draft_end(next, window, cx);
            }
        }
        cx.notify();
    }

    /// Accept the highlighted match (keyboard Enter/Tab).
    fn palette_accept_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(highlight) = self.palette.as_ref().map(|p| p.highlight) else { return false };
        self.palette_accept_match(highlight, window, cx);
        true
    }

    /// The root Enter handler delegates here. Returns `true` when the keystroke
    /// was consumed (so the caller stops propagation and the field does NOT
    /// insert a newline); `false` lets `⇧↵` fall through to the field as a
    /// newline.
    ///
    /// - An open overlay always wins: ↵ accepts the highlighted slash command or
    ///   `@file` mention.
    /// - `⇧↵` (`shift`) inserts a newline (returns `false`).
    /// - A plain `↵` submits the message.
    ///
    /// The mouse Send button stays the IME-proof submit path for input methods
    /// (e.g. Vietnamese Telex) that swallow Enter before the app sees it.
    pub fn on_enter_key(
        &mut self,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.palette_accept_highlighted(window, cx) || self.mention_accept_highlighted(window, cx)
        {
            return true;
        }
        // Enter while recording stops → transcribes → submits (the transcript
        // lands and sends on the next render). Consume so no newline is inserted.
        if self.dictation_enter(cx) {
            return true;
        }
        if shift {
            // ⇧↵ inserts a newline — let it reach the field.
            return false;
        }
        self.submit(window, cx);
        true
    }

    /// Ask the parent to switch model / permission mode (the parent respawns the
    /// session and pushes the new value back via [`Self::set_controls`]).
    fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::ModelPicked(model));
    }

    fn pick_permission_mode(&mut self, mode: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::PermissionModePicked(mode));
    }

    /// Push the thinking-visibility chip state (`None` hides the chip).
    pub fn set_thinking_display(&mut self, wire: Option<String>, cx: &mut Context<Self>) {
        if self.thinking_display != wire {
            self.thinking_display = wire;
            cx.notify();
        }
    }

    fn pick_thinking_display(&mut self, wire: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::ThinkingDisplayPicked(wire));
    }

    fn pick_effort(&mut self, effort: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::EffortPicked(effort));
    }

    /// Ask the parent to apply a feature-control change (toggle flip or select
    /// pick). The parent applies it live or via respawn and re-pushes the vocab.
    fn pick_feature(&mut self, id: String, value: FeatureValue, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::FeaturePicked { id, value });
    }

    /// Ask the parent to switch the *picked* coding agent on the unbound draft
    /// (the parent rebuilds the backend + default model and re-pushes the picker).
    fn pick_agent(&mut self, id: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::AgentPicked(id));
    }

    /// Read + clear the draft. When the agent is idle this emits
    /// [`ComposerEvent::Submit`] straight away; while a turn is still streaming it
    /// **queues** the message instead — parked in [`Self::queued`] and drained by
    /// the parent (via [`Self::take_next_queued`]) as each turn completes, so the
    /// user can line up follow-ups without waiting. Inert only when disconnected.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.disconnected {
            return;
        }
        // Submitting while still recording (e.g. the mouse Send button mid-
        // dictation) cancels the in-flight session and discards its audio — so a
        // late transcript can't splice itself into a later, unrelated draft. The
        // Enter-to-send path doesn't hit this: it stops via `dictation_enter`,
        // and by the time its transcript re-enters `submit`, the state is Idle.
        if self.dictation.is_active() {
            self.dictation_send_after = false;
            dictation_service::cancel(cx);
        }
        let text = self.input.read(cx).value().to_string();
        let text = text.trim().to_string();
        // An attachment-only prompt (images or context chips, no caption) is
        // valid; only bail when there's nothing at all to send.
        if text.is_empty() && self.pending_images.is_empty() && self.context_chips.is_empty() {
            return;
        }
        let staged: Vec<PendingImage> = self.pending_images.drain(..).collect();
        let context: Vec<ContextChip> = self.context_chips.drain(..).collect();
        // Leaving the draft behind ends any history recall.
        self.history.detach();
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        if self.turn_active {
            // A turn is still streaming: park the message (bounded) and repaint
            // the queued-chip row. It sends when the turn ends. Context chips ride
            // along structured so they still serialize at drain.
            if self.queued.len() < MAX_QUEUED {
                self.queued.push(QueuedMessage { text, images: staged, context });
            }
            cx.notify();
            return;
        }
        self.history.record(&text);
        let images: Vec<ChatImage> = staged.into_iter().map(|p| p.chat).collect();
        // Prepend any context chips as `<context>` blocks; history keeps the
        // TYPED text (recall shows what the user wrote, not the wire form).
        let wire = prepend_context(&context, &text);
        cx.emit(ComposerEvent::Submit { text: wire, images });
    }

    /// Seed the ↑/↓ prompt history from a restored transcript's user prompts
    /// (oldest→newest). Called once at construction so a resumed chat can recall
    /// what was already sent.
    pub fn seed_history(&mut self, prompts: Vec<String>) {
        self.history.seed(prompts);
    }

    /// The current draft text (for edit-and-resend to stash before prefilling).
    pub fn current_draft(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// The currently staged (not-yet-sent) attachments, as wire `ChatImage`s —
    /// stashed alongside the draft so cancelling a staged edit restores them too.
    pub fn current_images(&self) -> Vec<ChatImage> {
        self.pending_images.iter().map(|p| p.chat.clone()).collect()
    }

    /// Replace the draft with `text` and restage `images` (edit-and-resend).
    /// Re-decodes each stored `ChatImage` into a renderable thumbnail; a corrupt
    /// image is dropped rather than aborting the prefill. Caret parks at the end.
    pub fn prefill(
        &mut self,
        text: String,
        images: Vec<ChatImage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.detach();
        self.pending_images = images
            .into_iter()
            .filter_map(|chat| {
                image_attach::decode_render(&chat).map(|render| PendingImage { chat, render })
            })
            .collect();
        self.set_draft_end(text, window, cx);
        cx.notify();
    }

    /// Pop the oldest queued message for the parent to send, recording it in the
    /// prompt history as it goes out (it's now a sent prompt). Returns `None` when
    /// nothing is queued. Repaints so the drained chip disappears.
    pub fn take_next_queued(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(String, Vec<ChatImage>)> {
        if self.queued.is_empty() {
            return None;
        }
        let QueuedMessage { text, images, context } = self.queued.remove(0);
        self.history.record(&text);
        cx.notify();
        let images: Vec<ChatImage> = images.into_iter().map(|p| p.chat).collect();
        let wire = prepend_context(&context, &text);
        Some((wire, images))
    }

    /// Pull the most-recently parked message back into the composer to edit it
    /// (↑ with an empty draft while messages are queued — the Claude-Desktop
    /// "arrow up to edit" gesture). Removes it from the queue and restores its
    /// staged attachments; re-submitting re-queues it (or sends it if the turn
    /// has since ended). Returns whether it consumed the key. Only fires with an
    /// empty draft and no staged attachments, so it never clobbers work in
    /// progress.
    fn edit_last_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.queued.is_empty() {
            return false;
        }
        if !self.draft_is_empty(cx) {
            return false;
        }
        let QueuedMessage { text, images, context } =
            self.queued.pop().expect("non-empty checked above");
        self.pending_images = images;
        self.context_chips = context;
        self.set_draft_end(text, window, cx);
        cx.notify();
        true
    }

    /// Pop a specific queued message (its chip's ✎) back into the composer to
    /// edit. Guarded on an empty draft + no staged images — mirrors
    /// [`Self::edit_last_queued`] so it never silently clobbers work in
    /// progress; the ✎ button is hidden when the draft is non-empty, this is
    /// the defense-in-depth backstop.
    fn edit_queued(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.queued.len() {
            return;
        }
        if !self.draft_is_empty(cx) {
            return;
        }
        let QueuedMessage { text, images, context } = self.queued.remove(idx);
        self.pending_images = images;
        self.context_chips = context;
        self.set_draft_end(text, window, cx);
        cx.notify();
    }

    /// Whether the draft is empty AND nothing is staged (no images, no context
    /// chips) — the precondition for pulling a queued message back to edit so it
    /// never clobbers work in progress.
    fn draft_is_empty(&self, cx: &Context<Self>) -> bool {
        self.input.read(cx).value().trim().is_empty()
            && self.pending_images.is_empty()
            && self.context_chips.is_empty()
    }

    /// Cancel a parked message (its chip's ✕).
    fn cancel_queued(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.queued.len() {
            self.queued.remove(idx);
            cx.notify();
        }
    }

    /// A queued chip's "send now" (↑).
    ///
    /// While IDLE, dequeue and submit right away — exactly what the user asked.
    ///
    /// Mid-turn it depends on the backend. A [`Self::can_steer`] one takes the
    /// message into the running turn and redirects the agent, so "now" is
    /// honoured literally ([`ComposerEvent::SteerNow`]). Otherwise there is
    /// nothing to hand it to, so the message only moves to the FRONT of the queue
    /// and goes out first when the turn ends (a no-op if already there).
    ///
    /// Steering is deliberately reachable ONLY here, never from `submit`: pi
    /// cannot un-queue a steered message, so handing one over must be an explicit
    /// act. A message parked as a chip stays editable and cancellable until the
    /// user presses this button.
    ///
    /// A message with attachments never steers — pi's `steer` does carry images,
    /// but OxiMux has never sent one and won't find out at the cost of silently
    /// dropping the user's. It reorders instead, keeping the images intact for
    /// the ordinary drain.
    fn send_queued_now(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.queued.len() {
            return;
        }
        if self.turn_active && !(self.can_steer && self.queued[idx].images.is_empty()) {
            if idx != 0 {
                let m = self.queued.remove(idx);
                self.queued.insert(0, m);
                cx.notify();
            }
            return;
        }
        let steering = self.turn_active;
        let QueuedMessage { text, images, context } = self.queued.remove(idx);
        self.history.record(&text);
        let images: Vec<ChatImage> = images.into_iter().map(|p| p.chat).collect();
        let wire = prepend_context(&context, &text);
        cx.notify();
        if steering {
            cx.emit(ComposerEvent::SteerNow { text: wire });
        } else {
            cx.emit(ComposerEvent::Submit { text: wire, images });
        }
    }

    /// The text of each queued message, oldest first, for persistence. Text-only
    /// (staged images/context aren't persisted); an image-only queued message
    /// (blank text) is skipped since it can't be faithfully restored.
    pub fn queued_texts(&self) -> Vec<String> {
        self.queued.iter().map(|m| m.text.clone()).filter(|t| !t.trim().is_empty()).collect()
    }

    /// Seed the draft from restored text, but ONLY when the composer is empty —
    /// never clobber text the user is already typing (mirrors the edit-queued
    /// no-clobber guard). An empty `text` is a no-op.
    pub fn seed_draft(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if !text.trim().is_empty() && self.draft_is_empty(cx) {
            self.set_draft_end(text, window, cx);
        }
    }

    /// Re-seed queued chips from restored text (text-only, no images/context).
    /// Never auto-sends — a restored app must not fire billed sends without a user
    /// action; the chips sit visible with their edit/cancel/send-now affordances.
    pub fn seed_queued(&mut self, texts: Vec<String>, cx: &mut Context<Self>) {
        for text in texts {
            if self.queued.len() >= MAX_QUEUED {
                break;
            }
            if text.trim().is_empty() {
                continue;
            }
            self.queued.push(QueuedMessage { text, images: Vec::new(), context: Vec::new() });
        }
        cx.notify();
    }

    /// Whether the caret sits on the first visual line of the draft (no newline
    /// before it) — the entry condition for ↑ recalling history rather than
    /// moving the caret up a line in a multi-line draft.
    fn caret_on_first_line(&self, cx: &Context<Self>) -> bool {
        let s = self.input.read(cx);
        let cursor = s.cursor();
        let text = s.value();
        !text.get(..cursor).is_some_and(|before| before.contains('\n'))
    }

    /// Load a recalled history entry into the input, parking the caret at the end
    /// (as if freshly typed). The Change echo is ignored by the subscription's
    /// navigation guard, so this doesn't detach.
    fn load_history(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_draft_end(text, window, cx);
        cx.notify();
    }

    /// ↑ handler: recall an older prompt. Returns whether the key was consumed.
    /// Only enters history from the first line (so ↑ still moves the caret up in a
    /// multi-line draft); once navigating, ↑ keeps walking regardless of caret.
    /// Falls through (returns `false`) only when there's no history to show.
    fn history_older(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.history.is_navigating() && !self.caret_on_first_line(cx) {
            return false;
        }
        let live = self.input.read(cx).value().to_string();
        match self.history.older(&live) {
            Some(text) => {
                self.load_history(text, window, cx);
                true
            }
            None => false, // empty history — let the caret move
        }
    }

    /// ↓ handler: recall a newer prompt (or restore the live draft past the
    /// newest). Consumes the key only while navigating; otherwise falls through so
    /// ↓ moves the caret down a line.
    fn history_newer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.history.newer() {
            Some(text) => {
                self.load_history(text, window, cx);
                true
            }
            None => false,
        }
    }

    /// Ask the parent to interrupt the in-flight turn (the Stop button). Leaves
    /// the draft untouched so the user can send it once the turn is stopped.
    fn request_stop(&mut self, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::Stop);
    }

    /// The agent control in the bottom toolbar of an unbound *New Agent* draft:
    /// a flat ghost button labeled with the picked agent, opening the chat roster
    /// upward. Picking an agent rebinds the model picker beside it (the parent
    /// pushes that agent's static model vocab). Shown only while unbound — a bound
    /// chat's transport is fixed. Sits just before the model picker so the two
    /// read together ("Claude ▾  opus ▾").
    fn render_agent_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let agents = self.agent_options.clone();
        let current_id =
            self.current_agent.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
        let current_label = self
            .current_agent
            .as_ref()
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| "Agent".to_string());
        // The picked agent's brand glyph (Claude/Codex marks; a generic sparkles
        // for agents without one), shown as the provider icon on the control.
        let trigger_icon = adapter_icon_path(&current_id);
        let trigger = Button::new("chat-agent-btn")
            .icon(Icon::default().path(trigger_icon))
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true);
        // Icon-bearing rows (each agent's brand glyph + a ✓ on the pick), so this
        // can't reuse `render_labeled_dropdown`'s plain rows — but it shares the
        // same Popover + centered/self-hiding tooltip via `render_dropdown_shell`.
        let build_menu =
            move |mut menu: PopupMenu, window: &mut Window, _cx: &mut Context<PopupMenu>| {
                for (id, display) in &agents {
                    let selected = current_id == *id;
                    let icon_path = adapter_icon_path(id);
                    let text = if selected {
                        format!("\u{2713} {display}")
                    } else {
                        display.clone()
                    };
                    let choice = id.to_string();
                    let entity = entity.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_w, _c| {
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(Icon::default().path(icon_path).size(px(14.0)))
                                .child(text.clone())
                        })
                        .on_click(window.listener_for(
                            &entity,
                            move |view: &mut ComposerView, _ev: &gpui::ClickEvent, _w, cx| {
                                view.pick_agent(choice.clone(), cx);
                            },
                        )),
                    );
                }
                menu
            };
        self.render_dropdown_shell(
            "chat-agent".into(),
            "Coding agent".into(),
            trigger,
            Anchor::BottomRight,
            build_menu,
            cx,
        )
    }

    /// The model control in the bottom toolbar: a borderless (`appearance(false)`)
    /// searchable dropdown so it reads like the sibling ghost pickers while giving
    /// long ACP model lists (OpenCode advertises dozens) a filter box. The trigger
    /// shows the current model's short (namespace-stripped) label; the dropdown
    /// rows show full labels. State + selection are seeded from `render`; a pick
    /// routes through the Confirm subscription to `pick_model`.
    fn render_model_select(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        // `flex_none` (no explicit width) lets the borderless Select hug its
        // content — a compact "label ⌄" trigger like the sibling ghost pickers,
        // rather than stretching across the toolbar. The
        // dropdown itself stays a fixed, searchable width.
        div().flex_none().child(
            Select::new(&self.model_select)
                // Wide enough that a model's one-line capability blurb (the muted
                // second row) fits without clipping to an ellipsis.
                .appearance(false)
                .small()
                .menu_width(px(320.0))
                .search_placeholder("Search models…"),
        )
    }

    /// The *New Agent* draft's worktree control: a ghost pill in the bottom
    /// toolbar, sibling to the agent/model pickers, opening a popover with the
    /// two isolation choices and (when a worktree is picked) the branch slug.
    ///
    /// This cannot reuse [`Self::render_dropdown_shell`] — that builds a
    /// `PopupMenu`, whose rows are menu items and cannot host a text field. So it
    /// drives `Popover` directly with a panel, mirroring the shell's popover
    /// setup (`appearance(false)` + upward anchor + `open_dropdown` tracking) so
    /// it still reads and behaves like its siblings.
    ///
    /// Placement matters: living inside the toolbar means the control inherits
    /// the composer's centered reading column instead of laying itself out
    /// independently — which is what stranded the previous full-width checkbox
    /// ~97px to the left of every sibling (and further the wider the window got).
    fn render_worktree_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.worktree_draft.clone()?;
        let (theme, density) = (self.theme, self.density);
        let typo = self.typography.clone();
        let view = cx.entity();

        // Label: the slug when a worktree is armed (it's the thing the user
        // actually wants to see at a glance), "Local" otherwise.
        let label = if draft.enabled {
            self.worktree_slug_text(cx).unwrap_or_else(|| "Worktree".to_string())
        } else {
            "Local".to_string()
        };
        let trigger = Button::new("chat-worktree-btn")
            .icon(Icon::default().path(if draft.enabled {
                "icons/git-branch.svg"
            } else {
                "icons/folder-plus.svg"
            }))
            .label(label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .disabled(draft.busy);

        let popover = Popover::new("chat-worktree-pop")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .on_open_change({
                let view = view.clone();
                move |open: &bool, _window, cx| {
                    let open = *open;
                    view.update(cx, |v, cx| {
                        if open {
                            v.open_dropdown = Some(SharedString::from("chat-worktree"));
                        } else if v.open_dropdown.as_deref() == Some("chat-worktree") {
                            v.open_dropdown = None;
                        }
                        cx.notify();
                    });
                }
            })
            .content({
                let view = view.clone();
                let draft = draft.clone();
                move |_state, _window, cx| {
                    let view = view.clone();
                    let draft = draft.clone();
                    super::composer_worktree::worktree_popover_panel(
                        draft, view, theme, &typo, density, cx,
                    )
                }
            });

        // No tooltip, unlike the sibling pickers: theirs explain abbreviated
        // labels ("Opus", "high"), while this one already reads as a full phrase
        // next to a branch glyph. The siblings' tooltip is also a custom
        // `group_hover` overlay owned by `render_dropdown_shell` (gpui's native
        // one anchors to the cursor, off-center), and reproducing that here to
        // caption a self-describing control isn't worth the duplication.
        Some(div().flex_none().child(popover).into_any_element())
    }

    /// The slug field's current text, read through the shared handle the parent
    /// owns — so the pill's label tracks typing without the composer keeping a
    /// copy that could drift.
    fn worktree_slug_text(&self, cx: &App) -> Option<String> {
        let input = self.worktree_draft.as_ref()?.slug_input.as_ref()?;
        let text = input.read(cx).value().trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// The shared shell behind every footer dropdown (permission / effort /
    /// feature-select / agent picker): a ghost trigger button that opens a menu
    /// upward, plus a hover tooltip centered directly above the control and
    /// SUPPRESSED while its own menu is open (a trigger's tooltip is hidden while
    /// its dropdown is open). Callers pass the fully-built
    /// trigger and a `build_menu` closure so each control keeps its own row shape
    /// (plain checkmark rows for the labeled pickers, icon rows for the agent
    /// picker); this owns the `Popover` + open-state tracking + tooltip so the
    /// behavior stays identical across every control.
    fn render_dropdown_shell(
        &self,
        ctrl_id: SharedString,
        tooltip_text: SharedString,
        trigger: Button,
        // Which corner of the popup anchors to the trigger. Far-RIGHT toolbar
        // pickers (model/effort/agent) use `BottomRight` so the menu grows up
        // and to the LEFT, staying inside the window. Left-of-toolbar controls
        // (the mic device menu) pass `BottomLeft` so it opens up and to the
        // RIGHT off the button — the Claude Desktop mic-popover placement.
        anchor: Anchor,
        build_menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity();
        let is_open = self.open_dropdown.as_deref() == Some(ctrl_id.as_ref());

        let menu_key = SharedString::from(format!("{ctrl_id}-menu"));
        let wrap_id = SharedString::from(format!("{ctrl_id}-wrap"));
        let pop_id = SharedString::from(format!("{ctrl_id}-pop"));
        let build_menu = std::rc::Rc::new(build_menu);

        let popover = Popover::new(pop_id)
            .appearance(false)
            .overlay_closable(false)
            .anchor(anchor)
            .trigger(trigger)
            .on_open_change({
                let view = view.clone();
                let ctrl_id = ctrl_id.clone();
                move |open: &bool, _window, cx| {
                    let open = *open;
                    let ctrl_id = ctrl_id.clone();
                    view.update(cx, |v, cx| {
                        if open {
                            v.open_dropdown = Some(ctrl_id);
                        } else if v.open_dropdown.as_deref() == Some(ctrl_id.as_ref()) {
                            v.open_dropdown = None;
                        }
                        cx.notify();
                    });
                }
            })
            .content({
                let build_menu = build_menu.clone();
                move |_state, window, cx| {
                    // Build (and cache) the menu once; rebuild on dismiss so the
                    // checkmark tracks the current value on the next open.
                    let menu_state = window
                        .use_keyed_state(menu_key.clone(), cx, |_, _| None::<Entity<PopupMenu>>);
                    match menu_state.read(cx).clone() {
                        Some(menu) => menu,
                        None => {
                            let build_menu = build_menu.clone();
                            let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                                build_menu(menu, window, cx)
                            });
                            // Close the popover + drop the cache when the menu
                            // dismisses (pick, escape, click-away) — this also
                            // fires `on_open_change(false)`, clearing the tooltip
                            // suppression.
                            let popover_entity = cx.entity();
                            let menu_state2 = menu_state.clone();
                            window
                                .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                                    popover_entity.update(cx, |st, cx| st.dismiss(window, cx));
                                    menu_state2.update(cx, |s, _| *s = None);
                                })
                                .detach();
                            menu_state.update(cx, |s, _| *s = Some(menu.clone()));
                            menu
                        }
                    }
                }
            });

        // Hover tooltip, centered directly above the control and suppressed while
        // the menu is open. It is an absolute, full-width, center-justified overlay
        // so it centers over the button WITHOUT measuring the label, and
        // `group_hover` reveals it on hover. gpui's native `.tooltip()` anchors to
        // the mouse cursor (so it drifted off to the button's side); the managed
        // Button tooltip centers but can't be hidden when the upward menu opens —
        // this hand-rolled one gives both centering AND open-time suppression.
        let theme = self.theme;
        let body_sm = self.typography.t_body_sm;
        let group_name = SharedString::from(format!("{ctrl_id}-grp"));
        let mut wrap = div().id(wrap_id).relative().group(group_name.clone()).child(popover);
        if !is_open {
            wrap = wrap.child(
                div()
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .w_full()
                    .pb(px(6.0))
                    .flex()
                    .justify_center()
                    .invisible()
                    .group_hover(group_name, |s| s.visible())
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(self.density.r_xs))
                            .bg(theme.bg_overlay)
                            .border_1()
                            .border_color(theme.border_inactive)
                            .text_color(theme.fg_base)
                            .text_size(px(body_sm))
                            .shadow_md()
                            .child(tooltip_text.clone()),
                    ),
            );
        }
        wrap
    }

    /// A footer dropdown control (the shared shape behind permission / effort /
    /// feature-select): an icon + current-label ghost button that opens a
    /// checkmarked menu upward. Builds the trigger and the plain (checkmark +
    /// label) rows, then delegates the Popover + tooltip behavior to
    /// [`Self::render_dropdown_shell`].
    #[allow(clippy::too_many_arguments)]
    fn render_labeled_dropdown(
        &self,
        ctrl_id: SharedString,
        icon_path: &'static str,
        tooltip_text: SharedString,
        current_label: String,
        current_wire: String,
        items: Vec<(String, String)>,
        on_pick: DropdownPick,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity();
        let trigger = Button::new(SharedString::from(format!("{ctrl_id}-btn")))
            .icon(Icon::default().path(icon_path))
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true);
        let items = std::rc::Rc::new(items);
        let current = current_wire;
        let build_menu =
            move |mut menu: PopupMenu, window: &mut Window, _cx: &mut Context<PopupMenu>| {
                for (wire, label) in items.iter() {
                    let selected = *wire == current;
                    let display = if selected {
                        format!("\u{2713} {label}")
                    } else {
                        format!("   {label}")
                    };
                    let wire = wire.clone();
                    let on_pick = on_pick.clone();
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_w, _c| div().child(display.clone())).on_click(
                            window.listener_for(
                                &view,
                                move |v: &mut ComposerView, _e: &gpui::ClickEvent, _w, cx| {
                                    on_pick(v, wire.clone(), cx);
                                },
                            ),
                        ),
                    );
                }
                menu
            };
        self.render_dropdown_shell(ctrl_id, tooltip_text, trigger, Anchor::BottomRight, build_menu, cx)
    }

    /// The permission-mode control in the bottom toolbar: a flat ghost button
    /// (label + subtle caret) labeled with the current mode, opening the
    /// canonical mode menu upward.
    fn render_permission_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let modes: Vec<(String, String)> = self
            .vocab
            .permission_modes
            .iter()
            .map(|m| (m.wire.clone(), m.label.clone()))
            .collect();
        let current_wire = self
            .permission_mode
            .clone()
            .or_else(|| self.vocab.default_mode.clone())
            .unwrap_or_default();
        let current_label = modes
            .iter()
            .find(|(w, _)| *w == current_wire)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| current_wire.clone());
        self.render_labeled_dropdown(
            "chat-perm-mode".into(),
            "icons/lock.svg",
            "Permission mode".into(),
            current_label,
            current_wire,
            modes,
            std::rc::Rc::new(|view: &mut ComposerView, wire, cx| view.pick_permission_mode(wire, cx)),
            cx,
        )
    }

    /// The reasoning-effort control in the bottom toolbar: a flat ghost button
    /// (label + subtle caret) labeled with the current effort, opening the level
    /// menu upward.
    fn render_effort_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let efforts: Vec<(String, String)> = self
            .vocab
            .efforts
            .iter()
            .map(|e| (e.wire.clone(), e.label.clone()))
            .collect();
        let current_wire = self
            .effort
            .clone()
            .or_else(|| self.vocab.default_effort.clone())
            .unwrap_or_default();
        let current_label = efforts
            .iter()
            .find(|(w, _)| *w == current_wire)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| current_wire.clone());
        self.render_labeled_dropdown(
            "chat-effort".into(),
            "icons/sparkles.svg",
            "Reasoning effort".into(),
            current_label,
            current_wire,
            efforts,
            std::rc::Rc::new(|view: &mut ComposerView, wire, cx| view.pick_effort(wire, cx)),
            cx,
        )
    }

    /// The thinking-visibility control beside the effort picker — "how hard it
    /// thinks" next to "whether you see it". Off hides thinking blocks, Auto
    /// expands the streaming one and collapses settled ones, Always keeps every
    /// block open. A transcript view preference; no backend involved.
    fn render_thinking_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options: Vec<(String, String)> = [("off", "Off"), ("auto", "Auto"), ("shown", "Always")]
            .into_iter()
            .map(|(w, l)| (w.to_string(), l.to_string()))
            .collect();
        let current_wire = self.thinking_display.clone().unwrap_or_else(|| "auto".to_string());
        let current_label = options
            .iter()
            .find(|(w, _)| *w == current_wire)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| current_wire.clone());
        self.render_labeled_dropdown(
            "chat-thinking".into(),
            "icons/eye.svg",
            "Thinking visibility".into(),
            current_label,
            current_wire,
            options,
            std::rc::Rc::new(|view: &mut ComposerView, wire, cx| {
                view.pick_thinking_display(wire, cx)
            }),
            cx,
        )
    }

    /// Map a backend's *semantic* feature-icon hint (e.g. `"zap"`, `"plan"`) to a
    /// bundled asset path. The agents crate emits only the hint so it carries no
    /// UI asset path; unknown/`None` hints fall back to a neutral settings glyph.
    fn feature_icon_path(hint: Option<&str>) -> &'static str {
        match hint {
            Some("zap" | "fast") => "icons/sparkles.svg",
            Some("plan" | "list-todo" | "list") => "icons/list-tree.svg",
            Some("bot" | "check" | "auto-accept") => "icons/check.svg",
            Some("shield" | "lock" | "permission") => "icons/lock.svg",
            Some("file" | "file-text" | "context") => "icons/file-text.svg",
            _ => "icons/settings-2.svg",
        }
    }

    /// The generic feature-control cluster (fast / plan / auto-accept /
    /// agent-profile …) the backend advertised, rendered to the right of the
    /// effort picker. A `Toggle` is a compact icon button that shows a selected
    /// state when on; a `Select` is a labeled dropdown (same shape as the effort
    /// picker). Renders nothing when the backend advertises no features.
    fn render_feature_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let density = self.density;
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline));
        for feature in &self.vocab.features {
            let btn_id = SharedString::from(format!("chat-feat-{}", feature.id));
            match &feature.kind {
                FeatureKind::Toggle { on } => {
                    let on = *on;
                    let id = feature.id.clone();
                    // The glyph itself carries the state, and the tooltip says it
                    // in words. A ghost button's `selected` styling alone was not
                    // legible at this size — the control rendered IDENTICALLY on
                    // and off, so flipping it looked like a dead click even though
                    // it was respawning the agent underneath. For a switch that
                    // decides whether repo instruction files reach the model, an
                    // unreadable state is worse than no control.
                    let icon = if on {
                        Self::feature_icon_path(feature.icon.as_deref())
                    } else {
                        "icons/circle-slash.svg"
                    };
                    let tooltip = SharedString::from(format!(
                        "{} · {}",
                        feature.label,
                        if on { "on" } else { "off" }
                    ));
                    row = row.child(
                        Button::new(btn_id)
                            .icon(Icon::default().path(icon))
                            .ghost()
                            .small()
                            .selected(on)
                            .tooltip(tooltip)
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.pick_feature(id.clone(), FeatureValue::Bool(!on), cx);
                            })),
                    );
                }
                FeatureKind::Select { options, selected } => {
                    let opts: Vec<(String, String)> =
                        options.iter().map(|o| (o.wire.clone(), o.label.clone())).collect();
                    let current_wire = selected.clone().unwrap_or_default();
                    let current_label = opts
                        .iter()
                        .find(|(w, _)| *w == current_wire)
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| feature.label.clone());
                    let fid = feature.id.clone();
                    let on_pick: DropdownPick =
                        std::rc::Rc::new(move |view: &mut ComposerView, wire, cx| {
                            view.pick_feature(fid.clone(), FeatureValue::Choice(wire), cx);
                        });
                    row = row.child(self.render_labeled_dropdown(
                        btn_id,
                        Self::feature_icon_path(feature.icon.as_deref()),
                        feature.label.clone().into(),
                        current_label,
                        current_wire,
                        opts,
                        on_pick,
                        cx,
                    ));
                }
            }
        }
        row
    }

    /// The "Import session" control, shown only in the unbound *New Agent* flow:
    /// an outlined pill that sits ABOVE the input box (matching the reference
    /// cockpit's placement, not buried in the toolbar row) and opens the shared
    /// session-history modal — the same list the Cmd+Shift+H picker shows — from
    /// which a past Claude or Codex session reopens as a chat tab. One list, two
    /// entry points.
    fn render_import_session_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("chat-import-session-btn")
            .icon(Icon::default().path("icons/history.svg"))
            .label("Import session")
            .outline()
            .small()
            .tooltip("Reopen a past Claude or Codex session as a chat")
            .on_click(cx.listener(|_this, _ev, window, cx| {
                window.dispatch_action(Box::new(crate::actions::OpenSessionHistory), cx);
            }))
    }

    /// The row above the input pill in the New Agent draft: **where this agent
    /// runs** (the worktree pill) plus Import session, left-aligned against the
    /// reading column's left edge.
    ///
    /// The split is deliberate and follows Claude Desktop: session *context* —
    /// what this agent is pointed at — sits ABOVE the input, while the toolbar
    /// BELOW carries how it behaves (safety mode, model, effort). The worktree
    /// pick is the former: it's answered once, before the first send, and then
    /// never again for the life of the session.
    ///
    /// Rendered only while unbound; once a subprocess is bound, neither importing
    /// nor re-rooting applies.
    fn render_context_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(self.density.gap_inline))
            // Leftmost, mirroring Claude Desktop's "Local" chip: the first
            // question is where the work lands.
            .children(self.render_worktree_picker(cx))
            .child(self.render_import_session_button(cx))
    }

    /// The image-attach control (far left of the toolbar): a flat ghost button
    /// that opens the native image picker. Always enabled — attachments stage
    /// for the next send even while a turn streams.
    fn render_attach_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("chat-attach-btn")
            .icon(Icon::default().path("icons/image.svg"))
            .ghost()
            .small()
            .tooltip("Attach image")
            .on_click(cx.listener(|this, _ev, _window, cx| this.attach_from_picker(cx)))
    }

    /// The idle mic control: the mic button plus a chevron opening the
    /// device-picker + Hold-to-record menu (Claude-Desktop style). In Hold mode
    /// the mic is press-and-hold (mouse down starts, release stops+inserts); in
    /// Toggle mode a click toggles. The tooltip reflects model readiness so the
    /// user knows why a click won't record yet.
    ///
    /// Hold caveat: the up-listener fires only when the release lands on the
    /// button (gpui has no window-wide mouse-up capture), so a press-drag-off
    /// leaves the session running — recoverable via the recording bar's Stop /
    /// Cancel or ⌘E. Acceptable for a hold gesture users rarely drag off.
    fn render_mic_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = Self::dictation_settings(cx);
        let model_id = settings.model_id.clone();
        let hold = settings.mode.is_hold();
        // Tooltip reflects model readiness so the user knows why a click won't
        // record yet (the click still surfaces an actionable toast).
        let tooltip = match dictation_service::readiness(cx, &model_id) {
            Readiness::Ready if hold => "Hold to dictate (⌘E)".to_string(),
            Readiness::Ready => "Dictate (⌘E)".to_string(),
            Readiness::Downloading(p) => {
                format!("Downloading model… {}%", (p * 100.0).round() as u32)
            }
            Readiness::Missing => "Download a voice model in Settings › Voice".to_string(),
        };

        let theme = self.theme;
        let mic: AnyElement = if hold {
            // Press-and-hold: start on mouse-down, stop+insert on release. Both
            // go through `toggle_dictation`, which flips on the current state.
            div()
                .id("chat-dictate-btn")
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded(px(self.density.r_xs))
                .text_color(theme.fg_muted)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, window, cx| {
                        if !this.dictation.is_active() {
                            this.dictation_hold_released = false;
                            this.toggle_dictation(window, cx);
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _ev, window, cx| {
                        if this.dictation.is_active() {
                            this.toggle_dictation(window, cx);
                        } else {
                            // Released before an async permission grant resolved
                            // (first use): tell the pending start to abort so the
                            // mic doesn't go hot after the gesture ended.
                            this.dictation_hold_released = true;
                        }
                    }),
                )
                .child(Icon::default().path("icons/mic.svg"))
                .into_any_element()
        } else {
            Button::new("chat-dictate-btn")
                .icon(Icon::default().path("icons/mic.svg"))
                .ghost()
                .small()
                .tooltip(tooltip)
                .on_click(cx.listener(|this, _ev, window, cx| this.toggle_dictation(window, cx)))
                .into_any_element()
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .child(mic)
            .child(self.render_mic_menu(&settings, cx))
            .into_any_element()
    }

    /// The mic device-picker + Hold-to-record dropdown (a chevron next to the
    /// mic). Lists the system-default input plus every enumerated device with a
    /// checkmark on the active one, then a Hold-to-record toggle. Picking a row
    /// persists to `dictation.toml` (the settings watcher swaps the global).
    fn render_mic_menu(
        &self,
        settings: &DictationSettings,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity();
        let current_device = settings.device_name().unwrap_or_default();
        let hold = settings.mode.is_hold();
        let trigger = Button::new("chat-dictate-menu-btn")
            .icon(Icon::default().path("icons/chevron-down.svg"))
            .ghost()
            .small();

        let build_menu = move |mut menu: PopupMenu,
                               window: &mut Window,
                               _cx: &mut Context<PopupMenu>| {
            menu = menu.label("Microphone");
            let mut options: Vec<(String, String)> =
                vec![(String::new(), "System default".to_string())];
            // Enumerate devices only when the menu actually opens (a CoreAudio
            // HAL call — must never run on the per-render/per-keystroke path).
            for d in oximux_dictation::list_input_devices() {
                options.push((d.clone(), d));
            }
            for (wire, label) in options {
                let selected = current_device == wire;
                let display = if selected {
                    format!("\u{2713} {label}")
                } else {
                    format!("   {label}")
                };
                let view = view.clone();
                menu = menu.item(
                    PopupMenuItem::element(move |_w, _c| div().child(display.clone())).on_click(
                        window.listener_for(
                            &view,
                            move |v: &mut ComposerView, _e: &gpui::ClickEvent, _w, cx| {
                                let dev =
                                    (!wire.is_empty()).then(|| wire.clone());
                                v.update_dictation_settings(cx, |s| s.input_device = dev.clone());
                            },
                        ),
                    ),
                );
            }
            menu = menu.separator();
            let hold_display = if hold {
                "\u{2713} Hold to record".to_string()
            } else {
                "   Hold to record".to_string()
            };
            let view = view.clone();
            menu = menu.item(
                PopupMenuItem::element(move |_w, _c| div().child(hold_display.clone())).on_click(
                    window.listener_for(
                        &view,
                        move |v: &mut ComposerView, _e: &gpui::ClickEvent, _w, cx| {
                            v.update_dictation_settings(cx, |s| {
                                s.mode = if s.mode.is_hold() {
                                    DictationMode::Toggle
                                } else {
                                    DictationMode::Hold
                                };
                            });
                        },
                    ),
                ),
            );
            menu
        };

        self.render_dropdown_shell(
            "chat-dictate-menu".into(),
            "Microphone & mode".into(),
            trigger,
            // Open up-and-to-the-RIGHT off the mic button (Claude Desktop
            // placement), not up-left over the transcript.
            Anchor::BottomLeft,
            build_menu,
            cx,
        )
    }

    /// Read the dictation settings global, apply `f`, sanitize, and persist to
    /// `dictation.toml`. Used by the mic menu (device + Hold toggle). Writes the
    /// file ONLY — the settings watcher reparses + swaps the global (the module
    /// contract: writers must not `set_global` themselves and race the
    /// debouncer). The swap lands within the debounce window (~250 ms).
    fn update_dictation_settings(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DictationSettings),
    ) {
        let mut s = Self::dictation_settings(cx);
        f(&mut s);
        let s = s.sanitized();
        if let Err(err) = crate::dictation_settings::save(&s) {
            tracing::warn!(%err, "composer: failed to persist dictation settings");
        }
    }

    /// The active recording bar — a ChatGPT-style row that OVERLAYS the input
    /// field (absolute, opaque): a filled stop square (■ = stop + insert), a live
    /// scrolling waveform filling the width, the mm:ss timer, and cancel
    /// (✕ / Esc) + send (↑ / Enter) controls. Mounted as an absolute child of the
    /// input pill (which stays mounted underneath so focus + Escape/Enter keep
    /// working).
    fn render_recording_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let (status_text, timer_text, recording) = match &self.dictation {
            DictationUiState::Recording { started_at } => {
                (String::new(), format_elapsed(*started_at), true)
            }
            DictationUiState::Starting => ("Starting…".to_string(), String::new(), false),
            DictationUiState::Transcribing => ("Transcribing…".to_string(), String::new(), false),
            _ => (String::new(), String::new(), false),
        };
        let bars = self.dictation_waveform.filled_bars(22.0, 0.05);

        // Stop (■): finalize the recording and insert into the box (no send).
        let stop_square = div()
            .id("chat-dictate-stop")
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .size(px(20.0))
            .rounded(px(density.r_xs))
            .bg(theme.status_error)
            .cursor_pointer()
            .hover(|s| s.opacity(0.85))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, window, cx| this.toggle_dictation(window, cx)),
            )
            .child(div().size(px(9.0)).rounded(px(density.r_chip)).bg(theme.bg_base));

        // Cancel (✕): discard the recording.
        let cancel = div()
            .id("chat-dictate-cancel")
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .size(px(24.0))
            .rounded(px(density.r_xs))
            .text_color(theme.fg_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.dictation_escape(cx);
                }),
            )
            .child(Icon::default().path("icons/x.svg").size(px(14.0)));

        // Send (↑): stop + submit. Mirrors the composer's circular send button.
        let send = div()
            .id("chat-dictate-send")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(28.0))
            .rounded_full()
            .bg(theme.status_info)
            .text_color(theme.bg_base)
            .cursor_pointer()
            .hover(|s| s.opacity(0.85))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.dictation_enter(cx);
                }),
            )
            .child(
                div().relative().top(px(1.0)).child(
                    Icon::default()
                        .path("icons/arrow-up.svg")
                        .size(px(15.0))
                        .text_color(theme.bg_base),
                ),
            );

        // Center: the scrolling waveform (right-aligned so the newest audio shows
        // and older bars clip off the left) while recording, else the status word.
        let center = if recording {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .child(render_waveform(
                    &bars,
                    WaveformStyle {
                        height: 22.0,
                        bar_w: 2.5,
                        gap: 2.0,
                        color: theme.status_error,
                        // Spread the bars across the whole bar so the waveform
                        // fills it edge-to-edge (ChatGPT desktop app), instead
                        // of a short right-aligned cluster leaving empty space.
                        fill: true,
                    },
                ))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(status_text)
                .into_any_element()
        };

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .rounded(px(density.r_xl))
            .bg(theme.bg_panel_alt)
            .px(px(density.pad_panel))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_base)
            .child(stop_square)
            .child(center)
            .when(recording, |d| {
                d.child(div().flex_none().child(timer_text.clone()))
            })
            .child(cancel)
            .child(send)
            .into_any_element()
    }

    /// Staged-attachment chips shown above the input pill: a small thumbnail per
    /// pending image, each with a ✕ to remove it. Rendered only when something is
    /// staged. Thumbnails come pre-decoded (see [`PendingImage`]) so this row is
    /// cheap to repaint per keystroke.
    fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .w_full()
            .gap(px(density.gap_inline));
        for (idx, p) in self.pending_images.iter().enumerate() {
            let thumb = div()
                .size(px(48.0))
                .flex_none()
                .rounded(px(density.r_card))
                .overflow_hidden()
                .border_1()
                .border_color(theme.border_input)
                .child(
                    img(ImageSource::Image(p.render.clone()))
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                );
            let remove = div()
                .id(("chat-attach-remove", idx))
                .absolute()
                .top(px(-6.0))
                .right(px(-6.0))
                .size(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.bg_base)
                .border_1()
                .border_color(theme.border_input)
                .text_color(theme.fg_muted)
                .text_size(px(self.typography.t_sub_label))
                .cursor_pointer()
                .hover(|s| s.text_color(theme.fg_base))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, _w, cx| this.remove_image(idx, cx)),
                )
                .child(SharedString::from("✕"));
            row = row.child(div().relative().flex_none().child(thumb).child(remove));
        }
        row
    }

    /// Staged context chips shown above the input pill: one pill per captured
    /// attachment (`@diff · 128 lines`, `@terminal build · 42 lines`,
    /// `@clipboard · 3 lines`), each with a ✕ to drop it. The label carries the
    /// line count so the user sees the prompt cost of what they attached. Rendered
    /// only when something is staged.
    fn render_context_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .w_full()
            .gap(px(density.gap_inline));
        for (idx, chip) in self.context_chips.iter().enumerate() {
            let pill = div()
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .gap(px(density.gap_inline * 0.5))
                .rounded(px(density.r_card))
                .border_1()
                .border_color(theme.border_input)
                .bg(theme.bg_panel)
                .px(px(density.gap_inline))
                .py(px(density.gap_inline * 0.5))
                .child(
                    div()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(chip.label())),
                )
                .child(
                    div()
                        .id(("chat-context-remove", idx))
                        .size(px(14.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.fg_subtle)
                        .text_size(px(typo.t_sub_label))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.fg_base))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| this.remove_context_chip(idx, cx)),
                        )
                        .child(SharedString::from("✕")),
                );
            row = row.child(pill);
        }
        row
    }

    /// Parked-message chips shown above the input while a turn streams: one
    /// compact row per queued message with a truncated preview and a ✕ to cancel
    /// it, so the user can see (and drop) what will send next. Rendered only when
    /// something is queued.
    fn render_queued(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(density.gap_inline * 0.5));
        // ✎ (pop-to-edit) is offered only when the draft is empty — editing a
        // chip replaces the composer contents, so it must not clobber a draft
        // in progress (matches the ↑-to-edit guard).
        let draft_empty = self.draft_is_empty(cx);
        let turn_active = self.turn_active;
        for (idx, m) in self.queued.iter().enumerate() {
            // Whether ↑ on THIS chip would reach the running turn (see
            // `send_queued_now`) — decided per chip, since an attachment keeps a
            // message out of the steer path even on a backend that steers.
            let steers = turn_active && self.can_steer && m.images.is_empty();
            // Prefer the caption; fall back to an image count for an image-only
            // queued message so its chip isn't blank.
            let preview = if !m.text.is_empty() {
                m.text.clone()
            } else {
                format!("{} image{}", m.images.len(), if m.images.len() == 1 { "" } else { "s" })
            };
            let row = div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .gap(px(density.gap_inline))
                .rounded(px(density.r_lg))
                .border_1()
                .border_color(theme.border_input)
                .bg(theme.bg_panel)
                .px(px(density.pad_panel))
                .py(px(density.gap_inline))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from("Queued")),
                )
                // Preview clips to one line — wrapped in this flex_row so a long
                // prompt truncates instead of rendering blank (the flex-col trap).
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(preview)),
                )
                // "Send now" (↑). Idle: dequeue + send immediately. Mid-turn on a
                // steering backend: hand it to the running turn — which is a real
                // action at every index, front one included. Mid-turn otherwise:
                // jump it to the front of the queue, so it's hidden at index 0
                // where that would do nothing.
                .when(!turn_active || steers || idx != 0, |row| {
                    row.child(
                        div()
                            .id(("chat-queued-send-now", idx))
                            .flex_none()
                            .size(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.fg_subtle)
                            .text_size(px(typo.t_body_sm))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.fg_base))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| this.send_queued_now(idx, cx)),
                            )
                            .child(SharedString::from("↑")),
                    )
                })
                .when(draft_empty, |row| {
                    row.child(
                        div()
                            .id(("chat-queued-edit", idx))
                            .flex_none()
                            .size(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.fg_subtle)
                            .text_size(px(typo.t_body_sm))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.fg_base))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.edit_queued(idx, window, cx)
                                }),
                            )
                            .child(SharedString::from("✎")),
                    )
                })
                .child(
                    div()
                        .id(("chat-queued-cancel", idx))
                        .flex_none()
                        .size(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_color(theme.fg_subtle)
                        .text_size(px(typo.t_body_sm))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.fg_base))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| this.cancel_queued(idx, cx)),
                        )
                        .child(SharedString::from("✕")),
                );
            col = col.child(row);
        }
        col
    }

    /// A muted "History i/N" strip shown above the pill while walking prompt
    /// history — echoing a shell's recall indicator so ↑/↓ reads as browsing
    /// sent prompts, not moving the caret. `None` when not navigating.
    fn render_history_indicator(&self) -> Option<impl IntoElement> {
        let (pos, total) = self.history.position()?;
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .px(px(density.pad_panel))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(format!("History {pos}/{total}"))),
                ),
        )
    }

    /// The command list shown while the palette is open: an inline panel above
    /// the pill (grows upward over the transcript) with rows grouped under
    /// "Built-in" / "Skills" headers, each row a `/name`, its description, and a
    /// muted source tag. Rows are windowed around the highlight so a large list
    /// stays a fixed height and cheap per keystroke.
    fn render_slash_palette(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let p = self.palette.as_ref()?;
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        const VISIBLE: usize = 8;
        // Comfortable reading column for the hover tooltip: a fixed width so long
        // descriptions wrap onto several short lines instead of one wide line.
        const SLASH_TOOLTIP_WIDTH: f32 = 340.0;
        let total = p.matches.len();
        let start = if p.highlight < VISIBLE {
            0
        } else {
            (p.highlight + 1 - VISIBLE).min(total.saturating_sub(VISIBLE))
        };
        let end = (start + VISIBLE).min(total);
        let mut list = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(density.r_xl))
            .border_1()
            .border_color(theme.border_input)
            .bg(theme.bg_panel)
            .py(px(density.pad_row));
        let mut prev_group: Option<CommandGroup> = None;
        for (row, &cmd) in p.matches.iter().enumerate().skip(start).take(end - start) {
            let Some(name) = self.slash_commands.get(cmd) else { continue };
            let meta = self.slash_catalog.get(name);
            let group = meta.map(|m| m.group).unwrap_or(CommandGroup::BuiltIn);
            // A group header when the section changes (also at the window top).
            if prev_group != Some(group) {
                prev_group = Some(group);
                list = list.child(
                    div()
                        .px(px(density.pad_panel))
                        .pt(px(density.gap_inline))
                        .pb(px(density.gap_inline * 0.5))
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(group.label())),
                );
            }
            let selected = row == p.highlight;
            let name_el = div()
                .flex_none()
                .text_color(theme.fg_base)
                .child(SharedString::from(format!("/{name}")));
            let mut inner = div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .gap(px(density.gap_inline * 1.5))
                .child(name_el);
            // Description (muted) fills the middle when present, clipped to a
            // SINGLE line with an ellipsis so a long description never wraps and
            // pushes the row taller (which broke the panel's uniform row height).
            // The backend's own advertised description (ACP) wins over the on-disk
            // catalog — the agent knows its commands best.
            let desc = self
                .slash_descriptions
                .get(name)
                .map(String::as_str)
                .or_else(|| meta.and_then(|m| m.description.as_deref()));
            if let Some(desc) = desc {
                inner = inner.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(desc.to_string())),
                );
            } else {
                inner = inner.child(div().flex_1());
            }
            // Source tag (plugin name) on the far right.
            if let Some(src) = meta.and_then(|m| m.source_label.as_deref()) {
                inner = inner.child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(src.to_string())),
                );
            }
            let mut item = div()
                .id(("slash-row", row))
                .w_full()
                .px(px(density.pad_panel))
                .py(px(density.gap_inline))
                .text_size(px(typo.t_body_md))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, window, cx| {
                        this.palette_accept_match(row, window, cx)
                    }),
                )
                .child(inner);
            // Hover tooltip with the full description, so a row whose description
            // is clipped on screen can still be read in full (Claude-Desktop-style).
            // The tooltip content is rendered inside a fixed-width block so a long
            // description wraps onto several short lines (a comfortable reading
            // column) — the library tooltip is a flex row and would otherwise let
            // one-line text run off-screen (a flex row won't shrink a single text
            // child below its unwrapped width, so a plain `max_w` is ignored).
            if let Some(desc) = meta.and_then(|m| m.description.clone()) {
                item = item.tooltip(move |window, cx| {
                    let desc = desc.clone();
                    gpui_component::tooltip::Tooltip::element(move |_, _| {
                        div()
                            .w(px(SLASH_TOOLTIP_WIDTH))
                            .child(SharedString::from(desc.clone()))
                    })
                    .build(window, cx)
                });
            }
            if selected {
                // The keyboard highlight: a calm full-width accent tint.
                item = item.bg(theme.status_info.opacity(0.16));
            }
            list = list.child(item);
        }
        Some(list)
    }

    /// The `@file` mention overlay: an inline block above the pill listing the
    /// ranked file paths (or a scanning/no-match hint). Same visual language as
    /// the slash palette; mutually exclusive with it, so both can be rendered
    /// unconditionally — at most one is ever `Some`. The ranker caps at
    /// [`MAX_SUGGESTIONS`], so the list never needs windowing.
    fn render_mention_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let m = self.mention.as_ref()?;
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let mut list = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(density.r_xl))
            .border_1()
            .border_color(theme.border_input)
            .bg(theme.bg_panel)
            .py(px(density.pad_row));
        // A muted section label (Context / Files), inserted whenever the ranked
        // list crosses from one group to the other.
        let header = |label: &'static str| {
            div()
                .px(px(density.pad_panel))
                .pt(px(density.gap_inline))
                .pb(px(density.gap_inline * 0.5))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(label))
        };
        if m.matches.is_empty() {
            // A pending `@` with nothing to show yet: distinguish the async scan
            // still running from a query that matched no provider or file.
            let msg = if self.mention_candidates_loaded {
                "No matches"
            } else {
                "Scanning files…"
            };
            list = list.child(header("Files")).child(
                div()
                    .px(px(density.pad_panel))
                    .py(px(density.gap_inline))
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(msg)),
            );
            return Some(list);
        }
        let mut prev_was_context: Option<bool> = None;
        for (row, mm) in m.matches.iter().enumerate() {
            let is_context = matches!(mm, MentionMatch::Context(_));
            // Section header at each group boundary (Context rows sort first).
            if prev_was_context != Some(is_context) {
                list = list.child(header(if is_context { "Context" } else { "Files" }));
                prev_was_context = Some(is_context);
            }
            let label: String = match mm {
                MentionMatch::Context(i) => {
                    self.context_sources.get(*i).map(|s| s.label.clone()).unwrap_or_default()
                }
                MentionMatch::File(path) => path.clone(),
            };
            let selected = row == m.highlight;
            // Wrap the label in a flex_row so a long entry clips to one line —
            // a nowrap/truncate text placed DIRECTLY in a flex-col renders blank.
            let inner = div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .child(div().flex_1().min_w_0().truncate().child(SharedString::from(label)));
            let mut item = div()
                .id(("mention-row", row))
                .w_full()
                .px(px(density.pad_panel))
                .py(px(density.gap_inline))
                .text_size(px(typo.t_body_md))
                .text_color(theme.fg_base)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, window, cx| this.mention_accept(row, window, cx)),
                )
                .child(inner);
            if selected {
                item = item.bg(theme.status_info.opacity(0.16));
            }
            list = list.child(item);
        }
        Some(list)
    }

    /// The `(command name, argument hint)` to surface in the gap between
    /// accepting a slash command and typing its arguments, or `None` when no hint
    /// applies. `Some` only when the composer can send, the palette is closed,
    /// and the draft is a completed bare command that advertises an
    /// `argument-hint`; hidden the moment an argument is typed. Kept apart from
    /// the rendering so it is unit-testable without a laid-out view.
    fn usage_hint(&self, cx: &Context<Self>) -> Option<(String, String)> {
        if self.disconnected || self.turn_active || self.palette.is_some() {
            return None;
        }
        let draft = self.input.read(cx).value();
        let name = completed_command(draft.as_ref())?;
        // The backend's own advertised hint (ACP `AvailableCommand.input`) wins
        // over the on-disk catalog's `argument-hint`, mirroring the palette
        // description precedence — the agent knows its commands best.
        let hint = self
            .slash_hints
            .get(name)
            .cloned()
            .or_else(|| self.slash_catalog.get(name).and_then(|m| m.argument_hint.clone()))?;
        Some((name.to_string(), hint))
    }

    /// The argument-hint strip shown in the gap between accepting a slash command
    /// and typing its arguments: a muted one-line usage cue (`/name  arg-hint`)
    /// sitting where the palette was. The library input can't render true inline
    /// ghost text through a public API, so this strip stands in for it — same
    /// slot above the pill, same muted tone. Mutually exclusive with the palette
    /// (which the command's trailing space closes); see [`Self::usage_hint`] for
    /// when it applies.
    fn render_usage_hint(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let (name, hint) = self.usage_hint(cx)?;
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        Some(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .rounded(px(density.r_xl))
                .border_1()
                .border_color(theme.border_input)
                .bg(theme.bg_panel)
                .px(px(density.pad_panel))
                .py(px(density.gap_inline))
                .gap(px(density.gap_inline * 1.5))
                .text_size(px(typo.t_body_sm))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(format!("/{name}"))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(hint)),
                ),
        )
    }
}

impl Render for ComposerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // Flush any finished dictation transcript (insert at cursor) and pending
        // toast here — this is the first place with a `Window` after an event.
        self.apply_pending_dictation(window, cx);

        // Keep the input placeholder in step with the active agent: an unbound
        // draft follows the picked agent; once bound it follows the connected
        // provider. `InputState::set_placeholder` needs the `Window` we only have
        // here, so apply it lazily and only when the derived text changes (this
        // runs before any `&self` borrow below).
        let desired_label = if self.unbound {
            self.current_agent
                .as_ref()
                .map(|(_, d)| d.clone())
                .unwrap_or_else(|| "Agent".to_string())
        } else {
            self.provider_label.clone()
        };
        let desired_placeholder = format!("Message {desired_label}…  (↵ send · ⇧↵ newline)");
        if self.applied_placeholder.as_deref() != Some(desired_placeholder.as_str()) {
            self.input.update(cx, |s, cx| s.set_placeholder(desired_placeholder.clone(), window, cx));
            self.applied_placeholder = Some(desired_placeholder);
        }

        // Re-seed the searchable model dropdown when the advertised set changes and
        // keep its selection in step with the current model. Both need the `Window`
        // only available here; the signature guards make this a no-op most paints.
        let model_sig: Vec<(String, String, Option<String>)> = self
            .vocab
            .models
            .iter()
            .map(|m| (m.wire.clone(), m.label.clone(), m.description.clone()))
            .collect();
        if self.model_select_sig != model_sig {
            self.model_select_sig = model_sig.clone();
            let items: Vec<ModelItem> = model_sig
                .iter()
                .map(|(w, l, d)| ModelItem { wire: w.clone(), label: l.clone(), description: d.clone() })
                .collect();
            self.model_select.update(cx, |s, cx| s.set_items(SearchableVec::new(items), window, cx));
            self.model_select_current = None; // re-apply selection against the new list
        }
        let current_model = self.model.clone().or_else(|| self.vocab.default_model.clone());
        if self.model_select_current != current_model {
            self.model_select_current = current_model.clone();
            if let Some(wire) = current_model {
                self.model_select.update(cx, |s, cx| s.set_selected_value(&wire, window, cx));
            }
        }

        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let can_send = !self.disconnected;
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);

        // No status line here: the composer keeps a FIXED footprint so sending
        // (turn start/end) never resizes it. Live turn/disconnect state is shown
        // in the transcript instead — the way a native chat surfaces it —
        // leaving the composer a calm, stable pill.

        // Circular action button pinned to the bottom-right of the pill. While a
        // turn streams it becomes a Stop (■) that interrupts it; otherwise it's
        // the ↑ Send. A mouse target is the primary affordance because keyboard
        // ↵ can be swallowed by some input methods (e.g. Vietnamese Telex eats
        // Enter before the app sees it).
        let action_button = if self.turn_active {
            // Stop: always live during a turn, in a muted attention tone.
            div()
                .id("agent-chat-stop")
                .size(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.fg_base)
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _window, cx| this.request_stop(cx)),
                )
                // A drawn square, not the "■" glyph: the glyph's own metrics
                // decide its size and baseline, so it rendered as a small,
                // optically-high rectangle. A div gives an exact, centered
                // square with soft corners — the native chat Stop look.
                .child(div().size(px(10.0)).rounded(px(density.r_chip)).bg(theme.bg_base))
        } else {
            let (send_bg, send_fg) = if can_send {
                (theme.status_info, theme.bg_base)
            } else {
                (theme.bg_panel_alt, theme.fg_subtle)
            };
            div()
                .id("agent-chat-send")
                .size(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(send_bg)
                .text_color(send_fg)
                .when(can_send, |s| s.cursor_pointer().hover(|s| s.opacity(0.85)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, window, cx| this.submit(window, cx)),
                )
                // An SVG arrow, not the "↑" glyph: a text arrow centers by
                // line-box metrics and reads high in the filled circle. The
                // icon's geometry centers cleanly under `items_center`; the
                // arrowhead's visual mass still sits a hair above its midpoint,
                // so a 1px downward nudge optically centers it.
                .child(
                    div().relative().top(px(1.0)).child(
                        Icon::default()
                            .path("icons/arrow-up.svg")
                            .size(px(15.0))
                            .text_color(send_fg),
                    ),
                )
        };

        // The controls row that sits BELOW the input pill (on the composer
        // background), mirroring Claude Desktop: the image-attach + permission-mode
        // on the far LEFT, a `flex_1` spacer, then the model/effort pickers on the
        // far RIGHT. Send/Stop is NOT here — it lives inside the input pill.
        let mut controls = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .px(px(density.pad_row))
            .gap(px(density.gap_inline));
        // Paperclip/image attach anchors the far left, before the safety mode.
        // (New-chat + terminal-view moved to the tab header's view-options menu.)
        controls = controls.child(self.render_attach_button(cx));
        // Voice dictation mic (+ device menu), next to the paperclip. Hidden when
        // dictation is disabled, and while recording — the recording bar then
        // overlays the input field itself (ChatGPT-style), carrying its own
        // stop/send, so a toolbar mic would be redundant.
        if Self::dictation_settings(cx).enabled && !self.dictation.is_active() {
            controls = controls.child(self.render_mic_button(cx));
        }
        if self.supports_modes {
            controls = controls.child(self.render_permission_picker(cx));
        }
        // Spacer pushes the agent/model/effort cluster to the far right.
        controls = controls.child(div().flex_1());
        // The live context meter leads the far-right cluster (hidden until any
        // usage has arrived this session).
        if let Some(meter) = self.render_context_meter() {
            controls = controls.child(meter);
        }
        // Unbound *New Agent* draft: the agent picker precedes the model picker so
        // the user chooses which agent, then which model, before the first send
        // binds a subprocess. Hidden once bound (transport is fixed at spawn).
        if self.unbound && !self.agent_options.is_empty() {
            controls = controls.child(self.render_agent_picker(cx));
        }
        // Show the model picker only when the backend advertises models (like the
        // mode/effort pickers). A vocab-less/disconnected state hides it rather
        // than rendering a blank control.
        if !self.vocab.models.is_empty() {
            controls = controls.child(self.render_model_select(cx));
        }
        // Show the effort picker only when the backend actually advertises effort
        // levels — mirroring the model gate above. Gating on `supports_effort`
        // alone leaked a blank picker: that flag tracks the generic "accepts
        // config" capability, so an agent advertising some *other* config option
        // (with no thought-level) rendered an effort button with an empty label.
        if self.supports_effort && !self.vocab.efforts.is_empty() {
            controls = controls.child(self.render_effort_picker(cx));
        }
        // Thinking visibility (a transcript view preference) sits beside the
        // effort picker so "how hard it thinks" and "whether you see it" read
        // together. Hidden until the parent pushes a state — i.e. until the
        // transcript actually holds a thinking block.
        if self.thinking_display.is_some() {
            controls = controls.child(self.render_thinking_picker(cx));
        }
        // Generic backend-advertised feature controls (fast / plan / auto-accept
        // / agent-profile …) close the far-right cluster. Renders nothing when the
        // backend advertises no features, so providers without any stay unchanged.
        if !self.vocab.features.is_empty() {
            controls = controls.child(self.render_feature_controls(cx));
        }
        let controls = controls;

        // The pill: a rounded, focus-reactive frame holding the borderless input
        // AND the Send/Stop action at its right edge (like a native chat field).
        // The input takes the remaining width (`flex_1`); the circular action is
        // vertically CENTERED (`items_center`) so on a one-line draft it sits on
        // the text's midline instead of dropping to the bottom edge. As the field
        // grows it stays centered on the taller box (the common chat-composer
        // look). `appearance(false)` drops the input's own box so it doesn't nest
        // a second frame inside. The other controls (attach, mode, model, effort)
        // live on the row below.
        //
        // Height: NO explicit `.h()` — the `auto_grow(1, MAX_COMPOSER_ROWS)` input
        // sizes itself to its content, growing one line per WRAPPED row (not just
        // per hard newline) and capping at MAX_COMPOSER_ROWS before it scrolls.
        // An earlier hand-rolled `.h()` counted only `\n`s, so a long soft-wrapped
        // draft under-measured and spilled its text over the controls below.
        let pill = div()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .rounded(px(density.r_xl))
            .border_1()
            .border_color(if focused { theme.focus_ring } else { theme.border_input })
            .bg(theme.bg_panel_alt)
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .gap(px(density.gap_inline))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .text_size(px(typo.t_body_md)),
                ),
            )
            .child(action_button)
            // While recording, the ChatGPT-style recording bar overlays the input
            // field (waveform + timer + stop + send). The input stays mounted
            // underneath so its focus + the outer Escape/Enter capture-actions keep
            // working; the opaque overlay just hides the draft text meanwhile.
            .when(self.dictation.is_active(), |d| {
                d.child(self.render_recording_bar(cx))
            });

        div()
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .border_t_1()
            .border_color(theme.border_inactive)
            .p(px(density.pad_panel))
            // Intercept ⌘V before the text field: if the clipboard holds an
            // image, stage it and swallow the paste; otherwise let it fall
            // through so text pastes normally. Capture phase (this ancestor runs
            // before the focused input) is what lets us pre-empt it.
            .capture_action(cx.listener(|this, _: &Paste, _window, cx| {
                if this.try_paste_image(cx) {
                    cx.stop_propagation();
                }
            }))
            // While an overlay (slash palette or `@file` mention) is open, capture
            // the field's nav/accept/close actions before the input consumes them;
            // when both are closed these are no-ops that fall through to the input's
            // normal handling. The slash palette is tried first (it takes
            // precedence). Enter is captured at the view root (it also drives
            // submit) — see `on_enter_key`.
            // ↑/↓ first drive an open overlay; with none open, ↑ pulls a parked
            // message back to edit (empty draft + queued), then recalls prompt
            // history (shell-style) when the caret allows, else falls through to
            // the field's normal per-line caret movement.
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if this.palette_move(-1, cx)
                    || this.mention_move(-1, cx)
                    || this.edit_last_queued(window, cx)
                    || this.history_older(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if this.palette_move(1, cx)
                    || this.mention_move(1, cx)
                    || this.history_newer(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
                // A live recording is cancelled first (before any overlay close),
                // so Escape always discards the mic session when one is active.
                if this.dictation_escape(cx)
                    || this.palette_close(cx)
                    || this.mention_close(cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this.palette_accept_highlighted(window, cx)
                    || this.mention_accept_highlighted(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            // Cmd+E toggles dictation for this composer when it's the focused
            // surface. Global-scoped action dispatched from the focused element
            // up its ancestors; the composer is deeper than the workspace root,
            // so it fires first and stops propagation, keeping the root-level
            // handler (terminal/editor dictation) from also firing.
            .on_action(cx.listener(|this, _: &ToggleDictation, window, cx| {
                cx.stop_propagation();
                this.toggle_dictation(window, cx);
            }))
            .child(
                // Match the transcript's centered reading column so the pill +
                // controls line up with the messages above on wide windows. The
                // attachment chips (if any) sit above the input box, its controls
                // on a row below (Claude Desktop layout).
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(super::CONTENT_MAX_W))
                    .gap(px(density.gap_inline))
                    .when(!self.pending_images.is_empty(), |d| {
                        d.child(self.render_attachments(cx))
                    })
                    .when(!self.context_chips.is_empty(), |d| {
                        d.child(self.render_context_chips(cx))
                    })
                    .children(self.render_slash_palette(cx))
                    .children(self.render_mention_overlay(cx))
                    .children(self.render_usage_hint(cx))
                    .when(!self.queued.is_empty(), |d| d.child(self.render_queued(cx)))
                    .children(self.render_history_indicator())
                    // New Agent draft: where-this-runs + Import session sit above
                    // the input pill (reference-cockpit placement), not in the
                    // toolbar below — see `render_context_row`.
                    .when(self.unbound, |d| d.child(self.render_context_row(cx)))
                    .child(pill)
                    .child(controls),
            )
    }
}

#[cfg(test)]
impl ComposerView {
    /// The composer's OWN `unbound` flag — distinct from the parent view's, and
    /// pushed down via [`Self::set_agent_picker`]. The agent picker, the
    /// Import-session row and the placeholder's agent name all read this one, so
    /// a test asserting they survive a parent-side sync must read it here rather
    /// than the parent's.
    pub(crate) fn unbound_for_test(&self) -> bool {
        self.unbound
    }

    /// How many agents the picker currently offers — cleared to zero by a
    /// bound-chat sync, which is the other half of the same push.
    pub(crate) fn agent_options_len_for_test(&self) -> usize {
        self.agent_options.len()
    }

    /// Whether the worktree pill is currently offered. Pushed by the parent, and
    /// cleared by a bound sync — a live session's cwd can't change.
    pub(crate) fn worktree_draft_is_some_for_test(&self) -> bool {
        self.worktree_draft.is_some()
    }

    /// How many models the picker currently offers. Gated independently of
    /// `unbound` (the picker reads `!vocab.models.is_empty()`), so a test
    /// asserting only the two above would miss a regression that blanks the
    /// model list alone — which is exactly what pushing a connection-less
    /// draft's empty caps-derived vocab does.
    pub(crate) fn vocab_models_len_for_test(&self) -> usize {
        self.vocab.models.len()
    }

    /// Set the draft text so a `#[gpui::test]` can exercise submit / newline /
    /// palette routing without synthesising keystrokes. Parks the caret at the
    /// end (as if the text were typed) so trigger detection sees the token.
    pub(crate) fn set_draft_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_draft_end(text.to_string(), window, cx);
    }

    /// Read the current draft (to assert it survived a newline or was cleared by
    /// submit).
    pub(crate) fn draft_for_test(&self, cx: &Context<Self>) -> String {
        self.input.read(cx).value().to_string()
    }

    /// The caret's byte offset — to assert an accepted command parks it after the
    /// inserted token rather than jumping to the start of the box.
    pub(crate) fn cursor_for_test(&self, cx: &Context<Self>) -> usize {
        self.input.read(cx).cursor()
    }

    /// Recompute the overlays from the current draft (as an edit's `Change` would)
    /// so a test can open the palette before accepting a command.
    pub(crate) fn recompute_overlays_for_test(&mut self, cx: &mut Context<Self>) {
        self.recompute_overlays(cx);
    }

    /// Accept the highlighted palette command (as Tab/Enter would).
    pub(crate) fn accept_highlighted_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.palette_accept_highlighted(window, cx)
    }

    /// The `(command, argument-hint)` the usage strip would show, or `None`.
    pub(crate) fn usage_hint_for_test(&self, cx: &Context<Self>) -> Option<(String, String)> {
        self.usage_hint(cx)
    }

    /// Drive ↑ history recall (as the MoveUp capture would); returns whether it
    /// consumed the key.
    pub(crate) fn history_older_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.history_older(window, cx)
    }

    /// Drive ↓ history recall (as the MoveDown capture would).
    pub(crate) fn history_newer_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.history_newer(window, cx)
    }

    /// How many messages are currently parked (queued while a turn streamed).
    pub(crate) fn queued_len_for_test(&self) -> usize {
        self.queued.len()
    }

    /// Drive ↑ "edit the last queued message"; returns whether it consumed the key.
    pub(crate) fn edit_last_queued_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.edit_last_queued(window, cx)
    }

    /// Stage a context chip directly (as the parent's capture would), so a test
    /// can exercise serialization / clear-on-send without a live provider.
    pub(crate) fn stage_context_chip_for_test(&mut self, chip: ContextChip, cx: &mut Context<Self>) {
        self.add_context_chip(chip, cx);
    }

    /// How many context chips are currently staged.
    pub(crate) fn context_chips_len_for_test(&self) -> usize {
        self.context_chips.len()
    }

    /// The palette enrichment currently in effect (descriptions + grouping).
    pub(crate) fn slash_catalog_for_test(&self) -> &CommandCatalog {
        &self.slash_catalog
    }

    /// Stage an attachment directly (as the picker/paste path would), so a test
    /// can exercise the with-images branches without decoding a real image.
    pub(crate) fn stage_image_for_test(&mut self, chat: ChatImage) {
        let render = image_attach::decode_render(&chat);
        self.pending_images.push(PendingImage {
            chat,
            render: render.expect("a test image must decode"),
        });
    }
}


/// Append `@path ` tokens to `draft`, inserting a separating space when the
/// draft doesn't already end in whitespace so a drop can't fuse onto the last
/// word the user typed (`fix this@src/main.rs`). Each token carries its own
/// trailing space, so consecutive drops stay separated and the caret lands
/// ready for the next word.
fn with_mentions_appended(draft: &str, paths: &[String]) -> String {
    let mut next = draft.to_string();
    for path in paths {
        if !next.is_empty() && !next.ends_with(char::is_whitespace) {
            next.push(' ');
        }
        next.push('@');
        next.push_str(path);
        next.push(' ');
    }
    next
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_never_fuses_onto_the_last_typed_word() {
        // The failure this guards: dropping a file after "fix this" producing
        // `fix this@src/main.rs`, which is neither a mention nor prose.
        assert_eq!(
            with_mentions_appended("fix this", &["src/main.rs".into()]),
            "fix this @src/main.rs "
        );
        // A draft that already ends in whitespace gets no second space.
        assert_eq!(
            with_mentions_appended("fix this ", &["src/main.rs".into()]),
            "fix this @src/main.rs "
        );
        assert_eq!(
            with_mentions_appended("fix this\n", &["src/main.rs".into()]),
            "fix this\n@src/main.rs "
        );
    }

    #[test]
    fn an_empty_draft_takes_no_leading_space() {
        assert_eq!(with_mentions_appended("", &["a.rs".into()]), "@a.rs ");
    }

    #[test]
    fn several_dropped_files_stay_separated() {
        // A multi-file drop is one gesture; the tokens must not run together.
        assert_eq!(
            with_mentions_appended("", &["a.rs".into(), "b/c.rs".into()]),
            "@a.rs @b/c.rs "
        );
    }

    #[test]
    fn no_paths_leaves_the_draft_untouched() {
        assert_eq!(with_mentions_appended("keep me", &[]), "keep me");
    }

    use gpui::TestAppContext;

    #[test]
    fn short_model_label_strips_provider_namespace() {
        // The toolbar trigger drops any `provider/` prefix; the full label is kept
        // for the dropdown row + search.
        assert_eq!(short_model_label("openai/gpt-5.5"), "gpt-5.5");
        assert_eq!(short_model_label("opencode/big-pickle"), "big-pickle");
        // No namespace → unchanged; only the last segment survives nesting.
        assert_eq!(short_model_label("Sonnet"), "Sonnet");
        assert_eq!(short_model_label("a/b/c"), "c");
    }

    #[test]
    fn model_item_search_matches_name_or_description() {
        let opus = ModelItem {
            wire: "opus".into(),
            label: "Opus".into(),
            description: Some("Most capable — deep reasoning & hard tasks".into()),
        };
        // Name match (case-insensitive).
        assert!(opus.matches("opus"));
        assert!(opus.matches("OP"));
        // Description match: a capability query finds it even though "reasoning"
        // isn't in the name.
        assert!(opus.matches("reasoning"));
        assert!(opus.matches("capable"));
        // Neither → no match.
        assert!(!opus.matches("haiku"));

        // A model without a blurb only matches on its name.
        let bare = ModelItem { wire: "o3".into(), label: "o3".into(), description: None };
        assert!(bare.matches("o3"));
        assert!(!bare.matches("reasoning"));
    }

    /// Build a bare composer in a test window (no parent needed).
    fn test_composer(cx: &mut TestAppContext) -> gpui::WindowHandle<ComposerView> {
        cx.update(gpui_component::init);
        let w = cx.add_window(|window, cx| {
            ComposerView::new(
                Theme::default(),
                Density::default(),
                Typography::default(),
                "Claude",
                window,
                cx,
            )
        });
        cx.run_until_parked();
        w
    }

    /// ↑ recalls previously-sent prompts newest-first and stops at the oldest;
    /// ↓ walks back forward and restores the live draft past the newest.
    #[gpui::test]
    async fn up_down_arrow_recall_prompt_history(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.seed_history(vec!["alpha".into(), "beta".into()]);
                // Empty draft, caret on the first line → ↑ recalls the newest.
                assert!(c.history_older_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "beta");
                assert!(c.history_older_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "alpha");
                // At the oldest: stays put but still consumes the key.
                assert!(c.history_older_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "alpha");
                // ↓ walks forward, then restores the (empty) live draft and stops
                // consuming once out of history.
                assert!(c.history_newer_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "beta");
                assert!(c.history_newer_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "");
                assert!(!c.history_newer_for_test(window, cx), "not navigating → fall through");
            })
            .expect("window update");
    }

    /// With no history, ↑ is not consumed (so the caret can still move).
    #[gpui::test]
    async fn up_arrow_falls_through_with_empty_history(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                assert!(!c.history_older_for_test(window, cx));
            })
            .expect("window update");
    }

    /// Submitting while a turn streams parks the message (the queue branch in
    /// `submit` returns before it can emit a Submit) and clears the draft; the
    /// parked message is then handed back on drain, emptying the queue.
    #[gpui::test]
    async fn submit_during_turn_queues_instead_of_sending(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                // Simulate a streaming turn.
                c.set_state(false, true, cx);
                c.set_draft_for_test("queued one", window, cx);
                c.submit(window, cx);
                assert_eq!(c.queued_len_for_test(), 1, "message parked, not sent");
                assert!(c.draft_for_test(cx).is_empty(), "draft cleared on queue");
                // Drain hands the parked message back and empties the queue.
                let next = c.take_next_queued(cx);
                assert_eq!(next.map(|(t, _)| t), Some("queued one".to_string()));
                assert_eq!(c.queued_len_for_test(), 0);
            })
            .expect("window update");
    }

    /// "Send now" while a turn streams moves a queued message to the FRONT (next
    /// to auto-drain) without sending; it's a no-op at index 0.
    #[gpui::test]
    async fn send_queued_now_moves_to_front_mid_turn(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx); // streaming turn
                for t in ["one", "two", "three"] {
                    c.set_draft_for_test(t, window, cx);
                    c.submit(window, cx);
                }
                assert_eq!(c.queued_texts(), vec!["one", "two", "three"]);
                // Jump the third to the front.
                c.send_queued_now(2, cx);
                assert_eq!(c.queued_texts(), vec!["three", "one", "two"], "moved to front, none sent");
                // No-op at index 0.
                c.send_queued_now(0, cx);
                assert_eq!(c.queued_texts(), vec!["three", "one", "two"], "index 0 is a no-op");
            })
            .expect("window update");
    }

    /// On a backend that takes a mid-turn message, "send now" means now: the chip
    /// leaves the queue for the running turn instead of shuffling within it.
    #[gpui::test]
    async fn send_queued_now_steers_mid_turn_when_the_backend_takes_it(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx); // streaming turn
                c.set_can_steer(true, cx);
                for t in ["one", "two"] {
                    c.set_draft_for_test(t, window, cx);
                    c.submit(window, cx);
                }
                // Even at index 0 — the case that is a no-op without steering —
                // the message goes out rather than being re-parked at the front.
                c.send_queued_now(0, cx);
                assert_eq!(c.queued_texts(), vec!["two"], "the steered message left the queue");
                // It counts as sent, so ↑ recall offers it.
                c.send_queued_now(0, cx);
                assert_eq!(c.queued_texts(), Vec::<String>::new());
            })
            .expect("window update");
    }

    /// A queued message carrying an image never steers — pi's `steer` accepts
    /// images but OxiMux has never sent one, and reordering keeps the attachment
    /// rather than quietly dropping it.
    #[gpui::test]
    async fn a_queued_message_with_an_image_reorders_instead_of_steering(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx);
                c.set_can_steer(true, cx);
                c.set_draft_for_test("text only", window, cx);
                c.submit(window, cx);
                c.stage_image_for_test(ChatImage {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                });
                c.set_draft_for_test("has an image", window, cx);
                c.submit(window, cx);
                c.send_queued_now(1, cx);
                assert_eq!(
                    c.queued_texts(),
                    vec!["has an image", "text only"],
                    "moved to the front, still queued — its image is intact"
                );
            })
            .expect("window update");
    }

    /// End-to-end for Design Mode: an element picked in the embedded browser is
    /// staged exactly as `AgentChatView::stage_browser_pick` stages it, then sent.
    ///
    /// This is the seam a unit test cannot reach and a webview is not needed for:
    /// it proves the capture survives chip staging, image decode, and submit, and
    /// that both halves reach the wire together.
    #[gpui::test]
    async fn a_picked_browser_element_reaches_the_wire_with_its_screenshot(
        cx: &mut TestAppContext,
    ) {
        // A 1x1 red PNG — the smallest thing `pending_from_bytes` will accept.
        use base64::Engine as _;
        let png: Vec<u8> = base64::engine::general_purpose::STANDARD
            .decode(concat!(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8",
                "z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
            ))
            .expect("valid base64 fixture");

        let window = test_composer(cx);
        let sent = std::rc::Rc::new(std::cell::RefCell::new(None));
        let seen = sent.clone();
        cx.update(|cx| {
            let root = window.root(cx).expect("root view");
            cx.subscribe(
                &root,
                move |_, ev: &ComposerEvent, _| {
                    if let ComposerEvent::Submit { text, images } = ev {
                        *seen.borrow_mut() = Some((text.clone(), images.clone()));
                    }
                },
            )
            .detach();
        });

        window
            .update(cx, |c, window, cx| {
                // Exactly what `stage_browser_pick` does with a pick.
                let chip = crate::shell::agent_chat::context_providers::browser_chip(
                    "a#go",
                    "Selected element: <a id=\"go\">x</a>\ncolor: rgb(0, 0, 0)".to_string(),
                )
                .expect("a non-empty capture makes a chip");
                assert!(chip.label().starts_with("@browser a#go · "));
                c.add_context_chip(chip, cx);
                let staged = image_attach::pending_from_bytes(png, None)
                    .expect("a 1x1 PNG decodes");
                c.add_pending_images(vec![staged], cx);

                // The capture is staged, not sent — the user still types the ask.
                assert_eq!(c.context_chips_len_for_test(), 1);
                assert_eq!(c.current_images().len(), 1);

                c.set_draft_for_test("why is this misaligned?", window, cx);
                c.submit(window, cx);
            })
            .expect("window update");
        cx.run_until_parked();

        let (text, images) = sent.borrow_mut().take().expect("a submit was emitted");
        // The element rides as a tagged context block naming its selector...
        assert!(
            text.starts_with("<context name=\"browser\" source=\"a#go\">\n"),
            "wire text was: {text}"
        );
        assert!(text.contains("<a id=\"go\">x</a>"), "the HTML must survive");
        assert!(text.contains("color: rgb(0, 0, 0)"), "the computed CSS must survive");
        // ...and the user's own question is still the tail of the message.
        assert!(text.ends_with("why is this misaligned?"));
        // ...with the crop attached, not left on the clipboard.
        assert_eq!(images.len(), 1, "the screenshot must travel with the text");
        assert_eq!(images[0].media_type, "image/png");
        assert!(!images[0].data.is_empty());
    }

    /// Restored queued chips (`seed_queued`) re-render without auto-sending; a
    /// seeded draft respects the no-clobber guard.
    #[gpui::test]
    async fn seed_queue_and_draft_restore_without_clobber(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                // Restored queue: chips appear, nothing sent (no active turn).
                c.seed_queued(vec!["a".into(), "b".into(), "  ".into()], cx);
                assert_eq!(c.queued_texts(), vec!["a", "b"], "blank entry skipped, none sent");
                // seed_draft into an empty composer applies.
                c.seed_draft("restored draft".into(), window, cx);
                assert_eq!(c.draft_for_test(cx), "restored draft");
                // seed_draft into a NON-empty composer is ignored (no clobber).
                c.seed_draft("should be ignored".into(), window, cx);
                assert_eq!(c.draft_for_test(cx), "restored draft", "in-progress text preserved");
            })
            .expect("window update");
    }

    /// ↑ with an empty draft while a message is parked pulls it back into the
    /// composer to edit (removing it from the queue); with a non-empty draft it
    /// does not clobber the in-progress text.
    #[gpui::test]
    async fn up_arrow_edits_the_last_queued_message(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx); // streaming turn
                c.set_draft_for_test("park me", window, cx);
                c.submit(window, cx);
                assert_eq!(c.queued_len_for_test(), 1);
                assert!(c.draft_for_test(cx).is_empty());
                // Empty draft + a queued message → ↑ pulls it back for editing.
                assert!(c.edit_last_queued_for_test(window, cx));
                assert_eq!(c.draft_for_test(cx), "park me");
                assert_eq!(c.queued_len_for_test(), 0, "pulled out of the queue");
                // With a draft present now, ↑ must not pull (nothing queued anyway).
                assert!(!c.edit_last_queued_for_test(window, cx));
            })
            .expect("window update");
    }

    /// A staged context chip serializes into the drained message as a `<context>`
    /// block prepended to the typed text — and a chip alone (no caption) is enough
    /// to send (the empty-guard allows an attachment-only prompt).
    #[gpui::test]
    async fn context_chip_serializes_into_wire_on_drain(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx); // streaming → submit parks it
                c.stage_context_chip_for_test(
                    ContextChip::new(
                        oximux_agents::thread::ContextKind::Diff,
                        None,
                        "diff --git a b".into(),
                        false,
                    ),
                    cx,
                );
                c.set_draft_for_test("what changed?", window, cx);
                c.submit(window, cx);
                assert_eq!(c.queued_len_for_test(), 1, "parked with its chip");
                assert_eq!(c.context_chips_len_for_test(), 0, "chip drained off staging");
                let (wire, _) = c.take_next_queued(cx).expect("one queued");
                assert!(wire.starts_with("<context name=\"diff\">"), "block prepended: {wire}");
                assert!(wire.ends_with("what changed?"), "typed text preserved: {wire}");
            })
            .expect("window update");
    }

    /// Pulling a queued message back to edit restores its context chips (they
    /// re-serialize on the next send), mirroring image restore.
    #[gpui::test]
    async fn queued_context_chip_restored_on_edit(cx: &mut TestAppContext) {
        let window = test_composer(cx);
        window
            .update(cx, |c, window, cx| {
                c.set_state(false, true, cx);
                c.stage_context_chip_for_test(
                    ContextChip::new(
                        oximux_agents::thread::ContextKind::Clipboard,
                        None,
                        "pasted".into(),
                        false,
                    ),
                    cx,
                );
                c.submit(window, cx);
                assert_eq!(c.queued_len_for_test(), 1);
                assert_eq!(c.context_chips_len_for_test(), 0);
                assert!(c.edit_last_queued_for_test(window, cx));
                assert_eq!(c.context_chips_len_for_test(), 1, "chip restored on edit");
                assert_eq!(c.queued_len_for_test(), 0);
            })
            .expect("window update");
    }

    #[test]
    fn rank_context_sources_prefix_then_substring_then_all() {
        let sources = vec![
            ContextSource::diff(),                                    // key "diff"
            ContextSource::clipboard(),                              // key "clipboard"
            ContextSource::terminal(oximux_pty::TerminalSessionId(1), "diffbuild"), // key "terminal diffbuild"
        ];
        // Empty query → all sources in order.
        assert_eq!(rank_context_sources(&sources, ""), vec![0, 1, 2]);
        // "diff" prefixes source 0, is a substring of source 2 → prefix first.
        assert_eq!(rank_context_sources(&sources, "diff"), vec![0, 2]);
        // "clip" prefixes only clipboard.
        assert_eq!(rank_context_sources(&sources, "clip"), vec![1]);
        // No match.
        assert!(rank_context_sources(&sources, "zzz").is_empty());
    }
}
