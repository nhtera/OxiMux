//! Strips stacked above the composer's input: the recovery-note strip and the
//! prompt-history indicator (the held-queue strip lives with the queue).
//! Inherent on [`ComposerView`], like the composer's other child modules.

use super::*;

/// A ghost button for a strip above the input.
pub(super) fn strip_button(
    composer: &ComposerView,
    id: (&'static str, usize),
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    let (theme, density, typo) = (composer.theme, composer.density, &composer.typography);
    div()
        .id(id)
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .cursor_pointer()
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
        .hover(|s| s.bg(theme.bg_panel).text_color(theme.fg_base))
        .child(SharedString::from(label))
}

/// The frame every strip above the input shares: the in-chat inline-card idiom.
pub(super) fn strip_frame(composer: &ComposerView) -> gpui::Div {
    let (theme, density) = (composer.theme, composer.density);
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap(px(density.gap_inline * 0.5))
        .px(px(density.pad_panel))
        .py(px(density.gap_inline))
        .rounded(px(density.r_card))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
}

impl ComposerView {
    /// Show or hide the recovery-note strip (the parent owns the note).
    pub fn set_recovery_note(&mut self, pending: bool, cx: &mut Context<Self>) {
        if self.recovery_note != pending {
            self.recovery_note = pending;
            cx.notify();
        }
    }

    /// "Recovery note will be attached to your next message. [Remove]": the
    /// agent is told what the restart cut short, on a message the user sends.
    pub(super) fn render_recovery_note(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, density, typo) = (self.theme, self.density, &self.typography);
        strip_frame(self).child(
            div()
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
                        .child(SharedString::from("Recovery note will be attached to your next message.")),
                )
                .child(
                    strip_button(self, ("chat-recovery-remove", 0), "Remove")
                        .on_click(cx.listener(|_, _e, _w, cx| cx.emit(ComposerEvent::RemoveRecoveryNote))),
                ),
        )
    }

    /// A muted "History i/N" strip shown above the pill while walking prompt
    /// history — echoing a shell's recall indicator so ↑/↓ reads as browsing
    /// sent prompts, not moving the caret. `None` when not navigating.
    pub(super) fn render_history_indicator(&self) -> Option<impl IntoElement> {
        let (pos, total) = self.history.position()?;
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .px(px(density.pad_panel))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(format!("History {pos}/{total}"))),
                ),
        )
    }
}
