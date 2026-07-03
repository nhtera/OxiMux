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
use oximux_agents::thread::ChatImage;
use oximux_settings::{Density, Theme, Typography};

use super::image_attach::{PendingImage, pending_from_bytes, pending_from_path};
use super::slash_command_catalog::{CommandCatalog, CommandGroup};
use super::slash_palette::{detect_slash_trigger, rank_commands};
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

/// Open state of the `@file` mention overlay: the byte range of the `@query` it
/// replaces on accept, the ranked candidate paths, and which is highlighted
/// (index into `matches`). Mutually exclusive with [`SlashPaletteState`] — a
/// leading `/` opens the slash palette, which takes precedence.
struct MentionState {
    range: Range<usize>,
    matches: Vec<String>,
    highlight: usize,
}

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
    /// The user picked a model in the bottom toolbar (a Claude alias).
    ModelPicked(String),
    /// The user picked a permission mode in the bottom toolbar (a wire value).
    PermissionModePicked(String),
    /// The user picked a reasoning-effort level in the bottom toolbar.
    EffortPicked(String),
}

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
    /// The open `@file` mention overlay, or `None` when the caret isn't inside an
    /// `@query`. Mutually exclusive with `palette`.
    mention: Option<MentionState>,
    /// Repaints this view (only) on each keystroke so the draft stays visible.
    _sub: Subscription,
}

impl EventEmitter<ComposerEvent> for ComposerView {}

impl ComposerView {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            // SINGLE-LINE field: it self-sizes to one centered row at a stable
            // height, so the pill never resizes when a message is sent. A
            // multi-line / `auto_grow` field lays its element out at height:100%
            // of the parent (a circular height in this custom pill) and
            // top-aligns its content — after Enter clears the draft the caret
            // drops to the bottom and the pill stretches into dead space. Enter
            // still submits: the parent root `capture_action(InputEnter)`
            // intercepts the field's Enter action (see `AgentChatView::render`).
            // Long drafts scroll horizontally; multi-line-grow is future work and
            // must first solve the circular-height embedding.
            InputState::new(window, cx).placeholder("Message Claude…  (↵ to send)")
        });
        let sub = cx.subscribe(&input, |this, _input, ev: &InputEvent, cx| {
            // Repaint ONLY the composer on edits — the transcript is untouched.
            // Focus/Blur repaint too so the pill's border can track focus (a
            // brighter ring while typing), like a native chat field.
            match ev {
                InputEvent::Change => {
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
            pending_images: Vec::new(),
            slash_commands: Vec::new(),
            slash_catalog: CommandCatalog::new(),
            palette: None,
            mention_candidates: Vec::new(),
            mention_candidates_loaded: false,
            mention: None,
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
        cx: &mut Context<Self>,
    ) {
        if self.model != model
            || self.permission_mode != permission_mode
            || self.effort != effort
            || self.supports_modes != supports_modes
            || self.supports_effort != supports_effort
        {
            self.model = model;
            self.permission_mode = permission_mode;
            self.effort = effort;
            self.supports_modes = supports_modes;
            self.supports_effort = supports_effort;
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
        self.recompute_slash_palette(cx);
        if self.palette.is_some() {
            self.mention = None;
        } else {
            self.recompute_mention(cx);
        }
    }

    /// Recompute the `@file` overlay from the draft + caret. Opens whenever the
    /// caret sits inside an `@query` and the composer can send; shows a scanning /
    /// no-match hint until the candidate list ranks something. Preserves the
    /// highlighted path across a re-filter when it still matches.
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
        let matches = rank_mentions(&self.mention_candidates, &pm.query, MAX_SUGGESTIONS);
        // Keep the same path highlighted across a re-filter (highlight-by-value),
        // else fall back to the top match.
        let prev = self.mention.as_ref().and_then(|m| m.matches.get(m.highlight).cloned());
        let highlight = prev
            .and_then(|p| matches.iter().position(|m| *m == p))
            .unwrap_or(0);
        self.mention = Some(MentionState { range: pm.range, matches, highlight });
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

    /// Accept a ranked path: replace the `@query` with `@path ` (trailing space),
    /// ready to keep typing. The path rides to the agent as ordinary text.
    fn mention_accept_path(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.mention.take() else { return };
        if let Some(path) = m.matches.get(idx).cloned() {
            let text = self.input.read(cx).value().to_string();
            if m.range.end <= text.len()
                && text.is_char_boundary(m.range.start)
                && text.is_char_boundary(m.range.end)
            {
                let next = format!("{}@{path} {}", &text[..m.range.start], &text[m.range.end..]);
                self.input.update(cx, |s, cx| s.set_value(next, window, cx));
            }
        }
        cx.notify();
    }

    /// Accept the highlighted path (keyboard Enter/Tab). Returns `false` when the
    /// overlay is closed OR shows no real match, so Enter still submits / Tab
    /// still indents rather than being swallowed by an empty hint.
    fn mention_accept_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(m) = self.mention.as_ref() else { return false };
        if m.matches.is_empty() {
            return false;
        }
        let idx = m.highlight;
        self.mention_accept_path(idx, window, cx);
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
    /// as ordinary user text on submit. (The single-line field's `set_value`
    /// puts the caret at the end of the draft — after the space in the common
    /// case where the command is the whole draft; if the user accepted a
    /// mid-line command with text after it, the caret lands after that trailing
    /// text instead. Acceptable — commands are near-always typed at the start.)
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
                self.input.update(cx, |s, cx| s.set_value(next, window, cx));
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

    /// The root Enter handler delegates here: accept a highlighted slash command
    /// or `@file` mention if an overlay is open, otherwise submit the message.
    pub fn on_enter_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_accept_highlighted(window, cx) || self.mention_accept_highlighted(window, cx)
        {
            return;
        }
        self.submit(window, cx);
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

    /// Read + clear the draft, emitting [`ComposerEvent::Submit`] when it's a
    /// non-empty message and the agent is available. Inert while a turn is
    /// streaming: the primary affordance is Stop then, and a new message can't
    /// be sent until the turn ends (or is stopped).
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.disconnected || self.turn_active {
            return;
        }
        let text = self.input.read(cx).value().to_string();
        let text = text.trim().to_string();
        // An image-only prompt (attachments, no caption) is valid; only bail when
        // there's nothing at all to send.
        if text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        let images: Vec<ChatImage> =
            self.pending_images.drain(..).map(|p| p.chat).collect();
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.emit(ComposerEvent::Submit { text, images });
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
        let current = self
            .model
            .clone()
            .unwrap_or_else(|| super::CLAUDE_MODELS[1].to_string());
        let current_for_menu = current.clone();
        Button::new("chat-model-btn")
            .label(current)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for m in super::CLAUDE_MODELS {
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
        let current_wire = self
            .permission_mode
            .clone()
            .unwrap_or_else(|| super::DEFAULT_PERMISSION_MODE.to_string());
        let current_label = super::CLAUDE_PERMISSION_MODES
            .iter()
            .find(|(w, _)| *w == current_wire)
            .map(|(_, l)| *l)
            .unwrap_or(current_wire.as_str())
            .to_string();
        let current_for_menu = current_wire.clone();
        Button::new("chat-perm-mode-btn")
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for (wire, label) in super::CLAUDE_PERMISSION_MODES {
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
        let current_wire = self
            .effort
            .clone()
            .unwrap_or_else(|| super::DEFAULT_EFFORT.to_string());
        let current_label = super::CLAUDE_EFFORTS
            .iter()
            .find(|(w, _)| *w == current_wire)
            .map(|(_, l)| *l)
            .unwrap_or(current_wire.as_str())
            .to_string();
        let current_for_menu = current_wire.clone();
        Button::new("chat-effort-btn")
            .label(current_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, window, _cx| {
                for (wire, label) in super::CLAUDE_EFFORTS {
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
            .py(px(density.pad_row))
            .child(
                div()
                    .px(px(density.pad_panel))
                    .pt(px(density.gap_inline))
                    .pb(px(density.gap_inline * 0.5))
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("Files")),
            );
        if m.matches.is_empty() {
            // A pending `@` with nothing to show yet: distinguish the async scan
            // still running from a query that matched no file.
            let msg = if self.mention_candidates_loaded {
                "No matching files"
            } else {
                "Scanning files…"
            };
            list = list.child(
                div()
                    .px(px(density.pad_panel))
                    .py(px(density.gap_inline))
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(msg)),
            );
            return Some(list);
        }
        for (row, path) in m.matches.iter().enumerate() {
            let selected = row == m.highlight;
            // Wrap the path in a flex_row so a long path can clip to one line —
            // a nowrap/truncate text placed DIRECTLY in a flex-col renders blank.
            let inner = div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(path.clone())),
                );
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
                    cx.listener(move |this, _e, window, cx| {
                        this.mention_accept_path(row, window, cx)
                    }),
                )
                .child(inner);
            if selected {
                item = item.bg(theme.status_info.opacity(0.16));
            }
            list = list.child(item);
        }
        Some(list)
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
                .text_size(px(typo.t_body_md))
                .when(can_send, |s| s.cursor_pointer().hover(|s| s.opacity(0.85)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, window, cx| this.submit(window, cx)),
                )
                .child(SharedString::from("↑"))
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
        if self.supports_modes {
            controls = controls.child(self.render_permission_picker(cx));
        }
        // Spacer pushes the model/effort cluster to the far right.
        controls = controls.child(div().flex_1()).child(self.render_model_picker(cx));
        if self.supports_effort {
            controls = controls.child(self.render_effort_picker(cx));
        }
        let controls = controls;

        // The pill: a rounded, focus-reactive frame holding the borderless input
        // AND the Send/Stop action pinned to its right edge (like a native chat
        // field). The input takes the remaining width (`flex_1`); the circular
        // action sits after it. `appearance(false)` drops the input's own box so
        // it doesn't nest a second frame inside. The other controls (attach, mode,
        // model, effort) live on the row below.
        let pill = div()
            .flex()
            .flex_row()
            .items_center()
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
            .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
                if this.palette_move(-1, cx) || this.mention_move(-1, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
                if this.palette_move(1, cx) || this.mention_move(1, cx) {
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
                    .children(self.render_slash_palette(cx))
                    .children(self.render_mention_overlay(cx))
                    .child(pill)
                    .child(controls),
            )
    }
}
