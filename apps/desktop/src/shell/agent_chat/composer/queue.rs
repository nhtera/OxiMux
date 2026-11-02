//! The composer's parked-message queue: messages submitted while a turn
//! streams, drained one per completed turn by the parent view.
//!
//! Lifted out of `composer.rs` so that file can ratchet back under the size cap.
//! The methods stay inherent on [`ComposerView`]; a child module sees its
//! parent's private items, so the queue still reads the composer's own state.

use super::*;

/// A message the user submitted while a turn was still streaming: parked here
/// and sent (in order) as each turn completes, so they can line up follow-ups
/// without waiting. Keeps the fully-staged [`PendingImage`]s (not just the wire
/// [`ChatImage`]) so pulling the message back to edit (↑) restores its
/// attachments without re-decoding; the wire form is extracted only on send.
pub(super) struct QueuedMessage {
    pub(super) text: String,
    pub(super) images: Vec<PendingImage>,
    /// Context chips staged with this message. Held structured (not serialized)
    /// so pulling the message back to edit (↑) restores the chips, mirroring
    /// `images`; the `<context>` blocks are rendered onto the wire only at drain.
    pub(super) context: Vec<ContextChip>,
}

/// Upper bound on parked messages, so a stuck turn can't let the queue grow
/// without bound. Generous — a user rarely lines up more than a few.
pub(super) const MAX_QUEUED: usize = 20;

impl ComposerView {
    /// Pop the oldest queued message for the parent to send, recording it in the
    /// prompt history as it goes out (it's now a sent prompt). Returns `None` when
    /// nothing is queued. Repaints so the drained chip disappears.
    pub fn take_next_queued(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(String, Vec<ChatImage>)> {
        if self.queued.is_empty() {
            return None;
        }
        let QueuedMessage { text, images, context } = self.queued.remove(0);
        self.history.record(&text);
        cx.notify();
        let images: Vec<ChatImage> = images.into_iter().map(|p| p.chat).collect();
        let wire = prepend_context(&context, &text);
        Some((wire, images))
    }

    /// Pull the most-recently parked message back into the composer to edit it
    /// (↑ with an empty draft while messages are queued — the Claude-Desktop
    /// "arrow up to edit" gesture). Removes it from the queue and restores its
    /// staged attachments; re-submitting re-queues it (or sends it if the turn
    /// has since ended). Returns whether it consumed the key. Only fires with an
    /// empty draft and no staged attachments, so it never clobbers work in
    /// progress.
    pub(super) fn edit_last_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.queued.is_empty() {
            return false;
        }
        if !self.draft_is_empty(cx) {
            return false;
        }
        let QueuedMessage { text, images, context } =
            self.queued.pop().expect("non-empty checked above");
        self.pending_images = images;
        self.context_chips = context;
        self.set_draft_end(text, window, cx);
        cx.notify();
        true
    }

    /// Pop a specific queued message (its chip's ✎) back into the composer to
    /// edit. Guarded on an empty draft + no staged images — mirrors
    /// [`Self::edit_last_queued`] so it never silently clobbers work in
    /// progress; the ✎ button is hidden when the draft is non-empty, this is
    /// the defense-in-depth backstop.
    pub(super) fn edit_queued(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.queued.len() {
            return;
        }
        if !self.draft_is_empty(cx) {
            return;
        }
        let QueuedMessage { text, images, context } = self.queued.remove(idx);
        self.pending_images = images;
        self.context_chips = context;
        self.set_draft_end(text, window, cx);
        cx.notify();
    }

    /// Cancel a parked message (its chip's ✕).
    pub(super) fn cancel_queued(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.queued.len() {
            self.queued.remove(idx);
            cx.notify();
        }
    }

    /// A queued chip's "send now" (↑).
    ///
    /// While IDLE, dequeue and submit right away — exactly what the user asked.
    ///
    /// Mid-turn it depends on the backend. A [`Self::can_steer`] one takes the
    /// message into the running turn and redirects the agent, so "now" is
    /// honoured literally ([`ComposerEvent::SteerNow`]). Otherwise there is
    /// nothing to hand it to, so the message only moves to the FRONT of the queue
    /// and goes out first when the turn ends (a no-op if already there).
    ///
    /// Steering is deliberately reachable ONLY here, never from `submit`: pi
    /// cannot un-queue a steered message, so handing one over must be an explicit
    /// act. A message parked as a chip stays editable and cancellable until the
    /// user presses this button.
    ///
    /// A message with attachments never steers — pi's `steer` does carry images,
    /// but OxiMux has never sent one and won't find out at the cost of silently
    /// dropping the user's. It reorders instead, keeping the images intact for
    /// the ordinary drain.
    pub(super) fn send_queued_now(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.queued.len() {
            return;
        }
        if self.turn_active && !(self.can_steer && self.queued[idx].images.is_empty()) {
            if idx != 0 {
                let m = self.queued.remove(idx);
                self.queued.insert(0, m);
                cx.notify();
            }
            return;
        }
        let steering = self.turn_active;
        let QueuedMessage { text, images, context } = self.queued.remove(idx);
        self.history.record(&text);
        let images: Vec<ChatImage> = images.into_iter().map(|p| p.chat).collect();
        let wire = prepend_context(&context, &text);
        cx.notify();
        if steering {
            cx.emit(ComposerEvent::SteerNow { text: wire });
        } else {
            cx.emit(ComposerEvent::Submit { text: wire, images });
        }
    }

    /// The text of each queued message, oldest first, for persistence. Text-only
    /// (staged images/context aren't persisted); an image-only queued message
    /// (blank text) is skipped since it can't be faithfully restored.
    pub fn queued_texts(&self) -> Vec<String> {
        self.queued.iter().map(|m| m.text.clone()).filter(|t| !t.trim().is_empty()).collect()
    }

    /// Re-seed queued chips from restored text (text-only, no images/context).
    /// Never auto-sends — a restored app must not fire billed sends without a user
    /// action; the chips sit visible with their edit/cancel/send-now affordances.
    pub fn seed_queued(&mut self, texts: Vec<String>, cx: &mut Context<Self>) {
        for text in texts {
            if self.queued.len() >= MAX_QUEUED {
                break;
            }
            if text.trim().is_empty() {
                continue;
            }
            self.queued.push(QueuedMessage { text, images: Vec::new(), context: Vec::new() });
        }
        cx.notify();
    }

    /// Parked-message chips shown above the input while a turn streams: one
    /// compact row per queued message with a truncated preview and a ✕ to cancel
    /// it, so the user can see (and drop) what will send next. Rendered only when
    /// something is queued.
    pub(super) fn render_queued(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let mut col = div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(density.gap_inline * 0.5));
        // ✎ (pop-to-edit) is offered only when the draft is empty — editing a
        // chip replaces the composer contents, so it must not clobber a draft
        // in progress (matches the ↑-to-edit guard).
        let draft_empty = self.draft_is_empty(cx);
        let turn_active = self.turn_active;
        for (idx, m) in self.queued.iter().enumerate() {
            // Whether ↑ on THIS chip would reach the running turn (see
            // `send_queued_now`) — decided per chip, since an attachment keeps a
            // message out of the steer path even on a backend that steers.
            let steers = turn_active && self.can_steer && m.images.is_empty();
            // Prefer the caption; fall back to an image count for an image-only
            // queued message so its chip isn't blank.
            let preview = if !m.text.is_empty() {
                m.text.clone()
            } else {
                format!("{} image{}", m.images.len(), if m.images.len() == 1 { "" } else { "s" })
            };
            let row = div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .gap(px(density.gap_inline))
                .rounded(px(density.r_lg))
                .border_1()
                .border_color(theme.border_input)
                .bg(theme.bg_panel)
                .px(px(density.pad_panel))
                .py(px(density.gap_inline))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from("Queued")),
                )
                // Preview clips to one line — wrapped in this flex_row so a long
                // prompt truncates instead of rendering blank (the flex-col trap).
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(preview)),
                )
                // "Send now" (↑). Idle: dequeue + send immediately. Mid-turn on a
                // steering backend: hand it to the running turn — which is a real
                // action at every index, front one included. Mid-turn otherwise:
                // jump it to the front of the queue, so it's hidden at index 0
                // where that would do nothing.
                .when(!turn_active || steers || idx != 0, |row| {
                    row.child(
                        div()
                            .id(("chat-queued-send-now", idx))
                            .flex_none()
                            .size(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.fg_subtle)
                            .text_size(px(typo.t_body_sm))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.fg_base))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| this.send_queued_now(idx, cx)),
                            )
                            .child(SharedString::from("↑")),
                    )
                })
                .when(draft_empty, |row| {
                    row.child(
                        div()
                            .id(("chat-queued-edit", idx))
                            .flex_none()
                            .size(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.fg_subtle)
                            .text_size(px(typo.t_body_sm))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.fg_base))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.edit_queued(idx, window, cx)
                                }),
                            )
                            .child(SharedString::from("✎")),
                    )
                })
                .child(
                    div()
                        .id(("chat-queued-cancel", idx))
                        .flex_none()
                        .size(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_color(theme.fg_subtle)
                        .text_size(px(typo.t_body_sm))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.fg_base))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| this.cancel_queued(idx, cx)),
                        )
                        .child(SharedString::from("✕")),
                );
            col = col.child(row);
        }
        col
    }
}
