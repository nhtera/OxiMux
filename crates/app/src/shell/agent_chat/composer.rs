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
    Anchor, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, SharedString, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants, DropdownButton};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenuItem;
use oximux_settings::{Density, Theme, Typography};

/// Raised by the composer for the parent [`super::AgentChatView`] to act on.
/// The parent performs the actual send / interrupt / model+mode switch; on
/// `Submit` the composer has already cleared its input by the time the event
/// fires.
pub enum ComposerEvent {
    Submit(String),
    /// The user pressed Stop while a turn was streaming — interrupt it.
    Stop,
    /// The user picked a model in the bottom toolbar (a Claude alias).
    ModelPicked(String),
    /// The user picked a permission mode in the bottom toolbar (a wire value).
    PermissionModePicked(String),
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
    /// Whether the backend honors a permission-mode switch (hides the mode picker
    /// when it doesn't). Model is always offered.
    supports_modes: bool,
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
            supports_modes: false,
            _sub: sub,
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

    /// Mirror the parent's session controls (current model + permission mode, and
    /// whether the backend supports mode switching) so the bottom toolbar renders
    /// the right labels. Only repaints when something actually changed.
    pub fn set_controls(
        &mut self,
        model: Option<String>,
        permission_mode: Option<String>,
        supports_modes: bool,
        cx: &mut Context<Self>,
    ) {
        if self.model != model
            || self.permission_mode != permission_mode
            || self.supports_modes != supports_modes
        {
            self.model = model;
            self.permission_mode = permission_mode;
            self.supports_modes = supports_modes;
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
        if text.is_empty() {
            return;
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.emit(ComposerEvent::Submit(text));
    }

    /// Ask the parent to interrupt the in-flight turn (the Stop button). Leaves
    /// the draft untouched so the user can send it once the turn is stopped.
    fn request_stop(&mut self, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::Stop);
    }

    /// The model dropdown in the bottom toolbar: a small ghost button labeled
    /// with the current model, opening the Claude aliases upward (the composer
    /// sits at the bottom, so the menu anchors to the button's bottom-right).
    fn render_model_picker(&self, cx: &mut Context<Self>) -> DropdownButton {
        let entity = cx.entity();
        let current = self
            .model
            .clone()
            .unwrap_or_else(|| super::CLAUDE_MODELS[1].to_string());
        let current_for_menu = current.clone();
        DropdownButton::new("chat-model")
            .button(Button::new("chat-model-btn").label(current).small().ghost())
            .small()
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

    /// The permission-mode dropdown in the bottom toolbar: a small ghost button
    /// labeled with the current mode, opening the canonical mode menu upward.
    fn render_permission_picker(&self, cx: &mut Context<Self>) -> DropdownButton {
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
        DropdownButton::new("chat-perm-mode")
            .button(Button::new("chat-perm-mode-btn").label(current_label).small().ghost())
            .small()
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

        // The bottom toolbar (inside the pill, below the input), mirroring Claude
        // Desktop's composer: the permission-mode picker on the far LEFT, then a
        // `flex_1` spacer, then the model picker and the Send/Stop action on the
        // far RIGHT. The model sits next to Send (the two most-used controls
        // grouped) while the mode — a safety setting — anchors the opposite edge.
        let mut toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(density.gap_inline));
        if self.supports_modes {
            toolbar = toolbar.child(self.render_permission_picker(cx));
        }
        let toolbar = toolbar
            .child(div().flex_1())
            .child(self.render_model_picker(cx))
            .child(action_button);

        // The pill: a rounded, focus-reactive frame stacking the borderless input
        // over the toolbar (a column). The single-line input self-sizes to one
        // row and the toolbar is a fixed-height row, so the pill keeps a stable
        // footprint that never resizes when a message is sent. `appearance(false)`
        // drops the input's own box so it doesn't nest a second frame inside.
        let pill = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .w_full()
            .rounded(px(14.0))
            .border_1()
            .border_color(if focused { theme.focus_ring } else { theme.border_input })
            .bg(theme.bg_panel_alt)
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .child(
                div().w_full().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .text_size(px(typo.t_body_md)),
                ),
            )
            .child(toolbar);

        div()
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .border_t_1()
            .border_color(theme.border_inactive)
            .p(px(density.pad_panel))
            .child(
                // Match the transcript's centered reading column so the pill
                // lines up with the messages above it on wide windows.
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(super::CONTENT_MAX_W))
                    .child(pill),
            )
    }
}
