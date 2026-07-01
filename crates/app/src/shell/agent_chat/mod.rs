//! Agent Chat view — a dedicated tab that renders a Claude Code session as a
//! structured chat thread (user/assistant bubbles, streaming text, collapsible
//! thinking, tool-call lines) instead of a raw terminal.
//!
//! It owns a [`ChatThread`] (the gpui-free conversation model from
//! `oximux-agents`) plus a live [`AgentConnection`] to a headless `claude`
//! subprocess. Decoded events arrive on a background channel; a foreground task
//! folds each into the thread and repaints. The raw-PTY terminal agent path is
//! untouched — this is an additive second surface.
//!
//! Fail-closed: if the subprocess dies (stdout EOF) while a permission is
//! pending, the drain task rejects it rather than leaving a dangling prompt.

mod bubble;

use std::collections::HashSet;
use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_component::input::{Enter as InputEnter, Input, InputEvent, InputState};
use oximux_agents::thread::{
    AgentConnection, ChatThread, ClaudeStreamJsonConnection, PermissionDecision, ThreadEntry,
    ThreadEvent, ToolCallStatus,
};
use oximux_settings::{Density, Theme, Typography};

pub struct AgentChatView {
    /// The conversation model. Owned directly (not a nested entity) — the view
    /// is its sole mutator, on the foreground thread.
    thread: ChatThread,
    /// The live agent connection. `None` if the subprocess failed to spawn (a
    /// read-only error state) or after teardown.
    connection: Option<Box<dyn AgentConnection>>,
    /// Multi-line prompt input. `⌘↵` sends; bare Enter inserts a newline.
    composer: Entity<InputState>,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Launch context, retained for a future `--resume`.
    #[allow(dead_code)]
    cwd: PathBuf,
    #[allow(dead_code)]
    model: Option<String>,
    /// Set once the event channel closes (process exit / EOF). Disables sending.
    disconnected: bool,
    /// Assistant entry indices whose thinking disclosure is expanded.
    expanded_thinking: HashSet<usize>,
    /// Foreground event-drain task. Dropping it only cancels the *foreground*
    /// half at its next await point — it does NOT stop the forwarder/reader OS
    /// threads or reap the subprocess. Subprocess + thread teardown is owned by
    /// `Drop::shutdown()` (which kills the child → stdout EOF → both threads
    /// unwind). Keep that the single cleanup owner across future refactors.
    _drain_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentChatView {
    /// Construct a chat view and spawn its headless `claude` subprocess in
    /// `cwd`. A spawn failure degrades to a read-only error state rather than
    /// panicking, so the tab still opens and explains what went wrong.
    pub fn new(
        cwd: PathBuf,
        model: Option<String>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Message Claude…  (⌘↵ to send)")
        });
        let subscriptions = vec![cx.subscribe_in(
            &composer,
            window,
            |_this, _input, ev: &InputEvent, _window, cx| {
                // Repaint on edits so the composer reflects the typed value.
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            },
        )];

        let mut thread = ChatThread::new();
        let mut connection: Option<Box<dyn AgentConnection>> = None;
        let mut disconnected = false;
        let mut drain_task = None;
        match ClaudeStreamJsonConnection::spawn(&cwd, model.as_deref()) {
            Ok((conn, rx)) => {
                connection = Some(Box::new(conn));
                drain_task = Some(Self::spawn_drain(rx, cx));
            }
            Err(e) => {
                thread.last_error = Some(format!("Failed to start agent: {e}"));
                disconnected = true;
            }
        }

        Self {
            thread,
            connection,
            composer,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            theme,
            density,
            typography,
            cwd,
            model,
            disconnected,
            expanded_thinking: HashSet::new(),
            _drain_task: drain_task,
            _subscriptions: subscriptions,
        }
    }

    /// FocusHandle of the inner composer — the pane focuses this on activate so
    /// keystrokes land in the draft without a click first.
    pub fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }

    /// Test-only constructor: inject a connection (a `StubConnection`) instead
    /// of spawning a real subprocess, and skip the background drain so a
    /// `#[gpui::test]` can drive `on_event`/`on_disconnect` synchronously.
    #[cfg(test)]
    fn with_connection_for_test(
        connection: Box<dyn AgentConnection>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| InputState::new(window, cx).multi_line(true));
        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            composer,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            theme,
            density,
            typography,
            cwd: PathBuf::new(),
            model: None,
            disconnected: false,
            expanded_thinking: HashSet::new(),
            _drain_task: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Bridge the connection's blocking `std::mpsc` receiver onto the
    /// foreground: a dedicated OS thread forwards each decoded event to an async
    /// channel a `cx.spawn` task awaits and applies. The forwarder exits when
    /// the process closes stdout, which ends the async channel and triggers the
    /// fail-closed disconnect handler.
    fn spawn_drain(
        rx: std::sync::mpsc::Receiver<ThreadEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let (fwd_tx, mut fwd_rx) = futures::channel::mpsc::unbounded::<ThreadEvent>();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                if fwd_tx.unbounded_send(ev).is_err() {
                    break; // view gone
                }
            }
            // `rx` disconnected (stdout EOF / process exit): `fwd_tx` drops here,
            // so the foreground task observes the channel end and fails closed.
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(ev) = fwd_rx.next().await {
                if this.update(cx, |view, cx| view.on_event(ev, cx)).is_err() {
                    return; // view dropped
                }
            }
            let _ = this.update(cx, |view, cx| view.on_disconnect(cx));
        })
    }

    /// Fold one decoded event into the thread and repaint.
    fn on_event(&mut self, ev: ThreadEvent, cx: &mut Context<Self>) {
        self.thread.apply(&ev);
        cx.notify();
    }

    /// The event channel closed — the agent process exited or its stdout was
    /// closed. Fail closed: if a permission was still pending, reject it (the
    /// tool never ran, since the process is gone). Best-effort deny in case
    /// stdin is briefly still writable, then mark the tool `Rejected` so the UI
    /// never shows a dangling approval prompt.
    fn on_disconnect(&mut self, cx: &mut Context<Self>) {
        let pending = self
            .thread
            .pending_permission()
            .map(|(tool_id, req)| (tool_id.to_string(), req.request_id.clone()));
        if let Some((tool_id, request_id)) = pending {
            if let Some(conn) = &self.connection {
                let _ = conn.resolve_permission(
                    &request_id,
                    PermissionDecision::Deny { message: "agent disconnected".into() },
                );
            }
            self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
        }
        self.thread.turn_active = false;
        self.disconnected = true;
        if self.thread.last_error.is_none() {
            self.thread.last_error = Some("Agent process exited.".into());
        }
        cx.notify();
    }

    /// Send the composer's draft as a new user turn, then clear the input.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        let text = text.trim().to_string();
        if text.is_empty() || self.disconnected {
            return;
        }
        // Optimistically record the prompt + mark the turn active.
        self.thread.push_user_message(text.clone());
        if let Some(conn) = &self.connection
            && let Err(e) = conn.send_user_message(&text)
        {
            self.thread.last_error = Some(format!("Send failed: {e}"));
        }
        self.composer.update(cx, |s, cx| s.set_value("", window, cx));
        self.list_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn toggle_thinking(&mut self, idx: usize, cx: &mut Context<Self>) {
        // `insert` returns false when already present → toggle off.
        if !self.expanded_thinking.insert(idx) {
            self.expanded_thinking.remove(&idx);
        }
        cx.notify();
    }

    /// The scrollable transcript column.
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let mut list = div()
            .id("agent-chat-list")
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .gap(px(density.gap_inline * 1.5))
            .px(px(density.pad_panel))
            .py(px(density.pad_panel))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll);

        if self.thread.entries.is_empty() {
            return list.child(self.render_empty_hint(&theme, &typo)).into_any_element();
        }

        for (idx, entry) in self.thread.entries.iter().enumerate() {
            match entry {
                ThreadEntry::User(text) => {
                    list = list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .w_full()
                            .child(bubble::role_caption("You", theme.fg_subtle, &typo))
                            .child(bubble::user_body(text, theme, density, &typo)),
                    );
                }
                ThreadEntry::Assistant(msg) => {
                    if msg.is_empty() {
                        continue;
                    }
                    let mut block = div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .w_full()
                        .child(bubble::role_caption("Claude", theme.fg_muted, &typo));
                    if !msg.thinking.is_empty() {
                        let expanded = self.expanded_thinking.contains(&idx);
                        block = block.child(thinking_block(
                            idx, expanded, &msg.thinking, theme, density, &typo, cx,
                        ));
                    }
                    if !msg.text.is_empty() {
                        block = block.child(bubble::assistant_body(idx, &msg.text, &typo));
                    }
                    list = list.child(block);
                }
                ThreadEntry::ToolCall(tc) => {
                    list = list.child(bubble::tool_line(tc, theme, density, &typo));
                }
            }
        }
        list.into_any_element()
    }

    fn render_empty_hint(&self, theme: &Theme, typo: &Typography) -> AnyElement {
        let msg = if self.disconnected {
            self.thread.last_error.as_deref().unwrap_or("Agent unavailable.")
        } else {
            "Send a message to start the conversation."
        };
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .w_full()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_subtle)
            .child(SharedString::from(msg.to_string()))
            .into_any_element()
    }

    /// The bottom composer: a status/hint line plus the multi-line input.
    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let status = if self.disconnected {
            "Disconnected — the agent process exited."
        } else if self.thread.turn_active {
            "Claude is working…"
        } else {
            "⌘↵ to send · Enter for newline"
        };
        let status_color = if self.disconnected {
            theme.status_error
        } else {
            theme.fg_subtle
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .border_t_1()
            .border_color(theme.border_inactive)
            .p(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .text_size(px(typo.t_label_xs))
                            .text_color(status_color)
                            .child(SharedString::from(status.to_string())),
                    )
                    .child(
                        div()
                            .id("agent-chat-send")
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(density.r_chip))
                            .text_size(px(typo.t_label_xs))
                            .text_color(theme.fg_muted)
                            .hover(|s| s.text_color(theme.fg_base).bg(theme.hover_overlay))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| this.submit(window, cx)),
                            )
                            .child(SharedString::from("Send ⌘↵")),
                    ),
            )
            .child(Input::new(&self.composer).text_size(px(typo.t_body_md)))
            .into_any_element()
    }
}

impl Drop for AgentChatView {
    fn drop(&mut self) {
        // Kill + reap the `claude` child so closing the tab doesn't leak it.
        if let Some(conn) = &self.connection {
            conn.shutdown();
        }
    }
}

impl Focusable for AgentChatView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let transcript = self.render_transcript(cx);
        let composer = self.render_composer(cx);
        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_panel)
            // The Input context binds `enter`→Enter{secondary:false} (newline)
            // and `⌘↵`/`ctrl+↵`→Enter{secondary:true}; both dispatch as the
            // `Enter` action and would be consumed by the field before any
            // `on_key_down` could see them, so submit MUST be captured here.
            .capture_action(cx.listener(|this, action: &InputEnter, window, cx| {
                if action.secondary {
                    this.submit(window, cx);
                } else {
                    cx.propagate(); // bare Enter → newline in the field
                }
            }))
            .child(transcript)
            .child(composer)
    }
}

/// A collapsible thinking disclosure: a clickable header (chevron + "Thinking")
/// and, when expanded, the muted body. Built here rather than in `bubble` since
/// the toggle needs a `Context` listener.
fn thinking_block(
    idx: usize,
    expanded: bool,
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let chevron = if expanded { "▾" } else { "▸" };
    let header = div()
        .id(("agent-chat-thinking", idx))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .hover(|s| s.text_color(theme.fg_muted))
        .on_click(cx.listener(move |this, _e, _window, cx| this.toggle_thinking(idx, cx)))
        .child(SharedString::from(format!("{chevron} Thinking")));

    let mut block = div().flex().flex_col().gap(px(2.0)).w_full().child(header);
    if expanded {
        block = block.child(bubble::thinking_body(text, theme, density, typo));
    }
    block.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::StubConnection;
    use serde_json::json;

    /// The spike's central fail-closed requirement: if the agent channel
    /// disconnects (process exit / EOF) while a permission is pending, the view
    /// rejects it — clearing the prompt and sending a best-effort deny — rather
    /// than leaving a dangling approval.
    #[gpui::test]
    async fn disconnect_fails_closed_pending_permission(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("edit notes");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({"file_path": "notes.txt"}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "notes.txt".into(),
                    suggestions: vec![],
                });
                assert!(
                    view.thread.pending_permission().is_some(),
                    "permission pending before disconnect"
                );

                view.on_disconnect(cx);

                assert!(
                    view.thread.pending_permission().is_none(),
                    "fail-closed clears the pending permission"
                );
                assert!(view.disconnected, "view marks itself disconnected");
            })
            .expect("window update");

        // Best-effort deny reached the (stub) connection.
        let sent = stub_probe.sent();
        assert!(
            sent.iter()
                .any(|s| s["response"]["response"]["behavior"] == "deny"),
            "disconnect must send a deny control_response, got {sent:?}"
        );
    }

    /// A normal streamed turn folds into user + assistant entries via `on_event`.
    #[gpui::test]
    async fn on_event_builds_transcript(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("hi");
                view.on_event(ThreadEvent::AssistantText("Hello!".into()), cx);
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: Some("Hello!".into()),
                        cost_usd: None,
                        is_error: false,
                    },
                    cx,
                );
                assert_eq!(view.thread.entries.len(), 2, "user + assistant");
                assert!(!view.thread.turn_active, "turn ended");
            })
            .expect("window update");
    }
}
