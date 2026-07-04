//! In-chat "Sessions" browser — an inline picker that lists past Claude
//! sessions for the current project (including ones started outside OxiMux in
//! the terminal CLI) and lets the user reopen any of them as a chat tab.
//!
//! Discovery reuses [`SessionIndex::build`] (bounded head/tail reads, project
//! scoping, title precedence) on the background executor — never the UI thread.
//! Filtering/labelling reuse the pure helpers in
//! [`crate::shell::session_history::picker`]. This view only owns presentation +
//! keyboard; the chosen session is imported and opened by the pane group (which
//! holds the tab state + settings repo), reached via [`SessionPickerEvent`].

use std::path::PathBuf;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, div, hsla, prelude::FluentBuilder, px,
};

use oximux_agents::session_log::{
    now_unix_ms,
    session_index::{SessionEntry, SessionIndex, SessionScope},
};
use oximux_settings::{Theme, Typography};

use crate::shell::session_history::picker::{
    filter_sessions, session_row_subtitle, session_row_title,
};

/// Raised when the picker resolves: either open a session as chat, or dismiss.
pub enum SessionPickerEvent {
    /// Open the session as a chat tab. `path` is the session `.jsonl` for
    /// import (external sessions); the handler prefers a persisted OxiMux blob
    /// when one exists for `session_id`. `cwd` roots the resumed subprocess.
    Chosen {
        session_id: String,
        path: Option<String>,
        cwd: PathBuf,
    },
    /// Closed without choosing (Escape / click-away).
    Closed,
}

/// Max visible list height before it scrolls (keeps the inline card compact).
const LIST_MAX_HEIGHT: f32 = 300.0;
const CARET_BLINK_MS: u64 = 530;

pub struct SessionPickerView {
    theme: Theme,
    typography: Typography,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,

    /// Project launch dirs (root + worktrees) — the discovery scope.
    scope_paths: Vec<String>,
    /// Fallback cwd for a chosen session whose log omits one.
    fallback_cwd: PathBuf,
    home: Option<String>,

    query: String,
    caret_on: bool,
    loading: bool,
    entries: Vec<SessionEntry>,
    selected_idx: usize,
    /// Captured at open so row ages render consistently for the session.
    now_ms: i64,

    _load_task: Option<Task<()>>,
    _caret_task: Task<()>,
}

impl SessionPickerView {
    pub fn new(
        scope_paths: Vec<String>,
        fallback_cwd: PathBuf,
        theme: Theme,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let mut this = Self {
            theme,
            typography,
            focus_handle,
            list_scroll: ScrollHandle::new(),
            scope_paths,
            fallback_cwd,
            home: dirs::home_dir().map(|h| h.to_string_lossy().into_owned()),
            query: String::new(),
            caret_on: true,
            loading: true,
            entries: Vec::new(),
            selected_idx: 0,
            now_ms: now_unix_ms(),
            _load_task: None,
            _caret_task: Self::start_caret_blink(cx),
        };
        this.rescan(cx);
        this
    }

    /// Scan past sessions for the scope on the background executor, then publish
    /// back on the UI thread. `SessionIndex::build` is blocking std::fs.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.entries.clear();
        // Empty scope (no active project) → list every project rather than
        // nothing, so the picker is never mysteriously blank.
        let scope = if self.scope_paths.is_empty() {
            SessionScope::AllProjects
        } else {
            SessionScope::Projects(self.scope_paths.clone())
        };
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let entries = executor
                .spawn(async move {
                    match dirs::home_dir() {
                        Some(home) => SessionIndex::build(
                            &home.join(".claude"),
                            &home.join(".codex"),
                            &scope,
                        ),
                        None => Vec::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Codex sessions have no chat-importable log; keep Claude only.
                this.entries = entries
                    .into_iter()
                    .filter(|e| e.adapter == oximux_core::AgentAdapter::ClaudeCode)
                    .collect();
                this.loading = false;
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    fn start_caret_blink(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone())
                else {
                    return;
                };
                executor
                    .timer(std::time::Duration::from_millis(CARET_BLINK_MS))
                    .await;
                if this
                    .update(cx, |m, cx| {
                        m.caret_on = !m.caret_on;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    fn filtered(&self) -> Vec<usize> {
        filter_sessions(&self.query, &self.entries)
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(SessionPickerEvent::Closed);
    }

    fn move_selection(&mut self, delta: isize, row_count: usize, cx: &mut Context<Self>) {
        if row_count == 0 {
            return;
        }
        self.selected_idx =
            crate::shell::project_picker::wrap_index(self.selected_idx, delta, row_count);
        cx.notify();
    }

    /// Open the session at filtered position `list_idx` as a chat tab.
    fn choose(&mut self, list_idx: usize, cx: &mut Context<Self>) {
        let order = self.filtered();
        let Some(&entry_idx) = order.get(list_idx) else {
            return;
        };
        let Some(entry) = self.entries.get(entry_idx) else {
            return;
        };
        let cwd = entry
            .cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.fallback_cwd.clone());
        cx.emit(SessionPickerEvent::Chosen {
            session_id: entry.session_id.clone(),
            path: entry.path.clone(),
            cwd,
        });
    }

    /// Short project-folder label for the header.
    fn scope_label(&self) -> String {
        self.scope_paths
            .first()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
            .unwrap_or_else(|| "All projects".to_string())
    }

    fn render_header(&self) -> impl IntoElement {
        let t = &self.theme;
        let typo = &self.typography;
        let caret_color = if self.caret_on { t.fg_base } else { hsla(0.0, 0.0, 0.0, 0.0) };
        let caret = div().w(px(1.5)).h(px(15.0)).rounded(px(1.0)).bg(caret_color);
        let query_area = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .gap(px(1.0))
            .text_size(px(typo.t_body_sm))
            .when(!self.query.is_empty(), |d| {
                d.child(div().text_color(t.fg_base).child(self.query.clone()))
            })
            .child(caret)
            .when(self.query.is_empty(), |d| {
                d.child(div().text_color(t.fg_subtle).child("Search past sessions…"))
            });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .pb(px(6.0))
            .child(
                gpui_component::Icon::default()
                    .path("icons/history.svg")
                    .size(px(14.0))
                    .text_color(t.fg_muted),
            )
            .child(query_area)
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(typo.t_label_xs))
                    .text_color(t.fg_subtle)
                    .child(SharedString::from(self.scope_label())),
            )
    }

}

impl EventEmitter<SessionPickerEvent> for SessionPickerView {}

impl Focusable for SessionPickerView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SessionPickerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let typo = self.typography.clone();
        let order = self.filtered();
        let row_count = order.len();

        let mut list = div()
            .id("session-picker-list")
            .flex()
            .flex_col()
            .w_full()
            .gap(px(1.0))
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll);

        if self.loading {
            list = list.child(
                div()
                    .py(px(8.0))
                    .text_size(px(typo.t_label_xs))
                    .text_color(t.fg_subtle)
                    .child("Scanning sessions…"),
            );
        } else if row_count == 0 {
            let msg = if self.query.is_empty() {
                "No past sessions for this project."
            } else {
                "No sessions match your search."
            };
            list = list.child(
                div()
                    .py(px(8.0))
                    .text_size(px(typo.t_label_xs))
                    .text_color(t.fg_subtle)
                    .child(msg),
            );
        } else {
            for (list_idx, &entry_idx) in order.iter().enumerate() {
                if let Some(entry) = self.entries.get(entry_idx) {
                    // Rebuild the row here with a real cx.listener (render_row's
                    // placeholder handler is replaced by wiring the click at
                    // this level, where `cx` is available).
                    let selected = list_idx == self.selected_idx;
                    let title = session_row_title(entry);
                    let subtitle =
                        session_row_subtitle(entry, self.now_ms, false, self.home.as_deref());
                    list = list.child(
                        div()
                            .id(("session-row", list_idx))
                            .flex()
                            .flex_col()
                            .w_full()
                            .gap(px(1.0))
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .when(selected, |d| d.bg(t.hover_overlay))
                            .hover(|s| s.bg(t.hover_overlay))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| this.choose(list_idx, cx)),
                            )
                            .child(
                                div()
                                    .text_size(px(typo.t_body_sm))
                                    .text_color(t.fg_base)
                                    .child(SharedString::from(title)),
                            )
                            .when(!subtitle.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(typo.t_label_xs))
                                        .text_color(t.fg_subtle)
                                        .child(SharedString::from(subtitle)),
                                )
                            }),
                    );
                }
            }
        }

        div()
            .track_focus(&self.focus_handle)
            .key_context("SessionPicker")
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(560.0))
            .p(px(10.0))
            .rounded(px(9.0))
            .bg(t.bg_panel_alt)
            .border_1()
            .border_color(t.border_inactive)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                match key {
                    "escape" => this.close(cx),
                    "up" => this.move_selection(-1, row_count, cx),
                    "down" => this.move_selection(1, row_count, cx),
                    "enter" => this.choose(this.selected_idx, cx),
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
            .child(self.render_header())
            .child(list)
            .child(
                div()
                    .pt(px(6.0))
                    .text_size(px(typo.t_label_xs))
                    .text_color(t.fg_subtle)
                    .child("↑↓ navigate · ↵ open as chat · esc close"),
            )
    }
}
