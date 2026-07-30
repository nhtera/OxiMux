//! Session History — a right-sidebar panel listing past Claude sessions for the
//! project, with a click to reopen any of them as a chat tab.
//!
//! Mirrors the reference agent-cockpit "session history" panel: a
//! Project/All scope toggle, a search field, and a scrollable list of session
//! rows (agent glyph + title + msg-count · relative-time · branch). Discovery
//! reuses [`SessionIndex::build`] on the background executor; row labels + fuzzy
//! filter reuse the shared [`crate::shell::session_history::picker`] helpers.
//! Rows cover every chat-capable adapter (Claude/Codex native, OpenCode/Pi
//! bridges) and lead with the adapter's icon so a session's origin is readable
//! at a glance. Choosing a row dispatches [`OpenChatSession`] with the entry's
//! own adapter + preset, routed by `WorkspaceRoot` to the active pane group
//! (import transcript + resume). Reopening as chat is the panel's primary
//! action; terminal-resume is on the row's ⋯ menu and the ⌘⇧H modal.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    Anchor, App, ClipboardItem, Context, FocusHandle, Focusable, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, div, hsla, prelude::FluentBuilder, px, svg,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
};

use oximux_agents::session_log::{
    now_unix_ms,
    session_index::{SessionEntry, SessionIndex, SessionScope},
    session_preview::{PreviewMessage, PreviewRole, load_session_preview},
};
use oximux_core::AgentAdapter;
use oximux_settings::{Theme, Typography};

use crate::actions::{OpenChatSession, ResumeAgentSession};
use crate::shell::agent_ui::agent_presentation::adapter_icon_path;
use crate::shell::session_history::entry_opens_as_chat;
use crate::shell::session_history::picker::{
    entry_slug, filter_sessions, session_row_subtitle, session_row_title,
};

const CARET_BLINK_MS: u64 = 530;
/// Opening turns pulled into an expanded card's inline preview.
const PREVIEW_MAX_MESSAGES: usize = 6;

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

    /// The one card currently expanded to show its inline turn preview (session
    /// id), or `None`. Accordion-style — expanding one collapses the previous.
    expanded: Option<String>,
    /// Cached previews by session id, so re-expanding is instant.
    previews: HashMap<String, Vec<PreviewMessage>>,
    /// Session ids whose preview is being read off-thread (shows a spinner).
    preview_loading: HashSet<String>,

    _load_task: Option<Task<()>>,
    _preview_task: Option<Task<()>>,
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
            expanded: None,
            previews: HashMap::new(),
            preview_loading: HashSet::new(),
            _load_task: None,
            _preview_task: None,
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
                        Some(home) => SessionIndex::build(
                            &home.join(".claude"),
                            &home.join(".codex"),
                            &home,
                            &scope,
                        ),
                        None => Vec::new(),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Every chat-capable session: Claude/Codex import live, OpenCode/
                // Pi open as transcript bridges. Terminal-only adapters stay on
                // the ⌘⇧H modal, since this panel's actions all lead to a chat tab.
                this.entries = entries.into_iter().filter(entry_opens_as_chat).collect();
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
        self.expanded = None;
        self.rescan(cx);
        cx.notify();
    }

    /// Expand/collapse a card's inline turn preview. Expanding one collapses the
    /// previous (accordion) and lazily loads the preview off-thread.
    fn toggle_expand(&mut self, sid: String, path: Option<String>, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(sid.as_str()) {
            self.expanded = None;
        } else {
            self.expanded = Some(sid.clone());
            if !self.previews.contains_key(&sid)
                && !self.preview_loading.contains(&sid)
                && let Some(path) = path.filter(|p| !p.is_empty())
            {
                self.load_preview(sid, path, cx);
            }
        }
        cx.notify();
    }

    /// Read a session's opening turns off the UI thread and cache them.
    fn load_preview(&mut self, sid: String, path: String, cx: &mut Context<Self>) {
        self.preview_loading.insert(sid.clone());
        let task = cx.spawn(async move |this, cx| {
            let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone()) else {
                return;
            };
            let msgs = executor
                .spawn(async move {
                    load_session_preview(std::path::Path::new(&path), PREVIEW_MAX_MESSAGES)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.previews.insert(sid.clone(), msgs);
                this.preview_loading.remove(&sid);
                cx.notify();
            });
        });
        self._preview_task = Some(task);
    }

    /// Resolve a session's working directory (its logged cwd, else the panel's
    /// project root) as a string — used by the row actions.
    fn entry_cwd(&self, entry: &SessionEntry) -> String {
        entry
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.project_root.to_string_lossy().into_owned())
    }

    /// The expanded card's inline turn preview (or a loading/empty hint) plus an
    /// "Open as chat" action, indented under the chevron.
    #[allow(clippy::too_many_arguments)]
    fn render_preview(
        &self,
        sid: &str,
        path: &str,
        cwd: &str,
        adapter: AgentAdapter,
        preset_id: Option<String>,
        theme: Theme,
        typo: &Typography,
    ) -> impl IntoElement {
        // The whole expanded body is one sunken container inset under the row —
        // a distinct surface that separates the preview from the flat row list.
        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(6.0))
            .mx(px(6.0))
            .mt(px(2.0))
            .mb(px(6.0))
            .p(px(8.0))
            .rounded(px(8.0))
            .bg(theme.bg_base)
            .border_1()
            .border_color(theme.border_inactive)
            // Section header — labels the block and anchors the visual grouping.
            .child(
                div()
                    .text_size(px(typo.t_label_xs))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.fg_subtle)
                    .child("LATEST TURNS"),
            );
        if self.preview_loading.contains(sid) {
            col = col.child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("Loading preview…"),
            );
        } else if let Some(msgs) = self.previews.get(sid).filter(|m| !m.is_empty()) {
            for m in msgs {
                col = col.child(preview_turn(m, theme, typo));
            }
        } else {
            col = col.child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("No preview available."),
            );
        }
        // Primary action for the expanded card — a filled accent button so it
        // reads as the card's call-to-action, not another line of text.
        let open = OpenChatSession {
            session_id: sid.to_string(),
            path: path.to_string(),
            cwd: cwd.to_string(),
            adapter,
            // `Some` for OpenCode/Pi → the handler builds a transcript bridge
            // instead of a live resume.
            preset_id,
        };
        col.child(
            div().flex().flex_row().pt(px(2.0)).child(
                div()
                    .id(SharedString::from(format!("hist-open-{sid}")))
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(12.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .text_size(px(typo.t_label_xs))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.focus_ring)
                    .bg(Hsla { a: 0.16, ..theme.focus_ring })
                    .border_1()
                    .border_color(Hsla { a: 0.30, ..theme.focus_ring })
                    .hover(|s| s.bg(Hsla { a: 0.28, ..theme.focus_ring }))
                    .child("Open as chat")
                    .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
                        window.dispatch_action(Box::new(open.clone()), cx);
                    }),
            ),
        )
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
                adapter: entry.adapter,
                preset_id: entry.preset_id.clone(),
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
                    let sid = entry.session_id.clone();
                    let path = entry.path.clone().unwrap_or_default();
                    let cwd = self.entry_cwd(entry);
                    let adapter = entry.adapter;
                    let preset_id = entry.preset_id.clone();
                    let icon_path = adapter_icon_path(entry_slug(entry));
                    let is_expanded = self.expanded.as_deref() == Some(sid.as_str());
                    let chevron = if is_expanded { "▾" } else { "▸" };

                    // Header row: an expand zone (chevron + title + meta) that
                    // toggles the inline preview, plus a ⋯ actions menu. The zone
                    // and the menu are siblings so their clicks never collide.
                    let expand_sid = sid.clone();
                    let expand_path = path.clone();
                    let header = div()
                        // Stateful id: hover styles only repaint on hover change
                        // for id'd elements; without it the highlight waits for
                        // the next unrelated notify (the caret blink).
                        .id(SharedString::from(format!("hist-row-{sid}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .w_full()
                        .px(px(6.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .when(selected || is_expanded, |d| d.bg(t.hover_overlay))
                        .hover(|s| s.bg(t.hover_overlay))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .flex_1()
                                .min_w_0()
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, _w, cx| {
                                        this.selected_idx = list_idx;
                                        this.toggle_expand(
                                            expand_sid.clone(),
                                            Some(expand_path.clone()),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .w(px(10.0))
                                        .text_size(px(typo.t_label_xs))
                                        .text_color(t.fg_subtle)
                                        .child(chevron),
                                )
                                // Leading agent glyph: which agent this session
                                // belongs to (Claude / Codex / OpenCode / Pi …).
                                .child(
                                    Icon::default()
                                        .path(icon_path)
                                        .size(px(13.0))
                                        .flex_shrink_0()
                                        .text_color(t.fg_muted),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .gap(px(1.0))
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
                                ),
                        )
                        .child(dots_menu(
                            sid.clone(),
                            path.clone(),
                            cwd.clone(),
                            adapter,
                            preset_id.clone(),
                        ));

                    let mut card = div().flex().flex_col().w_full().child(header);
                    if is_expanded {
                        card = card.child(self.render_preview(
                            &sid,
                            &path,
                            &cwd,
                            adapter,
                            preset_id.clone(),
                            t,
                            &typo,
                        ));
                    }
                    list = list.child(card);
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

/// One previewed turn: a lifted, bordered box (so consecutive turns read as
/// distinct cards) with an uppercase role chip over its height-clamped body.
fn preview_turn(m: &PreviewMessage, theme: Theme, typo: &Typography) -> impl IntoElement {
    let (label, label_color) = match m.role {
        PreviewRole::User => ("YOU", theme.focus_ring),
        PreviewRole::Assistant => ("CLAUDE", theme.fg_muted),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .w_full()
        .p(px(8.0))
        .rounded(px(6.0))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(label_color)
                .child(label),
        )
        .child(
            // Clamp to a few lines so a long turn doesn't blow out the card; the
            // full text is on the opened session. Muted so the role chip leads.
            div()
                .w_full()
                .max_h(px(58.0))
                .overflow_hidden()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(m.text.clone())),
        )
}

/// The per-row `⋯` actions menu (reopen as chat, resume in terminal, copy ids,
/// reveal the log). Actions are dispatched or write the clipboard directly, so
/// the menu needs no entity handle — just the captured session facts.
fn dots_menu(
    sid: String,
    path: String,
    cwd: String,
    adapter: AgentAdapter,
    preset_id: Option<String>,
) -> impl IntoElement {
    Button::new(SharedString::from(format!("hist-dots-{sid}")))
        .ghost()
        .xsmall()
        .icon(Icon::default().path("icons/ellipsis.svg"))
        .tooltip("Session actions")
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _window, _cx| {
            build_session_menu(
                menu,
                sid.clone(),
                path.clone(),
                cwd.clone(),
                adapter,
                preset_id.clone(),
            )
        })
}

fn build_session_menu(
    menu: PopupMenu,
    sid: String,
    path: String,
    cwd: String,
    adapter: AgentAdapter,
    preset_id: Option<String>,
) -> PopupMenu {
    let has_path = !path.is_empty();
    let (open_sid, open_path, open_cwd) = (sid.clone(), path.clone(), cwd.clone());
    let open_preset = preset_id.clone();
    // Pi resumes by rollout file path (`pi --session <file>`); everything else
    // resumes by session id.
    let resume_handle = if preset_id.as_deref() == Some("pi") && has_path {
        path.clone()
    } else {
        sid.clone()
    };
    let (resume_sid, resume_cwd) = (sid.clone(), cwd);
    let copy_sid = sid;
    // These actions are handled by element-level `on_action` handlers on
    // `WorkspaceRoot`, so they must be dispatched through the WINDOW (which
    // bubbles up the focus tree) — `App::dispatch_action` only reaches GLOBAL
    // handlers and would silently no-op.
    let mut menu = menu
        .min_w(px(210.0))
        .item(
            PopupMenuItem::new("Open as chat").on_click(move |_, window, cx| {
                window.dispatch_action(
                    Box::new(OpenChatSession {
                        session_id: open_sid.clone(),
                        path: open_path.clone(),
                        cwd: open_cwd.clone(),
                        adapter,
                        preset_id: open_preset.clone(),
                    }),
                    cx,
                );
            }),
        )
        .item(
            PopupMenuItem::new("Resume in terminal").on_click(move |_, window, cx| {
                window.dispatch_action(
                    Box::new(ResumeAgentSession {
                        session_id: resume_sid.clone(),
                        adapter,
                        preset_id: preset_id.clone(),
                        resume_handle: resume_handle.clone(),
                        cwd: resume_cwd.clone(),
                        fork: false,
                    }),
                    cx,
                );
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new("Copy session id").on_click(move |_, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_sid.clone()));
            }),
        );
    if has_path {
        let copy_path = path.clone();
        let reveal_path = path;
        menu = menu
            .item(
                PopupMenuItem::new("Copy log path").on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
                }),
            )
            .item(
                // `open -R` reveals AND selects the file in Finder — plain `open`
                // (what `OpenInFinder` dispatches) would instead try to OPEN the
                // .jsonl in a text editor. Run it directly; no action needed.
                PopupMenuItem::new("Reveal log in Finder").on_click(move |_, _window, _cx| {
                    let _ = std::process::Command::new("open")
                        .arg("-R")
                        .arg(&reveal_path)
                        .spawn();
                }),
            );
    }
    menu
}
