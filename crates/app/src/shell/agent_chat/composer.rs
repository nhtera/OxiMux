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

use gpui::{
    Anchor, App, AppContext, ClipboardEntry, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ImageSource, InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, img,
    prelude::FluentBuilder, px,
};
use gpui::StyledImage as _;
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{
    IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp, Paste, Escape as InputEscape,
};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use oximux_agents::thread::{
    prepend_context, ChatImage, ContextChip, EffortChoice, ModeChoice, ModelChoice,
};

/// The model/mode/effort options plus their "current when unset" defaults,
/// sourced from the live [`oximux_agents::thread::AgentConnection`] and pushed
/// into the composer so the bottom-toolbar pickers render whatever the backend
/// advertises — no hardcoded provider vocabulary lives in the view.
#[derive(Clone, Default, PartialEq)]
pub struct ControlVocab {
    pub models: Vec<ModelChoice>,
    pub permission_modes: Vec<ModeChoice>,
    pub efforts: Vec<EffortChoice>,
    pub default_model: Option<String>,
    pub default_mode: Option<String>,
    pub default_effort: Option<String>,
}
use oximux_settings::{Density, Theme, Typography};

use super::composer_history::PromptHistory;
use super::context_providers::{ContextRequest, ContextSource};
use super::image_attach::{self, PendingImage, pending_from_bytes, pending_from_path};
use super::slash_command_catalog::{CommandCatalog, CommandGroup};
use super::slash_palette::{completed_command, detect_slash_trigger, rank_commands};
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
    /// The user pressed Stop while a turn was streaming — interrupt it.
    Stop,
    /// The user asked to start a fresh conversation in this tab ("New chat").
    /// The parent blanks the transcript and respawns a non-resumed session.
    NewChat,
    /// The user picked a model in the bottom toolbar (a Claude alias).
    ModelPicked(String),
    /// The user picked a permission mode in the bottom toolbar (a wire value).
    PermissionModePicked(String),
    /// The user picked a reasoning-effort level in the bottom toolbar.
    EffortPicked(String),
    /// The `@` overlay just opened. The parent refreshes the composer's context
    /// sources (esp. the live sibling-terminal list) in response, so the "Context"
    /// section is current without the composer reaching into the pane group.
    MentionOpened,
    /// The user picked a context provider in the `@` overlay. The parent captures
    /// the content (clipboard / git diff / terminal scrollback) and hands the
    /// resulting chip back via [`ComposerView::add_context_chip`].
    CaptureContext(ContextRequest),
}

/// The composer's `auto_grow` input grows one row per WRAPPED line of the draft
/// up to this many rows, then holds that height and scrolls internally — so a
/// long message never pushes the transcript off-screen. Tuned to Claude
/// Desktop's feel: generous but bounded.
const MAX_COMPOSER_ROWS: usize = 10;

pub struct ComposerView {
    input: Entity<InputState>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Mirrors the parent's connection state, for the status line + Send button.
    disconnected: bool,
    turn_active: bool,
    /// Mirrors of the parent's session controls, for the bottom toolbar pickers.
    /// The parent owns the truth (it respawns on a change) and pushes updates via
    /// [`Self::set_controls`]; the composer only renders them and emits a pick.
    model: Option<String>,
    permission_mode: Option<String>,
    effort: Option<String>,
    /// Whether the backend honors a permission-mode switch (hides the mode picker
    /// when it doesn't). Model is always offered.
    supports_modes: bool,
    /// Whether the backend accepts a reasoning-effort setting (hides the effort
    /// picker when it doesn't).
    supports_effort: bool,
    /// The model/mode/effort options the live backend advertises, pushed in via
    /// [`Self::set_controls`]. The pickers render from this (no hardcoded
    /// provider vocab); empty until a connection is attached.
    vocab: ControlVocab,
    /// Images staged for the next send (via the paperclip, ⌘V, or drag-drop).
    /// Each holds both its wire/persist [`ChatImage`] and a pre-decoded thumbnail
    /// so the chip row doesn't re-decode on every keystroke repaint. Cleared on
    /// submit.
    pending_images: Vec<PendingImage>,
    /// Command names the backend advertised at session init (from `SessionInit`),
    /// offered in the slash-command palette. Empty when the backend advertises
    /// none — which also disables the palette.
    slash_commands: Vec<String>,
    /// On-disk enrichment (description, group, source) for the advertised names,
    /// discovered off the main thread and pushed in when ready. Empty until then;
    /// names without an entry render bare under Built-in.
    slash_catalog: CommandCatalog,
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
        Self {
            input,
            theme,
            density,
            typography,
            disconnected: false,
            turn_active: false,
            model: None,
            permission_mode: None,
            effort: None,
            supports_modes: false,
            supports_effort: false,
            vocab: ControlVocab::default(),
            pending_images: Vec::new(),
            slash_commands: Vec::new(),
            slash_catalog: CommandCatalog::new(),
            palette: None,
            mention_candidates: Vec::new(),
            mention_candidates_loaded: false,
            mention: None,
            context_sources: Vec::new(),
            context_chips: Vec::new(),
            history: PromptHistory::new(),
            queued: Vec::new(),
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

    /// Push the backend's advertised slash-command names (from `SessionInit`).
    /// A non-empty list enables the palette; an empty one disables it. Recomputes
    /// in case the caret already sits in a `/token` when the list arrives.
    pub fn set_slash_commands(&mut self, commands: Vec<String>, cx: &mut Context<Self>) {
        if self.slash_commands != commands {
            self.slash_commands = commands;
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

    fn pick_effort(&mut self, effort: String, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::EffortPicked(effort));
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

    /// The model control in the bottom toolbar: a flat ghost button (no box —
    /// just the label + a subtle caret, like Claude Desktop) that opens the
    /// Claude aliases upward (the composer sits at the bottom, so the menu
    /// anchors to the button's bottom-right).
    fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let models: Vec<String> = self.vocab.models.iter().map(|m| m.wire.clone()).collect();
        let current = self
            .model
            .clone()
            .or_else(|| self.vocab.default_model.clone())
            .unwrap_or_default();
        let current_for_menu = current.clone();
        Button::new("chat-model-btn")
            .label(current)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for m in &models {
                    let selected = current_for_menu == *m;
                    let display = if selected {
                        format!("\u{2713} {m}")
                    } else {
                        format!("   {m}")
                    };
                    let choice = m.to_string();
                    menu = menu.item(
                        PopupMenuItem::element(move |_w, _c| div().child(display.clone())).on_click(
                            window.listener_for(
                                &entity,
                                move |view: &mut ComposerView, _ev: &gpui::ClickEvent, _w, cx| {
                                    view.pick_model(choice.clone(), cx);
                                },
                            ),
                        ),
                    );
                }
                menu
            })
    }

    /// The permission-mode control in the bottom toolbar: a flat ghost button
    /// (label + subtle caret) labeled with the current mode, opening the
    /// canonical mode menu upward.
    fn render_permission_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
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
        let current_for_menu = current_wire.clone();
        Button::new("chat-perm-mode-btn")
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for (wire, label) in &modes {
                    let selected = current_for_menu == *wire;
                    let display = if selected {
                        format!("\u{2713} {label}")
                    } else {
                        format!("   {label}")
                    };
                    let choice = wire.to_string();
                    menu = menu.item(
                        PopupMenuItem::element(move |_w, _c| div().child(display.clone())).on_click(
                            window.listener_for(
                                &entity,
                                move |view: &mut ComposerView, _ev: &gpui::ClickEvent, _w, cx| {
                                    view.pick_permission_mode(choice.clone(), cx);
                                },
                            ),
                        ),
                    );
                }
                menu
            })
    }

    /// The reasoning-effort control in the bottom toolbar: a flat ghost button
    /// (label + subtle caret) labeled with the current effort, opening the level
    /// menu upward.
    fn render_effort_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
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
        let current_for_menu = current_wire.clone();
        Button::new("chat-effort-btn")
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for (wire, label) in &efforts {
                    let selected = current_for_menu == *wire;
                    let display = if selected {
                        format!("\u{2713} {label}")
                    } else {
                        format!("   {label}")
                    };
                    let choice = wire.to_string();
                    menu = menu.item(
                        PopupMenuItem::element(move |_w, _c| div().child(display.clone())).on_click(
                            window.listener_for(
                                &entity,
                                move |view: &mut ComposerView, _ev: &gpui::ClickEvent, _w, cx| {
                                    view.pick_effort(choice.clone(), cx);
                                },
                            ),
                        ),
                    );
                }
                menu
            })
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

    /// "New chat" — blank the transcript and start a fresh session in this tab.
    /// Raises [`ComposerEvent::NewChat`]; the parent does the reset.
    fn render_new_chat_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("chat-new-btn")
            .icon(Icon::default().path("icons/plus.svg"))
            .ghost()
            .small()
            .tooltip("New chat")
            .on_click(cx.listener(|_this, _ev, _window, cx| cx.emit(ComposerEvent::NewChat)))
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
                .rounded(px(8.0))
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
                .text_size(px(9.0))
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
                .rounded(px(8.0))
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
                        .text_size(px(9.0))
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
        for (idx, m) in self.queued.iter().enumerate() {
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
                .rounded(px(10.0))
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
            .rounded(px(12.0))
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
            if let Some(desc) = meta.and_then(|m| m.description.as_deref()) {
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
            .rounded(px(12.0))
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
        let hint = self.slash_catalog.get(name)?.argument_hint.clone()?;
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
                .rounded(px(12.0))
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
                .bg(theme.fg_muted)
                .text_color(theme.bg_base)
                .text_size(px(typo.t_body_sm))
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _window, cx| this.request_stop(cx)),
                )
                .child(SharedString::from("■"))
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
        controls = controls.child(self.render_attach_button(cx));
        controls = controls.child(self.render_new_chat_button(cx));
        if self.supports_modes {
            controls = controls.child(self.render_permission_picker(cx));
        }
        // Spacer pushes the model/effort cluster to the far right.
        controls = controls.child(div().flex_1());
        // Show the model picker only when the backend advertises models (like the
        // mode/effort pickers). A vocab-less/disconnected state hides it rather
        // than rendering a blank button.
        if !self.vocab.models.is_empty() {
            controls = controls.child(self.render_model_picker(cx));
        }
        if self.supports_effort {
            controls = controls.child(self.render_effort_picker(cx));
        }
        let controls = controls;

        // The pill: a rounded, focus-reactive frame holding the borderless input
        // AND the Send/Stop action at its right edge (like a native chat field).
        // The input takes the remaining width (`flex_1`); the circular action is
        // pinned to the BOTTOM-right (`items_end`) so it stays put as the field
        // grows upward. `appearance(false)` drops the input's own box so it
        // doesn't nest a second frame inside. The other controls (attach, mode,
        // model, effort) live on the row below.
        //
        // Height: NO explicit `.h()` — the `auto_grow(1, MAX_COMPOSER_ROWS)` input
        // sizes itself to its content, growing one line per WRAPPED row (not just
        // per hard newline) and capping at MAX_COMPOSER_ROWS before it scrolls.
        // An earlier hand-rolled `.h()` counted only `\n`s, so a long soft-wrapped
        // draft under-measured and spilled its text over the controls below.
        let pill = div()
            .flex()
            .flex_row()
            .items_end()
            .w_full()
            .rounded(px(14.0))
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
            .child(action_button);

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
                if this.palette_close(cx) || this.mention_close(cx) {
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
                    .child(pill)
                    .child(controls),
            )
    }
}

#[cfg(test)]
impl ComposerView {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

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
