//! Session-history picker modal (`⌘⇧H`).
//!
//! A centered overlay — same shape as the command palette — listing past
//! Claude Code and Codex sessions newest-first. Type to fuzzy-filter; `↵`
//! resumes the highlighted session, `⇧↵` forks it. The index is built off
//! the main thread on open (a small head/tail read per log) and the chosen
//! session is relaunched by dispatching [`ResumeAgentSession`], handled in
//! `WorkspaceRoot` so it can resolve the active project's cwd when a Codex
//! entry doesn't record one.
//!
//! All non-GPUI logic — row labels, fuzzy filtering, resume/fork mapping —
//! lives in [`picker`] so it stays unit-testable; this file is the thin view.

pub mod picker;

use gpui::{
    Animation, AnimationExt, App, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render,
    StatefulInteractiveElement, Styled, Task, Window, div, hsla, prelude::FluentBuilder, px,
};
use gpui_component::{Icon, IconName};
use oximux_settings::{Density, Theme, Typography};

use oximux_agents::session_log::{
    now_unix_ms,
    session_index::{SessionEntry, SessionIndex},
};

use crate::actions::ResumeAgentSession;
use crate::shell::session_history::picker::LaunchKind;
use crate::ui::FloatingSurface;

const MODAL_WIDTH: f32 = 620.0;
const MODAL_TOP_OFFSET_PX: f32 = 96.0;
const HEADER_HEIGHT: f32 = 44.0;
const FOOTER_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 46.0;
const LIST_MAX_HEIGHT: f32 = ROW_HEIGHT * 10.0;
const SCRIM_ALPHA: f32 = 0.20;

/// Emitted when the modal closes, so `WorkspaceRoot` can reclaim keyboard
/// focus (the modal grabs focus on open).
pub enum SessionHistoryEvent {
    Closed,
}

pub struct SessionHistoryModal {
    open: bool,
    query: String,
    selected_idx: usize,
    entries: Vec<SessionEntry>,
    loading: bool,
    /// Captured at open time so row ages render consistently for the frame.
    now_ms: i64,
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
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _load_task: None,
        }
    }

    /// Open the modal, focus its query field, and kick off a background scan
    /// of `~/.claude` + `~/.codex` for past sessions.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.query.clear();
        self.selected_idx = 0;
        self.entries.clear();
        self.loading = true;
        self.now_ms = now_unix_ms();
        window.focus(&self.focus_handle, cx);

        // SessionIndex::build is blocking std::fs — run it on the background
        // executor, then publish back on the main thread.
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let entries = executor
                .spawn(async move {
                    match dirs::home_dir() {
                        Some(home) => {
                            SessionIndex::build(&home.join(".claude"), &home.join(".codex"))
                        }
                        None => Vec::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.entries = entries;
                this.loading = false;
                cx.notify();
            });
        });
        self._load_task = Some(task);
        cx.notify();
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
        picker::filter_sessions(&self.query, &self.entries)
    }

    fn move_selection(&mut self, delta: isize, row_count: usize, cx: &mut Context<Self>) {
        if row_count == 0 {
            return;
        }
        self.selected_idx =
            crate::shell::project_picker::wrap_index(self.selected_idx, delta, row_count);
        cx.notify();
    }

    /// Relaunch the session at filtered-list position `list_idx`: dispatch a
    /// resume/fork action and close. No-op when out of range.
    fn launch(
        &mut self,
        list_idx: usize,
        kind: LaunchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.filtered();
        let Some(&entry_idx) = order.get(list_idx) else {
            return;
        };
        let Some(entry) = self.entries.get(entry_idx) else {
            return;
        };
        let action = ResumeAgentSession {
            session_id: entry.session_id.clone(),
            adapter: entry.adapter,
            // Empty when the log omits cwd (Codex index): the handler falls
            // back to the active project's directory.
            cwd: entry.cwd.clone().unwrap_or_default(),
            fork: kind == LaunchKind::Fork,
        };
        self.close(cx);
        window.dispatch_action(Box::new(action), cx);
    }
}

impl EventEmitter<SessionHistoryEvent> for SessionHistoryModal {}

impl Focusable for SessionHistoryModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SessionHistoryModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let motion = crate::motion_settings::active(cx);
        let order = self.filtered();
        let row_count = order.len();
        let selected = self.selected_idx;
        let entity = cx.entity();

        let mut list = div()
            .id("session-history-list")
            .flex()
            .flex_col()
            .w_full()
            .px(px(density.pad_overlay))
            .py(px(4.))
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll();

        if self.loading {
            list = list.child(hint_row("Scanning sessions…", theme, &typography));
        } else if row_count == 0 {
            let msg = if self.entries.is_empty() {
                "No past sessions found"
            } else {
                "No matching sessions"
            };
            list = list.child(hint_row(msg, theme, &typography));
        } else {
            for (i, &entry_idx) in order.iter().enumerate() {
                let entry = &self.entries[entry_idx];
                let title = picker::session_row_title(entry);
                let subtitle = picker::session_row_subtitle(entry, self.now_ms);
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
                list = list.child(
                    div()
                        .id(("session-row", i))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap(px(2.))
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px(px(10.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(theme.selection))
                        .when(!is_selected, |d| d.hover(|s| s.bg(theme.hover_overlay)))
                        .on_mouse_down(
                            MouseButton::Left,
                            move |_e, window, cx| {
                                ent.update(cx, |m, cx| m.launch(i, LaunchKind::Resume, window, cx));
                            },
                        )
                        .child(title_line)
                        .child(subtitle_line),
                );
            }
        }

        let dismiss_entity = entity.clone();
        let card = div()
            .flex()
            .flex_col()
            .w(px(MODAL_WIDTH))
            .floating_chrome(&theme, &density)
            .overflow_hidden()
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_e, _window, cx| cx.stop_propagation())
            .child(header_row(&self.query, theme, &typography))
            .child(divider(theme))
            .child(list)
            .child(divider(theme))
            .child(footer_hints(theme, &typography));

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(MODAL_TOP_OFFSET_PX))
            .bg(hsla(0.0, 0.0, 0.0, SCRIM_ALPHA))
            .on_mouse_down(MouseButton::Left, move |_e, _window, cx| {
                dismiss_entity.update(cx, |m, cx| m.close(cx));
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                match key {
                    "escape" => this.close(cx),
                    "up" => this.move_selection(-1, row_count, cx),
                    "down" => this.move_selection(1, row_count, cx),
                    "enter" => {
                        let kind = if event.keystroke.modifiers.shift {
                            LaunchKind::Fork
                        } else {
                            LaunchKind::Resume
                        };
                        this.launch(this.selected_idx, kind, window, cx);
                    }
                    "backspace" => {
                        this.query.pop();
                        this.selected_idx = 0;
                        cx.notify();
                    }
                    _ => {
                        if event.keystroke.modifiers.control
                            || event.keystroke.modifiers.platform
                            || event.keystroke.modifiers.alt
                            || event.keystroke.modifiers.function
                        {
                            return;
                        }
                        if key.chars().count() == 1 {
                            this.query.push_str(key);
                            this.selected_idx = 0;
                            cx.notify();
                        }
                    }
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

fn header_row(query: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    let query_area = div()
        .flex()
        .flex_1()
        .text_size(px(typography.t_body_md))
        .when(query.is_empty(), |d| {
            d.child(
                div()
                    .text_color(theme.fg_subtle)
                    .child("Search past sessions…"),
            )
        })
        .when(!query.is_empty(), |d| {
            d.child(div().text_color(theme.fg_base).child(query.to_string()))
        });

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
                .rounded(px(4.))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child("History"),
        )
        .child(query_area)
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

fn footer_hints(theme: Theme, typography: &Typography) -> impl IntoElement {
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
        .child(hint("↵ resume"))
        .child(hint("⇧↵ fork"))
        .child(hint("esc dismiss"))
}
