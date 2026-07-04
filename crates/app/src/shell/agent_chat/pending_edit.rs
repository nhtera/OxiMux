//! Edit-and-resend as a STAGED operation.
//!
//! Clicking ✎ on a prior user message enters "pending edit" mode: the composer
//! is prefilled with that message's text + images, later messages are dimmed,
//! and a banner discloses what sending will do. **Nothing is destroyed yet** —
//! Escape (or clearing the banner) is a true no-op that restores the prior
//! draft. Only on send does the rewind actually run (fork the session, drop the
//! later turns) and the edited text go out into the forked session.
//!
//! This deliberately avoids the eager-truncate footgun: a misclick + Escape
//! must never silently lose conversation history.

use gpui::prelude::FluentBuilder;
use gpui::{div, px, Context, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window};
use oximux_agents::thread::{ChatImage, ThreadEntry};

use super::AgentChatView;

/// A staged edit-and-resend. In-memory only — dropped on tab close / restart.
pub struct PendingEdit {
    /// Ordinal among USER entries (what the rewind/fork operate on).
    pub ordinal: usize,
    /// Index into `thread.entries` of the message being edited.
    pub entry_index: usize,
    /// The composer draft that was in progress when ✎ was clicked, restored
    /// verbatim on cancel so a stray edit-click never eats work.
    pub saved_draft: String,
    /// Attachments staged in the composer when ✎ was clicked, restored on
    /// cancel so entering edit mode is a true no-op for images too.
    pub saved_images: Vec<ChatImage>,
    /// Snapshot sha for the optional files axis (`Some` only when `checkpoint.show`).
    pub sha: Option<String>,
    /// The banner's "also restore files" toggle.
    pub include_files: bool,
    /// How many messages (this one + later) will be removed on send.
    pub messages_removed: usize,
}

impl AgentChatView {
    /// Enter staged-edit mode for the user message at `entry_index`.
    pub(super) fn enter_pending_edit(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only while the turn is idle: a live turn would make the composer's
        // Submit QUEUE the edited text (composer.submit's turn_active branch)
        // instead of routing it to `send_pending_edit`. Requiring idle keeps
        // "capture happens at send" honest and avoids the queue interleave
        // (Stop first, then edit — like the rest of the resumable-idle model).
        if self.rewinding || self.thread.turn_active || self.thread.session_id.is_none() {
            return;
        }
        let Some(ordinal) = self.user_ordinal_at(entry_index) else { return };
        let (text, images) = match self.thread.entries.get(entry_index) {
            Some(ThreadEntry::User { text, images, .. }) => (text.clone(), images.clone()),
            _ => return,
        };
        let sha = match self.thread.entries.get(entry_index) {
            Some(ThreadEntry::User { checkpoint: Some(cp), .. }) if cp.show => Some(cp.sha.clone()),
            _ => None,
        };
        // Any open rewind-confirm is superseded by entering edit mode.
        self.rewind_confirm = None;
        let (saved_draft, saved_images) = {
            let c = self.composer.read(cx);
            (c.current_draft(cx), c.current_images())
        };
        self.pending_edit = Some(PendingEdit {
            ordinal,
            entry_index,
            saved_draft,
            saved_images,
            sha,
            include_files: false,
            messages_removed: self.thread.entries.len() - entry_index,
        });
        self.composer
            .update(cx, |c, cx| c.prefill(text, images, window, cx));
        cx.notify();
    }

    /// Cancel staged edit: restore the prior draft, undim, drop state. A true
    /// no-op on the transcript / session / disk.
    pub(super) fn cancel_pending_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pe) = self.pending_edit.take() else { return };
        self.composer
            .update(cx, |c, cx| c.prefill(pe.saved_draft, pe.saved_images, window, cx));
        cx.notify();
    }

    /// Whether transcript entry `idx` should render dimmed (a later message that
    /// the staged edit will remove on send).
    pub(super) fn is_pending_edit_dimmed(&self, idx: usize) -> bool {
        self.pending_edit
            .as_ref()
            .is_some_and(|pe| idx > pe.entry_index)
    }

    /// Send the staged edit: run the rewind (conversation ± files), then the
    /// edited `text`/`images` go out into the forked session (wired via
    /// `rewind_then_send`). Called from the composer's Submit when a pending
    /// edit is active.
    pub(super) fn send_pending_edit(
        &mut self,
        text: String,
        images: Vec<ChatImage>,
        cx: &mut Context<Self>,
    ) {
        // The composer already trims + rejects an empty-and-imageless submit
        // before emitting, so this only runs with real content — but guard
        // BEFORE consuming `pending_edit` so a stray empty call leaves the
        // staged edit intact rather than stranding a dimmed transcript.
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        let Some(pe) = self.pending_edit.take() else {
            // Shouldn't happen (the caller checks), but fall back to a normal send.
            self.send_text(text, images, cx);
            return;
        };
        let sha_for_restore = pe.include_files.then_some(pe.sha).flatten();
        self.rewind_then_send = Some((text, images));
        self.perform_rewind(pe.ordinal, pe.entry_index, sha_for_restore, cx);
    }

    /// The staged-edit banner, rendered above the composer while active.
    pub(super) fn render_pending_edit_banner(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let pe = self.pending_edit.as_ref()?;
        let t = &self.theme;
        let files_offerable = pe.sha.is_some();
        let include_files = pe.include_files;
        let removed = pe.messages_removed.saturating_sub(1);
        let subtitle: SharedString = if removed == 0 {
            "Send to resend · nothing removed until you send".into()
        } else {
            format!(
                "Send removes {removed} later message{} · nothing removed until you send",
                if removed == 1 { "" } else { "s" }
            )
            .into()
        };
        // Faint accent wash for the leading pencil badge — same hue as the
        // left stripe so the "you are editing" cue reads as one accent, not two.
        let accent_soft = Hsla { a: 0.14, ..t.focus_ring };

        Some(
            // Full-width row that centers a width-capped card, so the banner reads
            // as a contained notification above the composer rather than a
            // full-bleed strip that strands the Cancel button far to the right.
            div().flex().w_full().justify_center().child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .max_w(px(560.0))
                .gap(px(10.0))
                .pl(px(11.0))
                .pr(px(9.0))
                .py(px(8.0))
                .rounded(px(9.0))
                .bg(t.bg_panel_alt)
                .border_1()
                .border_color(t.border_inactive)
                // Leading pencil badge — a small accent-tinted chip that marks the
                // transient "edit in progress" mode (gpui borders are single-color,
                // so the accent lives here rather than as a stripe).
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(24.0))
                        .rounded(px(6.0))
                        .bg(accent_soft)
                        .text_sm()
                        .text_color(t.fg_base)
                        .child("✎"),
                )
                // Title + one-line disclosure, stacked tight.
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(1.0))
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(t.fg_base)
                                .child("Editing message"),
                        )
                        .child(div().text_xs().text_color(t.fg_muted).child(subtitle)),
                )
                // Optional "restore files too" toggle — a compact inline pill so
                // it doesn't force a second banner row.
                .when(files_offerable, |el| {
                    el.child(
                        div()
                            .id("edit-files-toggle")
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(if include_files { t.fg_base } else { t.fg_muted })
                            .when(include_files, |s| s.bg(accent_soft))
                            .hover(|s| s.bg(t.hover_overlay))
                            .child(if include_files { "☑" } else { "☐" })
                            .child("Files")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(pe) = &mut this.pending_edit {
                                    pe.include_files = !pe.include_files;
                                    cx.notify();
                                }
                            })),
                    )
                })
                // Cancel — a real ghost button, not loose text.
                .child(
                    div()
                        .id("edit-cancel")
                        .flex_shrink_0()
                        .px(px(10.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .text_xs()
                        .text_color(t.fg_muted)
                        .border_1()
                        .border_color(t.border_inactive)
                        .hover(|s| s.bg(t.hover_overlay).text_color(t.fg_base))
                        .child("Cancel")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_pending_edit(window, cx)
                        })),
                ),
            ),
        )
    }
}
