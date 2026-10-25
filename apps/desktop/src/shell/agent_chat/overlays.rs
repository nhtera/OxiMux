//! The chat's two full-window overlays: the image lightbox and the tool-call
//! payload sheet. They are mutually exclusive; opening one closes the other.

use super::*;

impl AgentChatView {
    /// Open the full-size lightbox on one message's image.
    pub(super) fn open_image_preview(&mut self, entry_idx: usize, img_idx: usize, cx: &mut Context<Self>) {
        self.preview = Some((entry_idx, img_idx));
        cx.notify();
    }

    /// Dismiss the lightbox (backdrop click or the ✕).
    pub(super) fn close_image_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    /// Open the fullscreen payload sheet on a tool call. The image lightbox and
    /// the sheet are mutually exclusive overlays — opening one closes the other.
    /// Focus moves to the view root so Escape dispatches from here (the overlay
    /// handlers' context) instead of being eaten by the composer input's IME —
    /// the same "focus the dialog on open" rule the find bar follows.
    pub(super) fn open_tool_sheet(&mut self, tool_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = None;
        self.sheet_copied = false;
        self.open_tool_sheet = Some(tool_id);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Dismiss the tool sheet (backdrop click, the ✕, or Escape) and return focus
    /// to the composer so typing resumes immediately (mirrors the find bar).
    pub(super) fn close_tool_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_tool_sheet.take().is_some() {
            self.sheet_copied = false;
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    /// Flash the sheet's Copy control to "Copied ✓" for a beat.
    pub(super) fn flash_sheet_copied(&mut self, cx: &mut Context<Self>) {
        self.sheet_copied = true;
        cx.notify();
        self._sheet_copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.sheet_copied {
                    view.sheet_copied = false;
                    cx.notify();
                }
            });
        }));
    }

    /// The live tool call backing the open sheet, looked up by id across the
    /// thread's tool calls each render (so a still-running tool grows in place).
    /// `None` if no sheet is open or the id is gone (e.g. after a rewind).
    pub(super) fn open_sheet_tool_call(&self) -> Option<&ToolCall> {
        let id = self.open_tool_sheet.as_deref()?;
        self.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    /// The fullscreen tool-payload sheet, rendered over everything when
    /// [`Self::open_tool_sheet`] names a still-present tool call.
    pub(super) fn render_tool_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tc = self.open_sheet_tool_call()?;
        Some(tool_sheet::render_tool_sheet(
            tc,
            self.sheet_copied,
            self.theme,
            self.density,
            &self.typography,
            cx,
        ))
    }

    /// Step within the CURRENT message's image group, wrapping at the ends.
    pub(super) fn step_image_preview(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some((entry, img)) = self.preview {
            let n = self.entry_images(entry).len();
            if n == 0 {
                return;
            }
            let next = (img as isize + delta).rem_euclid(n as isize) as usize;
            self.preview = Some((entry, next));
            cx.notify();
        }
    }

    /// The full-size image lightbox: a dimmed backdrop (click to dismiss) with
    /// the current image fit (aspect-preserved), a ‹ N of M › pager across the
    /// SAME message's images, and a ✕ — Claude-Desktop-style. Rendered over
    /// everything when [`Self::preview`] is set.
    pub(super) fn render_image_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (entry, img_idx) = self.preview?;
        let group = self.entry_images(entry);
        let i = img_idx.min(group.len().saturating_sub(1));
        let image = group.get(i)?.clone();
        let n = group.len();
        let theme = self.theme;
        let typo = &self.typography;

        // A circular control glyph used for the ✕ and the ‹ › arrows.
        let control = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .size(px(30.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border_input)
                .text_color(theme.fg_muted)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.fg_base))
                .child(SharedString::from(glyph))
        };

        // The image box is sized RELATIVE TO THE BACKDROP (a definite full-window
        // box), NOT a shrink-wrapped column — otherwise `relative(..)` resolves
        // against an auto-sized parent and collapses to zero (blank image). It's
        // a direct child of the backdrop for that reason. Clicking the image
        // itself is swallowed so only the dark margin (or ✕) dismisses.
        let image_box = div()
            .w(relative(0.86))
            .h(relative(0.78))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
            .child(
                img(ImageSource::Image(image))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );

        let pager = (n > 1).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(16.0))
                // Clicks on the pager (label / gaps) shouldn't close either.
                .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                .child(control("chat-image-prev", "‹").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.step_image_preview(-1, cx)),
                ))
                .child(
                    div()
                        .text_size(px(typo.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(SharedString::from(format!("{} of {}", i + 1, n))),
                )
                .child(control("chat-image-next", "›").on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.step_image_preview(1, cx)),
                ))
        });

        Some(
            div()
                .id("chat-image-preview")
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .bg(gpui::black().opacity(0.82))
                // A click on the bare (dark) backdrop closes the lightbox.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| this.close_image_preview(cx)),
                )
                .child(image_box)
                .children(pager)
                // ✕ sits on the backdrop, so clicking it bubbles to the close
                // handler.
                .child(
                    control("chat-image-preview-close", "✕")
                        .absolute()
                        .top(px(16.0))
                        .right(px(16.0)),
                )
                .into_any_element(),
        )
    }
}
