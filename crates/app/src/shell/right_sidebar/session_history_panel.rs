//! Session History — a right-sidebar panel listing past Claude sessions for the
//! project, with a click to reopen any of them as a chat tab.
//!
//! Mirrors the reference agent-cockpit "session history" panel: a
//! Project/All scope toggle, a search field, and a scrollable list of session
//! rows (title + msg-count · relative-time · branch). Discovery reuses
//! [`SessionIndex::build`] on the background executor; row labels + fuzzy filter
//! reuse the shared [`crate::shell::session_history::picker`] helpers. Choosing a
//! row dispatches [`OpenChatSession`], routed by `WorkspaceRoot` to the active
//! pane group (import transcript + resume). Reopening as chat is the panel's one
//! action; terminal-resume stays on the ⌘⇧H modal.

use std::path::PathBuf;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Task, Window, div, hsla, prelude::FluentBuilder, px, svg,
};

use oximux_agents::session_log::{
    now_unix_ms,
    session_index::{SessionEntry, SessionIndex, SessionScope},
};
use oximux_settings::{Theme, Typography};

use crate::actions::OpenChatSession;
use crate::shell::session_history::picker::{
    filter_sessions, session_row_subtitle, session_row_title,
};

const CARET_BLINK_MS: u64 = 530;

pub struct SessionHistoryPanel {
    theme: Theme,
    typography: Typography,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,

    /// Project root — the default (Project) scope and the fallback cwd for a
    /// chosen session whose log omits one.
    project_root: PathBuf,
    home: Option<String>,

    /// `true` = show every project's sessions; `false` = this project only.
    show_all: bool,
    query: String,
    caret_on: bool,
    loading: bool,
    entries: Vec<SessionEntry>,
    selected_idx: usize,
    now_ms: i64,

    _load_task: Option<Task<()>>,
    _caret_task: Task<()>,
}

impl SessionHistoryPanel {
    pub fn new(
        project_root: PathBuf,
        theme: Theme,
        typography: Typography,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            theme,
            typography,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            project_root,
            home: dirs::home_dir().map(|h| h.to_string_lossy().into_owned()),
            show_all: false,
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

    /// (Re)build the index for the current scope off the UI thread.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.entries.clear();
        self.selected_idx = 0;
        let scope = if self.show_all {
            SessionScope::AllProjects
        } else {
            SessionScope::Projects(vec![self.project_root.to_string_lossy().into_owned()])
        };
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let entries = executor
                .spawn(async move {
                    match dirs::home_dir() {
                        Some(home) => {
                            SessionIndex::build(&home.join(".claude"), &home.join(".codex"), &scope)
                        }
                        None => Vec::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Only Claude sessions can be imported into the chat view.
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
        cx.spawn(async move |this, cx| loop {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
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
        })
    }

    /// Focus the panel's search field (called when the tab is selected).
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn filtered(&self) -> Vec<usize> {
        filter_sessions(&self.query, &self.entries)
    }

    fn set_scope(&mut self, show_all: bool, cx: &mut Context<Self>) {
        if self.show_all == show_all {
            return;
        }
        self.show_all = show_all;
        self.rescan(cx);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, row_count: usize, cx: &mut Context<Self>) {
        if row_count == 0 {
            return;
        }
        self.selected_idx =
            crate::shell::project_picker::wrap_index(self.selected_idx, delta, row_count);
        cx.notify();
    }

    /// Reopen the session at filtered position `list_idx` as a chat tab.
    fn open(&mut self, list_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.filtered();
        let Some(&entry_idx) = order.get(list_idx) else {
            return;
        };
        let Some(entry) = self.entries.get(entry_idx) else {
            return;
        };
        let cwd = entry
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.project_root.to_string_lossy().into_owned());
        window.dispatch_action(
            Box::new(OpenChatSession {
                session_id: entry.session_id.clone(),
                path: entry.path.clone().unwrap_or_default(),
                cwd,
            }),
            cx,
        );
    }

    fn render_scope_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        let typo = &self.typography;
        let seg = |label: &'static str, active: bool, all: bool| {
            div()
                .id(SharedString::from(format!("hist-scope-{label}")))
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .py(px(4.0))
                .rounded(px(5.0))
                .cursor_pointer()
                .text_size(px(typo.t_label_xs))
                .text_color(if active { t.fg_base } else { t.fg_muted })
                .when(active, |d| d.bg(t.bg_panel))
                .when(!active, |d| d.hover(|s| s.bg(t.hover_overlay)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, _w, cx| this.set_scope(all, cx)),
                )
        };
        div()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(7.0))
            .bg(t.bg_base)
            .child(seg("This project", !self.show_all, false))
            .child(seg("All", self.show_all, true))
    }

    fn render_search(&self) -> impl IntoElement {
        let t = &self.theme;
        let typo = &self.typography;
        let caret_color = if self.caret_on { t.fg_base } else { hsla(0.0, 0.0, 0.0, 0.0) };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .bg(t.bg_base)
            .border_1()
            .border_color(t.border_inactive)
            .child(svg().path("icons/search.svg").size(px(12.0)).text_color(t.fg_subtle))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .gap(px(1.0))
                    .text_size(px(typo.t_body_sm))
                    .when(!self.query.is_empty(), |d| {
                        d.child(div().text_color(t.fg_base).child(self.query.clone()))
                    })
                    .child(div().w(px(1.5)).h(px(14.0)).rounded(px(1.0)).bg(caret_color))
                    .when(self.query.is_empty(), |d| {
                        d.child(div().text_color(t.fg_subtle).child("Search sessions…"))
                    }),
            )
    }
}

impl Focusable for SessionHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SessionHistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let typo = self.typography.clone();
        let order = self.filtered();
        let row_count = order.len();
        let count_label: SharedString = if self.loading {
            "scanning…".into()
        } else {
            format!("{} session{}", row_count, if row_count == 1 { "" } else { "s" }).into()
        };

        let mut list = div()
            .id("session-history-panel-list")
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .min_h(px(0.0))
            .gap(px(1.0))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll);

        if self.loading {
            list = list.child(hint_row("Scanning sessions…", t, &typo));
        } else if row_count == 0 {
            let msg = if self.query.is_empty() {
                "No past sessions here yet."
            } else {
                "No sessions match your search."
            };
            list = list.child(hint_row(msg, t, &typo));
        } else {
            for (list_idx, &entry_idx) in order.iter().enumerate() {
                if let Some(entry) = self.entries.get(entry_idx) {
                    let selected = list_idx == self.selected_idx;
                    let title = session_row_title(entry);
                    // In the All view, append the project path so rows are
                    // attributable; scoped view shares one project so it's omitted.
                    let subtitle =
                        session_row_subtitle(entry, self.now_ms, self.show_all, self.home.as_deref());
                    list = list.child(
                        div()
                            .id(("session-history-row", list_idx))
                            .flex()
                            .flex_col()
                            .w_full()
                            .gap(px(1.0))
                            .px(px(8.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .when(selected, |d| d.bg(t.hover_overlay))
                            .hover(|s| s.bg(t.hover_overlay))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.selected_idx = list_idx;
                                    this.open(list_idx, window, cx);
                                }),
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
            .key_context("SessionHistoryPanel")
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .gap(px(8.0))
            .p(px(10.0))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                match key {
                    "up" => this.move_selection(-1, row_count, cx),
                    "down" => this.move_selection(1, row_count, cx),
                    "enter" => this.open(this.selected_idx, window, cx),
                    "backspace" => {
                        this.query.pop();
                        this.selected_idx = 0;
                        cx.notify();
                    }
                    _ => {
                        if ev.keystroke.modifiers.control
                            || ev.keystroke.modifiers.platform
                            || ev.keystroke.modifiers.alt
                            || ev.keystroke.modifiers.function
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
            // Header: title + count.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(typo.t_body_md))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.fg_base)
                            .child("Session History"),
                    )
                    .child(
                        div()
                            .text_size(px(typo.t_label_xs))
                            .text_color(t.fg_subtle)
                            .child(count_label),
                    ),
            )
            .child(self.render_scope_toggle(cx))
            .child(self.render_search())
            .child(list)
    }
}

fn hint_row(msg: &str, theme: Theme, typo: &Typography) -> impl IntoElement {
    div()
        .py(px(8.0))
        .px(px(2.0))
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(msg.to_string()))
}
