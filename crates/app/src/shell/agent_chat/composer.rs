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

use gpui::{
    Anchor, App, AppContext, ClipboardEntry, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ImageSource, InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement, Render,
    SharedString, Styled, Subscription, Window, div, img, prelude::FluentBuilder, px,
};
use gpui::StyledImage as _;
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Paste};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use oximux_agents::thread::ChatImage;
use oximux_settings::{Density, Theme, Typography};

use super::image_attach::{PendingImage, pending_from_bytes, pending_from_path};

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
        let sub = cx.subscribe(&input, |_this, _input, ev: &InputEvent, cx| {
            // Repaint ONLY the composer on edits — the transcript is untouched.
            // Focus/Blur repaint too so the pill's border can track focus (a
            // brighter ring while typing), like a native chat field.
            if matches!(ev, InputEvent::Change | InputEvent::Focus | InputEvent::Blur) {
                cx.notify();
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

        // The controls row that sits BELOW the input box (outside the rounded
        // pill, on the composer background), mirroring Claude Desktop's composer:
        // the permission-mode picker on the far LEFT, then a `flex_1` spacer, then
        // the model picker and the Send/Stop action on the far RIGHT. The model
        // sits next to Send (the two most-used controls grouped) while the mode —
        // a safety setting — anchors the opposite edge.
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
        // Spacer pushes the model/effort/Send cluster to the far right.
        controls = controls.child(div().flex_1()).child(self.render_model_picker(cx));
        if self.supports_effort {
            controls = controls.child(self.render_effort_picker(cx));
        }
        let controls = controls.child(action_button);

        // The pill: a rounded, focus-reactive frame around the borderless input
        // ONLY. The single-line input self-sizes to one row, so the pill keeps a
        // stable, compact footprint that never resizes when a message is sent.
        // `appearance(false)` drops the input's own box so it doesn't nest a
        // second frame inside. The controls live below it, not within.
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
            .child(
                Input::new(&self.input)
                    .appearance(false)
                    .text_size(px(typo.t_body_md)),
            );

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
                    .child(pill)
                    .child(controls),
            )
    }
}
