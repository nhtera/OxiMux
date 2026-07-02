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
mod composer;
mod diff_card;
mod tool_card;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext, ClipboardItem, Context, Entity,
    FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Transformation,
    WeakEntity, Window, div, percentage, px,
};
use gpui_component::Icon;
use gpui_component::input::Enter as InputEnter;
use gpui_component::scroll::Scrollbar;

/// Max width of the reading column (transcript + composer). Wider windows keep
/// the conversation centered in a comfortable measure rather than stretching
/// text edge-to-edge — the calm, focused feel of a dedicated chat surface.
pub(super) const CONTENT_MAX_W: f32 = 720.0;

use composer::{ComposerEvent, ComposerView};
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
    /// The bottom composer (status line + input + Send button), isolated into
    /// its own view so typing repaints only it, never the transcript. It reports
    /// submissions back via [`ComposerEvent`].
    composer: Entity<ComposerView>,
    focus_handle: FocusHandle,
    list_scroll: ScrollHandle,
    /// Whether the transcript auto-follows the bottom. True by default and while
    /// the user stays at the end; set false when they scroll up to read history
    /// (so streaming doesn't yank them down), re-armed when they scroll back to
    /// the bottom or send a new message. `render` re-pins every frame while true,
    /// which keeps the newest row glued even as its height settles a frame after
    /// it arrives (markdown/diff measuring) — a single per-event scroll lands
    /// short in that case.
    stick_to_bottom: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Launch context, retained so [`Self::respawn`] can re-spawn the subprocess
    /// (Stop→next-send resume) in the same directory with the same model.
    cwd: PathBuf,
    model: Option<String>,
    /// Set once the event channel closes (process exit / EOF). Disables sending.
    disconnected: bool,
    /// True after the user pressed Stop: the turn was interrupted and the child
    /// exited, but the session is **resumable** — the next send transparently
    /// respawns with `--resume`. Distinct from `disconnected` (an unexpected
    /// crash, which stays unavailable), so an intentional Stop shows no error.
    interrupted: bool,
    /// Assistant entry indices whose thinking disclosure is expanded.
    expanded_thinking: HashSet<usize>,
    /// Tool-call ids whose card disclosure (raw input + result) is expanded.
    expanded_tool_calls: HashSet<String>,
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
        Self::assemble(cwd, model, ChatThread::new(), theme, density, typography, window, cx)
    }

    /// Rebuild a chat view on session restore: seed the thread from the
    /// persisted transcript and spawn the subprocess with `--resume
    /// <session_id>` (via [`ChatThread::rehydrated`]'s captured id) so the
    /// continued conversation keeps its context. The visible history paints
    /// immediately from `entries` — it does not wait on the resumed process.
    ///
    /// LIVE-VERIFY: `claude -p --resume` in stream-json mode is expected to load
    /// the session server-side and wait for input (not replay prior turns to
    /// stdout). If it *does* replay, the drain would append duplicate entries
    /// atop the rehydrated ones — watch for doubled bubbles on the first restore
    /// eyeball; the fix would be to drop the rehydrated seed and render purely
    /// from the replay.
    #[allow(clippy::too_many_arguments)]
    pub fn new_resumed(
        cwd: PathBuf,
        model: Option<String>,
        session_id: Option<String>,
        entries: Vec<ThreadEntry>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread = ChatThread::rehydrated(session_id, model.clone(), entries);
        Self::assemble(cwd, model, thread, theme, density, typography, window, cx)
    }

    /// Shared construction for [`new`]/[`new_resumed`]: wire the composer, spawn
    /// the subprocess (resuming when `thread.session_id` is set), and start the
    /// event drain. A spawn failure degrades to a read-only error state so the
    /// tab still opens and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cwd: PathBuf,
        model: Option<String>,
        mut thread: ChatThread,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer =
            cx.new(|cx| ComposerView::new(theme, density, typography.clone(), window, cx));
        // The composer owns its input and repaints itself per keystroke. We only
        // react when it reports a finished submission — so typing never touches
        // this view (and thus never rebuilds the transcript, which is the lag we
        // want to avoid).
        let subscriptions = vec![cx.subscribe(
            &composer,
            |this, _composer, ev: &ComposerEvent, cx| match ev {
                ComposerEvent::Submit(text) => this.send_text(text.clone(), cx),
                ComposerEvent::Stop => this.stop_turn(cx),
            },
        )];

        // A resumed thread carries the prior session id; a fresh one is `None`
        // (spawn a new session). Either way the subprocess is spawned the same.
        let resume_session_id = thread.session_id.clone();
        let mut connection: Option<Box<dyn AgentConnection>> = None;
        let mut disconnected = false;
        let mut drain_task = None;
        match ClaudeStreamJsonConnection::spawn_resumed(
            &cwd,
            model.as_deref(),
            resume_session_id.as_deref(),
        ) {
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
            stick_to_bottom: true,
            theme,
            density,
            typography,
            cwd,
            model,
            disconnected,
            interrupted: false,
            expanded_thinking: HashSet::new(),
            expanded_tool_calls: HashSet::new(),
            _drain_task: drain_task,
            _subscriptions: subscriptions,
        }
    }

    /// Snapshot the transcript for persistence, or `None` when there's nothing
    /// worth restoring. A session id is required (it keys the blob and drives
    /// `--resume`); a chat with no completed turn has neither an id nor history,
    /// so it simply won't restore — the tab reopens fresh.
    pub fn transcript_snapshot(&self) -> Option<crate::persisted_chat::PersistedChatTranscript> {
        let session_id = self.thread.session_id.clone()?;
        if self.thread.entries.is_empty() {
            return None;
        }
        Some(crate::persisted_chat::PersistedChatTranscript {
            session_id,
            model: self.thread.model.clone().or_else(|| self.model.clone()),
            entries: self.thread.entries.clone(),
        })
    }

    /// The chat's session id once Claude has minted one (after the first turn
    /// begins). Persisted in the tab's `PersistedTabKind::AgentChat` so restore
    /// can find the matching transcript blob and `--resume`.
    pub fn session_id(&self) -> Option<&str> {
        self.thread.session_id.as_deref()
    }

    /// FocusHandle of the inner composer — the pane focuses this on activate so
    /// keystrokes land in the draft without a click first.
    pub fn composer_focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.read(cx).focus_handle(cx)
    }

    /// Push the current connection/turn state into the composer so its status
    /// line + Send button reflect reality. Cheap no-op when nothing changed.
    fn sync_composer(&self, cx: &mut Context<Self>) {
        let (disconnected, turn_active) = (self.disconnected, self.thread.turn_active);
        self.composer
            .update(cx, |c, cx| c.set_state(disconnected, turn_active, cx));
    }

    /// Record + transmit a submitted prompt (from the composer's Submit event).
    /// The composer has already cleared its own input.
    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // A prior Stop killed the child but left the session resumable — bring it
        // back with `--resume` before sending so the conversation continues.
        if self.interrupted {
            self.respawn(self.model.clone(), cx);
        }
        if self.disconnected {
            return; // unrecoverable (a crash, or the resume failed) — nothing to send to
        }
        // Optimistically record the prompt; the reply streams in via `on_event`.
        self.thread.push_user_message(text.clone());
        if let Some(conn) = &self.connection
            && let Err(e) = conn.send_user_message(&text)
        {
            self.thread.last_error = Some(format!("Send failed: {e}"));
        }
        // Jump to (and re-arm following of) the bottom for the new turn.
        self.stick_to_bottom = true;
        self.list_scroll.scroll_to_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Interrupt the streaming turn (the composer's Stop button). SIGINTs the
    /// child, finalizes the transcript, and fail-closes any pending approval,
    /// then marks the session **resumable-idle**: the next send respawns via
    /// `--resume`. Not marked `disconnected` — the stop was intentional, so no
    /// error banner is shown.
    fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if !self.thread.turn_active {
            return; // nothing is streaming
        }
        if let Some(conn) = &self.connection {
            let _ = conn.cancel();
        }
        self.interrupted = true;
        self.thread.interrupt();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Reap the current child and spawn a fresh one resuming the same session
    /// (`--resume <session_id>`) on `model`, rewiring the event drain. The one
    /// place a live chat re-establishes its subprocess — shared by Stop→next-send
    /// and a later in-chat model switch. Degrades to a read-only error state if
    /// the respawn fails.
    fn respawn(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        // Reap the old connection before replacing it — `Child`'s Drop neither
        // kills nor waits, so after a Stop this harvests the already-dead child
        // (and hard-kills it if somehow still alive).
        if let Some(old) = self.connection.take() {
            old.shutdown();
        }
        let session_id = self.thread.session_id.clone();
        match ClaudeStreamJsonConnection::spawn_resumed(
            &self.cwd,
            model.as_deref(),
            session_id.as_deref(),
        ) {
            Ok((conn, rx)) => {
                self.connection = Some(Box::new(conn));
                // Reassigning drops the old drain task, cancelling its foreground
                // half; its forwarder thread then exits on the dead child's
                // stdout EOF. We're single-threaded here, so no stale
                // `on_disconnect` can interleave onto the fresh connection.
                self._drain_task = Some(Self::spawn_drain(rx, cx));
                self.interrupted = false;
                self.disconnected = false;
                self.thread.last_error = None;
            }
            Err(e) => {
                self.thread.last_error = Some(format!("Failed to resume agent: {e}"));
                self.disconnected = true;
                self.interrupted = false;
            }
        }
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
        let composer =
            cx.new(|cx| ComposerView::new(theme, density, typography.clone(), window, cx));
        Self {
            thread: ChatThread::new(),
            connection: Some(connection),
            composer,
            focus_handle: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            stick_to_bottom: true,
            theme,
            density,
            typography,
            cwd: PathBuf::new(),
            model: None,
            disconnected: false,
            interrupted: false,
            expanded_thinking: HashSet::new(),
            expanded_tool_calls: HashSet::new(),
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
        // A user-initiated Stop makes `claude` end the turn with an
        // `error_during_execution` result (terminal_reason: aborted_streaming).
        // That's the expected shape of an interrupt, not a failure — swallow it
        // so an intentional Stop never flashes an error banner.
        if self.interrupted {
            self.thread.last_error = None;
        }
        // Following (and the actual `scroll_to_bottom`) is owned by `render` via
        // `stick_to_bottom`, so newly-arrived content — streamed text, a tall
        // tool card, an Allow/Reject row — stays glued as it settles. Nothing to
        // scroll here; just repaint.
        // The turn's active flag may have flipped (e.g. `TurnEnded`); keep the
        // composer's status line in step.
        self.sync_composer(cx);
        cx.notify();
    }

    /// Whether the transcript is scrolled to (within one card of) the bottom.
    /// `offset().y` is `<= 0` and reaches `-max_offset().y` at the very bottom,
    /// so their sum is the remaining scroll distance. Fresh views (no paint yet)
    /// report `0`, i.e. "at bottom", so the first turn follows.
    fn is_near_bottom(&self) -> bool {
        let sh = &self.list_scroll;
        sh.max_offset().y + sh.offset().y <= px(160.0)
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
        if self.interrupted {
            // Intentional Stop: the child exited exactly as asked. Stay
            // resumable-idle (the next send respawns via `--resume`) instead of
            // marking the tab unavailable.
            self.thread.last_error = None;
            self.sync_composer(cx);
            cx.notify();
            return;
        }
        self.disconnected = true;
        if self.thread.last_error.is_none() {
            self.thread.last_error = Some("Agent process exited.".into());
        }
        self.sync_composer(cx);
        cx.notify();
    }

    fn toggle_thinking(&mut self, idx: usize, cx: &mut Context<Self>) {
        // `insert` returns false when already present → toggle off.
        if !self.expanded_thinking.insert(idx) {
            self.expanded_thinking.remove(&idx);
        }
        cx.notify();
    }

    fn toggle_tool_expanded(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.expanded_tool_calls.insert(id.clone()) {
            self.expanded_tool_calls.remove(&id);
        }
        cx.notify();
    }

    /// Answer a pending tool permission from a card button. Routes the decision
    /// to the connection by `request_id`, then transitions the local status so
    /// the card updates immediately: Allow → `InProgress` (the tool proceeds and
    /// the later `ToolResult` finalizes it); Deny → `Rejected`.
    fn resolve_permission(
        &mut self,
        tool_id: String,
        request_id: String,
        decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) {
        // Idempotency guard: only answer a tool that is STILL awaiting. Once
        // answered its status leaves `WaitingForConfirmation` (below) and the
        // buttons drop on re-render, but this closes the sub-frame window where
        // a stray second click could send a second control_response for an
        // already-decided request_id.
        let still_awaiting = self.thread.entries.iter().any(|e| {
            matches!(e, ThreadEntry::ToolCall(tc)
                if tc.id == tool_id
                    && matches!(&tc.status,
                        ToolCallStatus::WaitingForConfirmation(r) if r.request_id == request_id))
        });
        if !still_awaiting {
            return;
        }
        if let Some(conn) = &self.connection {
            let _ = conn.resolve_permission(&request_id, decision.clone());
        }
        let status = match &decision {
            PermissionDecision::Deny { .. } => ToolCallStatus::Rejected,
            PermissionDecision::Allow { .. } | PermissionDecision::AllowWithSuggestion { .. } => {
                ToolCallStatus::InProgress
            }
        };
        self.thread.set_tool_status(&tool_id, status);
        cx.notify();
    }

    /// The scrollable transcript column. Entries stack in a centered reading
    /// column ([`CONTENT_MAX_W`]) so wide windows don't stretch text edge-to-
    /// edge; the outer element only scrolls and centers.
    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let density = self.density;
        let typo = self.typography.clone();
        let scroll = div()
            .id("agent-chat-list")
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .flex_1()
            // `min_h(0)` is essential: a flex child defaults to `min-height:auto`
            // (= content height), so without this the transcript grows to its
            // content size instead of shrinking to the flex-allocated space —
            // its scroll box then extends past the composer and the true bottom
            // (the newest message / approval row) is never reachable, no matter
            // the scroll offset. Pinning min-height to 0 lets it shrink so
            // `overflow_y_scroll` actually bounds the box to the visible area.
            .min_h(px(0.0))
            .px(px(density.pad_panel))
            .py(px(density.pad_panel))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            // Release auto-follow when the user scrolls UP to read history (so a
            // streaming turn doesn't yank them back down); re-arm once they
            // return to the bottom. gpui's scroll offset grows more negative as
            // you scroll down, so a positive wheel delta means "toward the top".
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _window, cx| {
                let dy = ev.delta.pixel_delta(px(20.0)).y;
                let was = this.stick_to_bottom;
                if dy > px(0.0) {
                    this.stick_to_bottom = false;
                } else if this.is_near_bottom() {
                    this.stick_to_bottom = true;
                }
                if this.stick_to_bottom != was {
                    cx.notify();
                }
            }));

        if self.thread.entries.is_empty() {
            // Even the empty state rides the scroll box + overlay so the layout
            // is identical once messages arrive; the scrollbar auto-hides when
            // content fits.
            return self
                .wrap_scroll(scroll.child(self.render_empty_hint(&theme, &typo)))
                .into_any_element();
        }

        // Turns breathe a little more than inline content — a chat rhythm.
        let mut content = div()
            .flex()
            .flex_col()
            .w_full()
            .max_w(px(CONTENT_MAX_W))
            .gap(px(density.pad_panel * 2.0));

        for (idx, entry) in self.thread.entries.iter().enumerate() {
            match entry {
                ThreadEntry::User(text) => {
                    // No "You" caption — the right-aligned bubble is the signal.
                    content = content.child(bubble::user_body(text, theme, density, &typo));
                }
                ThreadEntry::Assistant(msg) => {
                    if msg.is_empty() {
                        continue;
                    }
                    let group = SharedString::from(format!("chat-asst-{idx}"));
                    let mut block = div()
                        .group(group.clone())
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .w_full()
                        .child(assistant_header(group, &msg.text, theme, &typo, cx));
                    if !msg.thinking.is_empty() {
                        let expanded = self.expanded_thinking.contains(&idx);
                        block = block.child(thinking_block(
                            idx, expanded, &msg.thinking, theme, density, &typo, cx,
                        ));
                    }
                    if !msg.text.is_empty() {
                        block = block.child(bubble::assistant_body(idx, &msg.text, &typo));
                    }
                    content = content.child(block);
                }
                ThreadEntry::ToolCall(tc) => {
                    let expanded = self.expanded_tool_calls.contains(&tc.id);
                    content = content.child(tool_card::render_tool_card(
                        tc, expanded, theme, density, &typo, cx,
                    ));
                }
            }
        }
        // Live turn / disconnect state lives at the tail of the transcript (like
        // a native chat), NOT above the composer — so it never resizes the input.
        if self.disconnected {
            content = content.child(
                div()
                    .w_full()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("Agent process exited.")),
            );
        } else if self.thread.turn_active {
            content = content.child(working_indicator(theme, &typo));
        }
        // Trailing clearance INSIDE the scrollable content, above the composer.
        // `scroll_to_bottom` pins the offset to gpui's `scroll_max`, which is
        // derived from the content height sampled at layout — but a rich-text /
        // markdown row's final painted height settles a hair TALLER than that
        // sample, so the pinned offset stops a few pixels short of the true
        // bottom and the newest row's descenders (or an Allow/Reject row) tuck
        // under the composer. A generous scrollable trailing gap absorbs that
        // fixed shortfall: it's real content (folded into `scroll_max`), so the
        // last real row always lands clear of the composer with breathing room.
        content = content.child(div().flex_none().w_full().h(px(density.pad_panel * 3.0)));
        self.wrap_scroll(scroll.child(content)).into_any_element()
    }

    /// Wrap the scrolling transcript box in a positioned container and overlay a
    /// fading scrollbar bound to the SAME [`ScrollHandle`]. The bar paints on the
    /// container's right edge, auto-hides when the content fits, and — being a
    /// `Normal` hitbox gated to its own 16px strip — never blocks clicks on the
    /// messages, tool cards, or Allow/Reject rows beneath it.
    fn wrap_scroll(&self, scroll_box: impl IntoElement) -> gpui::Div {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(scroll_box)
            .child(Scrollbar::vertical(&self.list_scroll))
    }

    fn render_empty_hint(&self, theme: &Theme, typo: &Typography) -> AnyElement {
        // Disconnected → surface the error plainly. Otherwise a calm, centered
        // greeting (title + hint) rather than a lone sentence.
        let (title, subtitle, title_color) = if self.disconnected {
            (
                "Agent unavailable",
                self.thread.last_error.as_deref().unwrap_or("The agent process exited."),
                theme.status_error,
            )
        } else {
            (
                "Start a conversation",
                "Ask Claude to explain code, make edits, or run commands.",
                theme.fg_muted,
            )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .w_full()
            .child(
                div()
                    .text_size(px(typo.t_body_lg))
                    .text_color(title_color)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(subtitle.to_string())),
            )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // Keyboard focus must live on the composer, not this view's root. The
        // pane focuses the composer on open, but an inline focus during action/
        // click dispatch is clobbered onto the root's tracked handle — so
        // keystrokes hit the root, the composer stays empty, and ⌘↵ never
        // dispatches the field's Enter action. If the root holds focus, hand it
        // to the composer (deferred so it wins the post-dispatch focus race).
        // Self-limiting: once the composer is focused the root no longer is.
        if self.focus_handle.is_focused(window) {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.read(cx).focus_handle(cx).focus(window, cx);
            });
        }
        // Re-pin to the bottom every frame while following. Re-asserting each
        // render (not just once per event) keeps the newest row glued as its
        // height settles a frame after it arrives (markdown/diff measuring) and
        // through the end of the turn — a single per-event scroll lands short in
        // that case. Released when the user scrolls up (see the wheel handler on
        // the transcript). `scroll_to_bottom` only sets a flag consumed at paint,
        // so this is cheap.
        if self.stick_to_bottom {
            self.list_scroll.scroll_to_bottom();
        }
        let transcript = self.render_transcript(cx);
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
            .capture_action(cx.listener(|this, _action: &InputEnter, window, cx| {
                // Both ↵ and ⌘↵ send. The field's key map collapses Enter and
                // Shift+Enter into the same action, so a keyboard newline can't
                // be distinguished here — multi-line prompts arrive via paste.
                // The mouse Send button remains the IME-proof fallback. The
                // composer emits `Submit`, which this view's subscription sends.
                this.composer.update(cx, |c, cx| c.submit(window, cx));
            }))
            .child(transcript)
            .child(self.composer.clone())
    }
}

/// A live "Claude is working…" row shown at the tail of the transcript while a
/// turn streams — a stepped rotating spinner (the reused rail cadence: 12
/// mechanical ticks/sec) plus muted text. Keeping it here rather than above the
/// composer means the input never resizes when a turn starts or ends.
fn working_indicator(theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .w_full()
        .child(
            Icon::default()
                .path("icons/loader-circle.svg")
                .size(px(13.0))
                .text_color(theme.fg_muted)
                .with_animation(
                    SharedString::from("chat-working-spinner"),
                    Animation::new(Duration::from_secs(1)).repeat(),
                    |icon, delta| {
                        let stepped = (delta * 12.0).floor() / 12.0;
                        icon.transform(Transformation::rotate(percentage(stepped)))
                    },
                ),
        )
        .child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Claude is working…")),
        )
        .into_any_element()
}

/// The assistant caption row: the "Claude" label on the left and a Copy
/// affordance on the right that's revealed while the message block is hovered
/// (`group`) — the copy-on-hover pattern of a native chat. Clicking copies the
/// reply's raw markdown to the clipboard. Built here (not `bubble`) because the
/// click needs a `Context` listener.
fn assistant_header(
    group: SharedString,
    text: &str,
    theme: Theme,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let copy_text = text.to_string();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .child(bubble::role_caption("Claude", theme.fg_muted, typo))
        .child(
            div()
                .id(SharedString::from(format!("copy-{group}")))
                .flex_none()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .cursor_pointer()
                // Reserve its slot (invisible, not absent) so the caption never
                // shifts; reveal on hover of the surrounding message block.
                .invisible()
                .group_hover(group, |s| s.visible())
                .hover(|s| s.text_color(theme.fg_base))
                .on_click(cx.listener(move |_this, _e, _w, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                }))
                .child(SharedString::from("Copy")),
        )
        .into_any_element()
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

    /// Card buttons route Allow/Reject to the connection by request_id and flip
    /// the local status (Allow → InProgress; Deny → Rejected), clearing the
    /// pending prompt. Allow echoes the tool input as updatedInput.
    #[gpui::test]
    async fn approve_and_reject_route_permission_decisions(cx: &mut TestAppContext) {
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
                view.thread.push_user_message("do two things");
                for (tid, rid, name, input) in [
                    ("t1", "r1", "Edit", json!({"file_path": "a.txt"})),
                    ("t2", "r2", "Bash", json!({"command": "rm x"})),
                ] {
                    view.thread.apply(&ThreadEvent::ToolCallStarted {
                        id: tid.into(),
                        name: name.into(),
                        input: input.clone(),
                    });
                    view.thread.apply(&ThreadEvent::PermissionRequested {
                        request_id: rid.into(),
                        tool_use_id: Some(tid.into()),
                        tool_name: name.into(),
                        input,
                        description: name.into(),
                        suggestions: vec![],
                    });
                }

                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({"file_path": "a.txt"}) },
                    cx,
                );
                view.resolve_permission(
                    "t2".into(),
                    "r2".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );

                assert!(
                    view.thread.pending_permission().is_none(),
                    "both permissions resolved"
                );
                assert_eq!(tool_status(view, "t1"), Some("InProgress"));
                assert_eq!(tool_status(view, "t2"), Some("Rejected"));
            })
            .expect("window update");

        let sent = stub_probe.sent();
        let allow = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r1")
            .expect("r1 control_response");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(
            allow["response"]["response"]["updatedInput"],
            json!({"file_path": "a.txt"})
        );
        let deny = sent
            .iter()
            .find(|s| s["response"]["request_id"] == "r2")
            .expect("r2 control_response");
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
    }

    /// A stray second click after a card is answered must not send a second
    /// control_response or flip the decision — the guard makes it a no-op.
    #[gpui::test]
    async fn second_answer_is_ignored(cx: &mut TestAppContext) {
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
                view.thread.push_user_message("go");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                });
                // First answer: allow.
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Allow { updated_input: json!({}) },
                    cx,
                );
                // Stray second answer: deny — must be ignored (already decided).
                view.resolve_permission(
                    "t1".into(),
                    "r1".into(),
                    PermissionDecision::Deny { message: "no".into() },
                    cx,
                );
                assert_eq!(
                    tool_status(view, "t1"),
                    Some("InProgress"),
                    "stays allowed, not flipped to Rejected by the second click"
                );
            })
            .expect("window update");

        let responses: Vec<_> = stub_probe
            .sent()
            .into_iter()
            .filter(|s| s["response"]["request_id"] == "r1")
            .collect();
        assert_eq!(responses.len(), 1, "exactly one control_response for r1");
        assert_eq!(responses[0]["response"]["response"]["behavior"], "allow");
    }

    /// Stop mid-turn: the turn clears, a pending approval fail-closes, and the
    /// tab enters resumable-idle (interrupted, NOT disconnected — no error).
    #[gpui::test]
    async fn stop_turn_interrupts_and_stays_resumable(cx: &mut TestAppContext) {
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
                view.thread.push_user_message("do a long thing");
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "x".into(),
                    suggestions: vec![],
                });
                assert!(view.thread.turn_active, "turn active before Stop");

                view.stop_turn(cx);

                assert!(!view.thread.turn_active, "Stop ends the turn");
                assert!(view.interrupted, "session marked resumable-idle");
                assert!(!view.disconnected, "an intentional Stop is not a disconnect");
                assert!(
                    view.thread.pending_permission().is_none(),
                    "pending approval fail-closes on Stop"
                );
                assert_eq!(tool_status(view, "t1"), Some("Rejected"));

                // The interrupt `result` arrives flagged as an error; it must be
                // swallowed, not shown as a banner.
                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: None,
                        cost_usd: None,
                        is_error: true,
                    },
                    cx,
                );
                assert!(
                    view.thread.last_error.is_none(),
                    "the interrupt's error result is suppressed"
                );

                // The child's stdout then EOFs: still resumable, still no error.
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after an intentional Stop stays resumable");
                assert!(view.interrupted);
                assert!(view.thread.last_error.is_none());
            })
            .expect("window update");
    }

    /// Order-independence: if the child's stdout EOF is observed BEFORE the
    /// interrupt's `result` event, the tab must still stay resumable-idle (not
    /// flip to disconnected/unavailable), and a straggler error result arriving
    /// afterward is still suppressed.
    #[gpui::test]
    async fn stop_then_eof_before_result_stays_resumable(cx: &mut TestAppContext) {
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
                view.thread.push_user_message("go");
                view.stop_turn(cx);
                assert!(view.interrupted);

                // EOF arrives first (before any TurnEnded is folded in).
                view.on_disconnect(cx);
                assert!(!view.disconnected, "EOF after Stop stays resumable, order-independent");
                assert!(view.thread.last_error.is_none());

                // A late error result then folds in — still suppressed.
                view.on_event(
                    ThreadEvent::TurnEnded { result: None, cost_usd: None, is_error: true },
                    cx,
                );
                assert!(view.thread.last_error.is_none());
                assert!(view.interrupted, "still resumable for the next send");
            })
            .expect("window update");
    }

    /// A Stop with no live turn is a no-op (nothing to interrupt).
    #[gpui::test]
    async fn stop_turn_without_active_turn_is_noop(cx: &mut TestAppContext) {
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
                assert!(!view.thread.turn_active);
                view.stop_turn(cx);
                assert!(!view.interrupted, "no turn → Stop does nothing");
            })
            .expect("window update");
    }

    fn tool_status(view: &AgentChatView, id: &str) -> Option<&'static str> {
        view.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(match tc.status {
                ToolCallStatus::InProgress => "InProgress",
                ToolCallStatus::Rejected => "Rejected",
                ToolCallStatus::Completed => "Completed",
                ToolCallStatus::WaitingForConfirmation(_) => "WaitingForConfirmation",
                _ => "Other",
            }),
            _ => None,
        })
    }
}
