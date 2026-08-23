//! The global "Listening…" HUD — voice dictation for any focused text pane.
//!
//! Dictation is no longer chat-only. When ⌘E is pressed with a terminal or code
//! editor focused, the workspace root resolves a [`HudSink`] for that pane and
//! hands it to this entity, which owns the recording state machine, renders a
//! floating pill (a ChatGPT-style waveform + timer, mounted screen-anchored at
//! the workspace root like the toast layer), and inserts the finished transcript
//! into the sink. The chat composer keeps its own in-line recording bar; this
//! HUD covers everything else.
//!
//! It mirrors the composer's dictation state machine deliberately: same
//! [`DictationUiState`], same ticker, same stash-transcript-then-apply-on-render
//! idiom (the event drain has no `Window`; `render` does).

use std::time::Instant;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Styled,
    WeakEntity, Window, div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::input::InputState;
use oximux_dictation::DictationEvent;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::terminal_view::TerminalView;
use crate::ui::FloatingSurface;

use super::dictation_service::{self, DictationTarget, StartDecision};
use super::dictation_ui::{DictationUiState, WaveformBuffer, dictation_spacing, format_elapsed};
use super::dictation_waveform::{WaveformStyle, render_waveform};

/// Where the HUD delivers a finished transcript — the concrete non-composer text
/// pane that was focused when recording began. Held as a weak handle so a closed
/// tab drops it (the HUD cancels the session when its sink dies mid-record).
#[derive(Clone)]
pub enum HudSink {
    Terminal(WeakEntity<TerminalView>),
    Editor(WeakEntity<InputState>),
}

impl HudSink {
    fn alive(&self) -> bool {
        match self {
            HudSink::Terminal(w) => w.upgrade().is_some(),
            HudSink::Editor(w) => w.upgrade().is_some(),
        }
    }
}

/// The global dictation HUD entity. One per workspace window; mounted as a
/// pass-through overlay at the root so it floats above every pane.
pub struct DictationHud {
    state: DictationUiState,
    waveform: WaveformBuffer,
    /// The pane receiving this session's transcript. Kept through `Final` so
    /// `apply_pending` can insert; cleared on insert / cancel / error.
    sink: Option<HudSink>,
    /// Transcript stashed from the (window-less) event drain, inserted on the
    /// next `render` (which owns the `Window`).
    pending: Option<String>,
    pending_toast: Option<String>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl DictationHud {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            state: DictationUiState::Idle,
            waveform: WaveformBuffer::default(),
            sink: None,
            pending: None,
            pending_toast: None,
            theme,
            density,
            typography,
        }
    }

    /// Refresh theme tokens each render (same push-down doctrine as the toast
    /// layer). Store-only, no notify.
    pub fn set_tokens(&mut self, theme: Theme, density: Density, typography: Typography) {
        self.theme = theme;
        self.density = density;
        self.typography = typography;
    }

    /// ⌘E over a terminal/editor pane: stop a live session, else run the shared
    /// pre-flight checks and begin recording into `sink`.
    pub fn toggle(&mut self, sink: HudSink, window: &mut Window, cx: &mut Context<Self>) {
        if dictation_service::is_active(cx) {
            // One session at a time; ⌘E anywhere stops the current one.
            dictation_service::stop(cx);
            return;
        }
        match dictation_service::prepare_start(cx, window) {
            StartDecision::Ready { paths, device } => self.begin(sink, paths, device, cx),
            StartDecision::NeedsPermission { paths, device } => {
                let (tx, rx) = futures::channel::oneshot::channel::<bool>();
                crate::mic_permission::request(move |granted| {
                    let _ = tx.send(granted);
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(true) = rx.await {
                        let _ = this.update(cx, |this, cx| this.begin(sink, paths, device, cx));
                    }
                })
                .detach();
            }
            StartDecision::Blocked => {}
        }
    }

    fn begin(
        &mut self,
        sink: HudSink,
        paths: oximux_dictation::ModelPaths,
        device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.sink = Some(sink);
        self.state = DictationUiState::Starting;
        self.waveform.clear();
        self.pending = None;
        let weak = cx.entity().downgrade();
        if !dictation_service::start(cx, DictationTarget::Hud(weak), paths, device) {
            self.reset();
            self.pending_toast = Some("Dictation is busy".into());
        }
        cx.notify();
        self.spawn_ticker(cx);
    }

    /// Repaint ~2×/sec while recording so the mm:ss timer advances even in total
    /// silence (level events drive the waveform). Self-terminates when idle.
    fn spawn_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if this.state.is_active() {
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

    /// Fold a dictation event into the HUD state. Called by the service drain on
    /// the main thread; no `Window`, so a final transcript is stashed for render.
    pub fn on_dictation_event(&mut self, ev: DictationEvent, cx: &mut Context<Self>) {
        match ev {
            DictationEvent::Started => {
                self.state = DictationUiState::Recording {
                    started_at: Instant::now(),
                };
            }
            DictationEvent::Level(level) => {
                // The target pane closed mid-record — cancel so the mic doesn't
                // stay hot (mirrors the composer orphan guard; ~10 Hz self-heal).
                if self.sink.as_ref().map(|s| !s.alive()).unwrap_or(true) {
                    dictation_service::cancel(cx);
                    self.reset();
                    cx.notify();
                    return;
                }
                self.waveform.push(level);
            }
            DictationEvent::Transcribing => self.state = DictationUiState::Transcribing,
            DictationEvent::Capped => {
                self.pending_toast = Some("Recording stopped at the 2-minute limit".into());
            }
            DictationEvent::Cancelled => self.reset(),
            DictationEvent::Final(text) => {
                // Keep the sink until `apply_pending` inserts; only stash text.
                self.state = DictationUiState::Idle;
                self.waveform.clear();
                if !text.trim().is_empty() {
                    self.pending = Some(text);
                } else {
                    self.sink = None;
                }
            }
            DictationEvent::Error(msg) => {
                self.reset();
                self.pending_toast = Some(format!("Dictation error: {msg}"));
            }
        }
        cx.notify();
    }

    fn reset(&mut self) {
        self.state = DictationUiState::Idle;
        self.waveform.clear();
        self.sink = None;
        self.pending = None;
    }

    /// Insert the stashed transcript into the sink and flush any toast. Called at
    /// the top of `render` (which owns the `Window`). Terminal insertion needs no
    /// window; editor insertion does — hence the deferral to render.
    fn apply_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(msg) = self.pending_toast.take() {
            window.push_notification(msg, cx);
        }
        let Some(text) = self.pending.take() else {
            return;
        };
        let trailing = dictation_service::append_trailing_space(cx);
        match self.sink.take() {
            Some(HudSink::Terminal(weak)) => {
                if let Some(term) = weak.upgrade() {
                    // Empty `before_cursor` deliberately suppresses the leading
                    // space: at a shell prompt a leading space is significant
                    // (`HISTCONTROL=ignorespace` drops the command from history).
                    // The trailing space still applies.
                    let insert = dictation_spacing("", &text, trailing);
                    term.update(cx, |view, cx| view.insert_dictation_text(&insert, cx));
                }
            }
            Some(HudSink::Editor(weak)) => {
                if let Some(state) = weak.upgrade() {
                    let before = state.read(cx).value().to_string();
                    let insert = dictation_spacing(&before, &text, trailing);
                    if !insert.is_empty() {
                        state.update(cx, |st, cx| st.insert(insert, window, cx));
                    }
                }
            }
            None => {}
        }
    }

    /// The floating pill's label + whether the waveform shows.
    fn pill_label(&self) -> (String, bool) {
        match &self.state {
            DictationUiState::Recording { started_at, .. } => (format_elapsed(*started_at), true),
            DictationUiState::Starting => ("Starting…".to_string(), false),
            DictationUiState::Transcribing => ("Transcribing…".to_string(), false),
            _ => (String::new(), false),
        }
    }
}

impl Render for DictationHud {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // Insert any finished transcript first (window in hand).
        self.apply_pending(window, cx);

        // Nothing to show unless a session is live.
        if !self.state.is_active() {
            return div();
        }

        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let (label, recording) = self.pill_label();
        let bars = self.waveform.filled_bars(22.0, 0.05);

        let stop_square = div()
            .size(px(10.0))
            .rounded(px(density.r_chip))
            .bg(theme.status_error);

        let pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(density.h_overlay_item))
            .px(px(12.0))
            .floating_chrome(&theme, &density)
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_base)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _ev, _window, cx| dictation_service::stop(cx)),
            )
            .child(stop_square)
            .child(div().min_w(px(30.0)).child(label))
            .when(recording, |d| {
                // The pill stays compact, so clip the full-width strip to a
                // fixed window showing the newest bars (older ones scroll off
                // the left) rather than letting 64 bars stretch the pill.
                d.child(
                    div()
                        .w(px(132.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .overflow_hidden()
                        .child(render_waveform(
                            &bars,
                            WaveformStyle {
                                height: 18.0,
                                bar_w: 2.5,
                                gap: 2.0,
                                color: theme.status_error,
                                fill: false,
                            },
                        )),
                )
                .child(
                    div()
                        .text_size(px(typo.t_sub_label))
                        .text_color(theme.fg_muted)
                        .child("Esc"),
                )
            });

        // Pass-through overlay anchored bottom-center, clearing the status bar.
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .justify_end()
            .items_center()
            .pb(px(density.h_status_bar + 16.0))
            .child(pill)
    }
}
