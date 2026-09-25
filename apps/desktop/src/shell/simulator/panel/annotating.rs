//! Annotate mode: freeze a full-resolution screenshot, mark it up, then send
//! it to the active agent (the browser picker's `SendPickToActiveChat`
//! route), copy it, or save it to the Desktop. The toolbar pill becomes the
//! annotation controls meanwhile.

use std::time::Duration;

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Image, ImageFormat, IntoElement, ParentElement as _, Styled as _,
    Window, div, px,
};
use gpui_component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use super::{PanelEvent, RootRequest, SimulatorPanel};
use crate::actions::SendPickToActiveChat;
use crate::shell::simulator::annotate::{AnnotateView, Frozen};
use crate::shell::simulator::annotate::export::{self, Tool};
use crate::shell::simulator::hub::{CaptureKind, NoticeKind, capture_dir, capture_path, simulator_dir, stamp};

/// What to do with the marked-up screenshot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    Send,
    Copy,
    Save,
}

impl SimulatorPanel {
    /// Freeze the screen for annotation: the helper's screenshot is full
    /// resolution and rotated like the stream, so strokes land where drawn.
    pub(super) fn start_annotating(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.annotate.is_some() {
            return;
        }
        let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) else { return };
        let Some(session) = hub.read(cx).session(&udid) else { return };
        let radius = self.screen_radius();
        cx.spawn_in(window, async move |this, cx| {
            let shot = cx
                .background_executor()
                .spawn(async move {
                    let png = session.screenshot_png(Duration::from_secs(10)).map_err(|e| e.to_string())?;
                    Frozen::decode(png)
                })
                .await;
            let _ = this.update(cx, |panel, cx| match shot {
                Ok(frozen) => {
                    panel.annotate = Some(cx.new(|_| AnnotateView::new(frozen, radius)));
                    cx.notify();
                }
                Err(e) => cx.emit(PanelEvent::Notice(NoticeKind::Error, format!("Could not freeze the screen: {e}"))),
            });
        })
        .detach();
    }

    pub(super) fn stop_annotating(&mut self, cx: &mut Context<Self>) {
        if self.annotate.take().is_some() {
            cx.notify();
        }
    }

    fn finish_annotating(&mut self, how: Finish, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.annotate.take() else { return };
        cx.notify();
        let (png, strokes) = view.read(cx).snapshot();
        let device = self.device_name(cx);
        cx.spawn_in(window, async move |this, cx| {
            let name = format!("{device} (annotated)");
            let made = cx
                .background_executor()
                .spawn(async move {
                    // Resolved here: probing the Desktop can wait on the
                    // privacy prompt.
                    let dir = match how {
                        Finish::Save => Some(capture_dir()),
                        // Terminal agents cannot take an image, only a path to one.
                        Finish::Send => Some(simulator_dir().join("annotations")),
                        Finish::Copy => None,
                    };
                    let out = export::annotate_png(&png, &strokes)?;
                    let path = match dir {
                        Some(dir) => {
                            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                            let path = capture_path(&dir, CaptureKind::Screenshot, &name, &stamp());
                            std::fs::write(&path, &out).map_err(|e| format!("could not write {}: {e}", path.display()))?;
                            Some(path)
                        }
                        None => None,
                    };
                    // The chat gets a model-sized copy; the file keeps full size.
                    let out = if how == Finish::Send { export::fit_long_edge(&out, export::AGENT_LONG_EDGE)? } else { out };
                    Ok::<_, String>((out, path))
                })
                .await;
            let send = this.update(cx, |panel, cx| {
                let (png, path) = match made {
                    Ok(made) => made,
                    Err(e) => {
                        cx.emit(PanelEvent::Notice(NoticeKind::Error, format!("Annotation failed: {e}")));
                        return None;
                    }
                };
                match how {
                    Finish::Send => {
                        let mut markdown = format!("Annotated iOS Simulator screenshot ({}).", panel.device_name(cx));
                        if let Some(path) = &path {
                            markdown.push_str(&format!("\nImage: {}", path.display()));
                        }
                        let pick = SendPickToActiveChat { markdown, selector: "iOS Simulator".into(), png };
                        return panel.command_sink.clone().map(|sink| (sink, pick));
                    }
                    Finish::Copy => {
                        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)));
                        cx.emit(PanelEvent::Notice(NoticeKind::Success, "Annotated screenshot copied".into()));
                    }
                    Finish::Save => {
                        let name = path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                        let text = format!("Saved {}", name.unwrap_or_default());
                        cx.emit(PanelEvent::Notice(NoticeKind::Success, text));
                    }
                }
                None
            });
            // Outside the panel's update: the root may update the panel.
            if let Ok(Some((sink, pick))) = send {
                let _ = cx.update(|window, cx| sink(RootRequest::SendToAgent(pick), window, cx));
            }
        })
        .detach();
    }

    /// The annotation controls, in the toolbar's place.
    pub(super) fn render_annotate_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.annotate.clone() else { return div().into_any_element() };
        let (theme, density) = (self.theme, self.density);
        let (tool, can_undo) = { let v = view.read(cx); (v.tool(), v.can_undo()) };
        let divider = || div().w(px(1.0)).h(px(density.h_row * 0.6)).bg(theme.border_inactive);
        let mut pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .flex_none()
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .rounded_full()
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel);
        for (id, icon, tip, t) in [
            ("sim-an-pen", "icons/pencil.svg", "Pen", Tool::Pen),
            ("sim-an-arrow", "icons/arrow-up-right.svg", "Arrow", Tool::Arrow),
            ("sim-an-rect", "icons/square.svg", "Rectangle", Tool::Rect),
        ] {
            let target = view.clone();
            let mut button = Button::new(id).small().icon(Icon::default().path(icon)).tooltip(tip);
            button = if tool == t { button.primary() } else { button.ghost() };
            pill = pill.child(button.on_click(move |_, _window, cx| target.update(cx, |v, cx| v.set_tool(t, cx))));
        }
        let undo = view.clone();
        pill = pill
            .child(divider())
            .child(
                Button::new("sim-an-undo")
                    .ghost()
                    .small()
                    .icon(Icon::default().path("icons/undo-2.svg"))
                    .tooltip("Undo")
                    .disabled(!can_undo)
                    .on_click(move |_, _window, cx| undo.update(cx, |v, cx| v.undo(cx))),
            )
            .child(divider());
        for (id, icon, tip, how) in [
            ("sim-an-copy", "icons/copy.svg", "Copy", Finish::Copy),
            ("sim-an-save", "icons/download.svg", "Save to Desktop", Finish::Save),
        ] {
            pill = pill.child(
                Button::new(id)
                    .ghost()
                    .small()
                    .icon(Icon::default().path(icon))
                    .tooltip(tip)
                    .on_click(cx.listener(move |this, _, window, cx| this.finish_annotating(how, window, cx))),
            );
        }
        pill.child(
            Button::new("sim-an-send")
                .primary()
                .small()
                .label("Send to agent")
                .on_click(cx.listener(|this, _, window, cx| this.finish_annotating(Finish::Send, window, cx))),
        )
        .child(
            Button::new("sim-an-cancel")
                .ghost()
                .small()
                .icon(Icon::default().path("icons/x.svg"))
                .tooltip("Cancel")
                .on_click(cx.listener(|this, _, _window, cx| this.stop_annotating(cx))),
        )
        .into_any_element()
    }
}
