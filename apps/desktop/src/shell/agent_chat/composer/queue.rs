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

/// A message that was queued when the app last quit. It comes back **held**:
/// nothing sends it but the user's own Send / Send all (not a completed turn,
/// not a new message), because a restart is exactly when nobody has reviewed
/// what was waiting. Text only, like the queue it was saved from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldMessage {
    pub text: String,
    /// Its send was written but never confirmed before the quit, so the agent
    /// may already have it.
    pub maybe_sent: bool,
}

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
        self.queue_changed(cx);
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
        self.queue_changed(cx);
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
        self.queue_changed(cx);
    }

    /// Cancel a parked message (its chip's ✕).
    pub(super) fn cancel_queued(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.queued.len() {
            self.queued.remove(idx);
            self.queue_changed(cx);
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
        self.queue_changed(cx);
        if steering {
            cx.emit(ComposerEvent::SteerNow { text: wire });
        } else {
            cx.emit(ComposerEvent::Submit { text: wire, images });
        }
    }

    /// The text of each queued message, oldest first, for persistence: the
    /// held ones (queued before the last restart) ahead of the open queue.
    /// Text-only (staged images/context aren't persisted); an image-only queued
    /// message (blank text) is skipped since it can't be faithfully restored.
    pub fn queued_texts(&self) -> Vec<String> {
        let open = self.queued.iter().map(|m| m.text.clone()).filter(|t| !t.trim().is_empty());
        self.held.iter().map(|m| m.text.clone()).chain(open).collect()
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
        self.queue_changed(cx);
    }

    /// Restore the messages queued when the app last quit, **held** (see
    /// [`HeldMessage`]).
    pub fn seed_held(&mut self, entries: Vec<HeldMessage>, cx: &mut Context<Self>) {
        for entry in entries {
            if self.held.len() >= MAX_QUEUED {
                break;
            }
            if !entry.text.trim().is_empty() {
                self.held.push(entry);
            }
        }
        self.queue_changed(cx);
    }

    /// The send written but never answered before the quit: held first (it
    /// went before anything queued behind it) and marked "may already have been
    /// sent".
    pub fn hold_unconfirmed(&mut self, text: String, cx: &mut Context<Self>) {
        if !text.trim().is_empty() {
            self.held.insert(0, HeldMessage { text, maybe_sent: true });
            self.held.truncate(MAX_QUEUED);
            self.queue_changed(cx);
        }
    }

    /// Release held message `idx`: sent at once when idle, else moved to the
    /// front of the open queue so it goes when the turn ends.
    pub(super) fn send_held(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.disconnected || idx >= self.held.len() {
            return;
        }
        let entry = self.held.remove(idx);
        self.release_held(vec![entry], cx);
    }

    /// Release every held message, oldest first: the first is sent when idle,
    /// the rest go to the front of the open queue in order.
    pub(super) fn send_all_held(&mut self, cx: &mut Context<Self>) {
        if self.disconnected || self.held.is_empty() {
            return;
        }
        let entries = std::mem::take(&mut self.held);
        self.release_held(entries, cx);
    }

    fn release_held(&mut self, entries: Vec<HeldMessage>, cx: &mut Context<Self>) {
        let mut texts = entries.into_iter().map(|e| e.text);
        let now = if self.turn_active { None } else { texts.next() };
        let parked: Vec<QueuedMessage> = texts
            .map(|text| QueuedMessage { text, images: Vec::new(), context: Vec::new() })
            .collect();
        self.queued.splice(0..0, parked);
        self.queued.truncate(MAX_QUEUED);
        self.queue_changed(cx);
        if let Some(text) = now {
            self.history.record(&text);
            cx.emit(ComposerEvent::Submit { text, images: Vec::new() });
        }
    }

    /// Drop held message `idx` (its card's Drop).
    pub(super) fn drop_held(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.held.len() {
            self.held.remove(idx);
            self.queue_changed(cx);
        }
    }

    /// Drop every held message (the strip's Discard).
    pub(super) fn discard_held(&mut self, cx: &mut Context<Self>) {
        if !self.held.is_empty() {
            self.held.clear();
            self.queue_changed(cx);
        }
    }

    /// Why the open queue is not draining, shown on its chips instead of
    /// "Queued" (the parent's call: a turn that failed holds the queue).
    pub fn set_queue_note(&mut self, note: Option<&'static str>, cx: &mut Context<Self>) {
        if self.queue_note != note {
            self.queue_note = note;
            cx.notify();
        }
    }

    /// The queue changed: repaint and tell the parent the counts.
    pub(super) fn queue_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        cx.emit(ComposerEvent::QueueChanged { held: self.held.len() as u32, queued: self.queued.len() as u32 });
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
                        .child(SharedString::from(self.queue_note.unwrap_or("Queued"))),
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

    /// The held-queue strip: what was queued before the restart, held until the
    /// user sends or drops it. One card per message with [Send] [Drop]; the
    /// header carries [Send all] [Discard]. The buttons sit outside the flex
    /// flow of the text so a narrow column wraps the text, not the buttons.
    pub(super) fn render_held(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, density, typo) = (self.theme, self.density, &self.typography);
        let button = |id, label| super::strips::strip_button(self, id, label);
        let n = self.held.len();
        let title = format!(
            "{n} message{} queued before restart {} held (text only).",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" },
        );
        let header = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(density.gap_inline))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_base)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(4.0))
                    .child(
                        button(("chat-held-send-all", 0), "Send all")
                            .on_click(cx.listener(|this, _e, _w, cx| this.send_all_held(cx))),
                    )
                    .child(
                        button(("chat-held-discard", 0), "Discard")
                            .on_click(cx.listener(|this, _e, _w, cx| this.discard_held(cx))),
                    ),
            );
        let mut strip = super::strips::strip_frame(self)
            .child(header);
        for (idx, m) in self.held.iter().enumerate() {
            let first_line = m.text.lines().next().unwrap_or_default().to_string();
            strip = strip.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .gap(px(density.gap_inline))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.0))
                            .text_size(px(typo.t_body_sm))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.fg_muted)
                                    .child(SharedString::from(format!("▸ {first_line}"))),
                            )
                            .when(m.maybe_sent, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_color(theme.status_warning)
                                        .child(SharedString::from("may already have been sent")),
                                )
                            }),
                    )
                    .child(
                        button(("chat-held-send", idx), "Send")
                            .on_click(cx.listener(move |this, _e, _w, cx| this.send_held(idx, cx))),
                    )
                    .child(
                        button(("chat-held-drop", idx), "Drop")
                            .on_click(cx.listener(move |this, _e, _w, cx| this.drop_held(idx, cx))),
                    ),
            );
        }
        strip
    }
}
