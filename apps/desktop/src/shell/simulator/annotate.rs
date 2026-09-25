//! [`AnnotateView`]: a frozen, full-resolution screenshot the user marks up
//! (pen, arrow, rectangle, undo) before sending it to an agent, copying or
//! saving it. It takes the live screen's place inside the phone; the panel
//! swaps its toolbar for the annotation controls meanwhile.
//!
//! Strokes are kept in image fractions ([`export::Stroke`]) and drawn live
//! with GPUI paths; [`export::annotate_png`] burns them into the PNG.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Bounds, Context, DispatchPhase, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement as _, PathBuilder, Pixels, Point, Render, RenderImage, Styled as _, StyledImage as _,
    Window, canvas, div, img, point, px,
};
use oximux_simulator::geometry::{self, Size};

pub mod export;

use export::{Stroke, Tool};

/// On-screen stroke width.
const INK_WIDTH: f32 = 3.0;

pub struct AnnotateView {
    png: Arc<Vec<u8>>,
    image: Arc<RenderImage>,
    size: (u32, u32),
    strokes: Vec<Stroke>,
    drawing: Option<Stroke>,
    tool: Tool,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    radius: f32,
}

/// A decoded screenshot, made off the UI thread (the view itself is not
/// `Send`).
pub struct Frozen {
    png: Vec<u8>,
    image: Arc<RenderImage>,
    size: (u32, u32),
}

impl Frozen {
    /// Decode `png` (the helper's full-resolution screenshot, rotated like
    /// the stream) for display.
    pub fn decode(png: Vec<u8>) -> Result<Self, String> {
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| format!("screenshot did not decode: {e}"))?
            .to_rgba8();
        let size = decoded.dimensions();
        let mut bgra = decoded.into_raw();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(size.0, size.1, bgra).ok_or("screenshot size and data disagree")?;
        let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
        Ok(Self { png, image, size })
    }
}

impl AnnotateView {
    pub fn new(frozen: Frozen, radius: f32) -> Self {
        let Frozen { png, image, size } = frozen;
        Self { png: Arc::new(png), image, size, strokes: Vec::new(), drawing: None, tool: Tool::Pen, bounds: Rc::default(), radius }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = tool;
        cx.notify();
    }

    pub fn can_undo(&self) -> bool {
        !self.strokes.is_empty()
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.strokes.pop();
        cx.notify();
    }

    /// What export needs, cheap to move to a background thread.
    pub fn snapshot(&self) -> (Arc<Vec<u8>>, Vec<Stroke>) {
        (self.png.clone(), self.strokes.clone())
    }

    /// The frozen image's painted rect in window coordinates.
    fn image_rect(&self) -> Option<geometry::Rect> {
        let b = self.bounds.get()?;
        let (bw, bh) = (f64::from(f32::from(b.size.width)), f64::from(f32::from(b.size.height)));
        let fit = geometry::letterbox(Size::new(bw, bh), Size::new(f64::from(self.size.0), f64::from(self.size.1)));
        Some(geometry::Rect::new(f64::from(f32::from(b.origin.x)) + fit.x, f64::from(f32::from(b.origin.y)) + fit.y, fit.w, fit.h))
    }

    /// `pos` as an image fraction, clamped onto the image.
    fn fraction(&self, pos: Point<Pixels>) -> Option<(f32, f32)> {
        let r = self.image_rect()?;
        if r.w <= 0.0 || r.h <= 0.0 {
            return None;
        }
        let x = ((f64::from(f32::from(pos.x)) - r.x) / r.w).clamp(0.0, 1.0);
        let y = ((f64::from(f32::from(pos.y)) - r.y) / r.h).clamp(0.0, 1.0);
        Some((x as f32, y as f32))
    }

    fn on_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // Only on the image: a press in the letterbox bars starts nothing.
        let Some(r) = self.image_rect() else { return };
        let (x, y) = (f64::from(f32::from(event.position.x)), f64::from(f32::from(event.position.y)));
        if geometry::to_normalized((x, y), r).is_none() {
            return;
        }
        let Some(p) = self.fraction(event.position) else { return };
        self.drawing = Some(Stroke { tool: self.tool, points: vec![p] });
        cx.notify();
    }

    fn drag_to(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(p) = self.fraction(pos) else { return };
        let Some(stroke) = self.drawing.as_mut() else { return };
        match stroke.tool {
            Tool::Pen => stroke.points.push(p),
            Tool::Arrow | Tool::Rect => {
                stroke.points.truncate(1);
                stroke.points.push(p);
            }
        }
        cx.notify();
    }

    fn drag_end(&mut self, cx: &mut Context<Self>) {
        if let Some(stroke) = self.drawing.take() {
            self.strokes.push(stroke);
            cx.notify();
        }
    }
}

impl Render for AnnotateView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let rect = self.image_rect();
        let strokes: Vec<Stroke> = self.strokes.iter().chain(self.drawing.iter()).cloned().collect();
        let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
        let weak = cx.weak_entity();
        let ink = gpui::Rgba { r: 1.0, g: 59.0 / 255.0, b: 48.0 / 255.0, a: 1.0 };
        let overlay = canvas(
            move |b, _, _| bounds.set(Some(b)),
            move |_, _, window, _| {
                if let Some(r) = rect {
                    let at = |(x, y): (f32, f32)| point(px(r.x as f32 + x * r.w as f32), px(r.y as f32 + y * r.h as f32));
                    for stroke in &strokes {
                        let mut path = PathBuilder::stroke(px(INK_WIDTH));
                        for (a, b) in stroke.segments(aspect) {
                            path.move_to(at(a));
                            // A dot still needs length to show.
                            let b = if a == b { (b.0 + 0.002, b.1) } else { b };
                            path.line_to(at(b));
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, ink);
                        }
                    }
                }
                // Window-level, capture phase: a stroke keeps drawing (and
                // always ends) when the pointer leaves the screen.
                let on_move = weak.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _window, cx| {
                    if phase == DispatchPhase::Capture && e.pressed_button == Some(MouseButton::Left) {
                        let _ = on_move.update(cx, |view, cx| view.drag_to(e.position, cx));
                    }
                });
                let on_up = weak;
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, cx| {
                    if phase == DispatchPhase::Capture && e.button == MouseButton::Left {
                        let _ = on_up.update(cx, |view, cx| view.drag_end(cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .relative()
            .size_full()
            .bg(gpui::black())
            .cursor_crosshair()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_down))
            .child(img(self.image.clone()).size_full().object_fit(ObjectFit::Contain).rounded(px(self.radius)))
            .child(overlay)
    }
}
