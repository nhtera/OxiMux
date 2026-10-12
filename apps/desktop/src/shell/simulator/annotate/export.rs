//! Burn annotation strokes into the full-resolution screenshot. Pure raster
//! work on the `image` crate: a thick line is a run of filled discs stamped
//! along it, which also rounds its joins and ends.

use image::{Rgba, RgbaImage};

/// iOS system red.
pub(crate) const INK: Rgba<u8> = Rgba([255, 59, 48, 255]);

/// What a stroke draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Arrow,
    Rect,
}

/// One mark, in image fractions (`0..1` of width and height), so it survives
/// any display size. Arrow and rectangle use the first and last points.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub tool: Tool,
    pub points: Vec<(f32, f32)>,
}

impl Stroke {
    /// The segments to draw, in image fractions. `aspect` is width ÷ height
    /// of the image, so an arrow head keeps its angle on a tall screen.
    pub fn segments(&self, aspect: f32) -> Vec<((f32, f32), (f32, f32))> {
        let (Some(&a), Some(&b)) = (self.points.first(), self.points.last()) else { return Vec::new() };
        match self.tool {
            Tool::Pen if self.points.len() == 1 => vec![(a, a)],
            Tool::Pen => self.points.windows(2).map(|w| (w[0], w[1])).collect(),
            Tool::Rect => {
                let (c, d) = ((b.0, a.1), (a.0, b.1));
                vec![(a, c), (c, b), (b, d), (d, a)]
            }
            Tool::Arrow => {
                let mut out = vec![(a, b)];
                // Work in a square space (x scaled by the aspect) so the head
                // is symmetric, then map back.
                let (dx, dy) = ((b.0 - a.0) * aspect, b.1 - a.1);
                let len = dx.hypot(dy);
                if len > f32::EPSILON {
                    let head = (len * 0.3).min(0.05);
                    let (ux, uy) = (dx / len, dy / len);
                    for turn in [0.5f32, -0.5] {
                        // Back along the shaft, turned ±~29°.
                        let (c, s) = (turn.cos(), turn.sin());
                        let (hx, hy) = (-(ux * c - uy * s) * head, -(ux * s + uy * c) * head);
                        out.push((b, (b.0 + hx / aspect, b.1 + hy)));
                    }
                }
                out
            }
        }
    }
}

/// Stroke thickness in pixels for an image `width` wide: about what 3 pt
/// looks like on the panel's phone.
pub fn thickness(width: u32) -> f32 {
    (width as f32 * 0.008).max(2.0)
}

/// Draw `strokes` onto `image`.
pub fn composite(image: &mut RgbaImage, strokes: &[Stroke]) {
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let radius = thickness(w) / 2.0;
    let aspect = w as f32 / h as f32;
    for stroke in strokes {
        for (a, b) in stroke.segments(aspect) {
            let (ax, ay, bx, by) = (a.0 * w as f32, a.1 * h as f32, b.0 * w as f32, b.1 * h as f32);
            let steps = ((bx - ax).hypot(by - ay) / (radius * 0.5).max(0.5)).ceil().max(1.0) as u32;
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                disc(image, ax + (bx - ax) * t, ay + (by - ay) * t, radius);
            }
        }
    }
}

fn disc(image: &mut RgbaImage, cx: f32, cy: f32, r: f32) {
    let (w, h) = image.dimensions();
    let (x0, x1) = ((cx - r).floor().max(0.0) as u32, ((cx + r).ceil() as u32).min(w.saturating_sub(1)));
    let (y0, y1) = ((cy - r).floor().max(0.0) as u32, ((cy + r).ceil() as u32).min(h.saturating_sub(1)));
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if dx * dx + dy * dy <= r * r {
                image.put_pixel(x, y, INK);
            }
        }
    }
}

/// Longest side of an image sent to an agent: model APIs cap image size and
/// scale larger ones down anyway.
pub const AGENT_LONG_EDGE: u32 = 1568;

/// `png` scaled so its longer side is at most `max` (unchanged when it
/// already fits), re-encoded as PNG.
pub fn fit_long_edge(png: &[u8], max: u32) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    if image.width().max(image.height()) <= max {
        return Ok(png.to_vec());
    }
    let scaled = image.resize(max, max, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    scaled.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out)
}

/// `png` cut to `rect` — `(x, y, w, h)` as fractions of the image — grown by
/// `pad` pixels on every side and clamped to the image, re-encoded as PNG.
pub fn crop(png: &[u8], rect: (f64, f64, f64, f64), pad: u32) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    let (w, h) = (f64::from(image.width()), f64::from(image.height()));
    let pad = f64::from(pad);
    let x0 = (rect.0 * w - pad).clamp(0.0, w);
    let y0 = (rect.1 * h - pad).clamp(0.0, h);
    let x1 = ((rect.0 + rect.2) * w + pad).clamp(0.0, w);
    let y1 = ((rect.1 + rect.3) * h + pad).clamp(0.0, h);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return Err("the element is off the screen".into());
    }
    // Whole pixels that cover the element: start down, end up.
    let (x0, y0, x1, y1) = (x0.floor(), y0.floor(), x1.ceil().min(w), y1.ceil().min(h));
    let cut = image.crop_imm(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
    let mut out = Vec::new();
    cut.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Composite `strokes` onto the PNG `screenshot` and encode the result.
pub fn annotate_png(screenshot: &[u8], strokes: &[Stroke]) -> Result<Vec<u8>, String> {
    let mut image = image::load_from_memory_with_format(screenshot, image::ImageFormat::Png)
        .map_err(|e| format!("screenshot did not decode: {e}"))?
        .to_rgba8();
    composite(&mut image, strokes);
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("could not encode the annotated screenshot: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(white(w, h)).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn size_of(png: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn a_crop_takes_the_frame_with_padding_clamped_at_the_edges() {
        let shot = png(400, 800);
        // A 100×50 element in the middle, 10 px of padding each side.
        assert_eq!(size_of(&crop(&shot, (0.25, 0.5, 0.25, 0.0625), 10).unwrap()), (120, 70));
        // At the top-left corner the padding stops at the image.
        assert_eq!(size_of(&crop(&shot, (0.0, 0.0, 0.25, 0.0625), 10).unwrap()), (110, 60));
        assert!(crop(&shot, (2.0, 2.0, 0.1, 0.1), 0).is_err(), "off the image");
        // x 100.9 → 150.2 covers pixels 100 to 150: 51 wide, none clipped.
        assert_eq!(size_of(&crop(&shot, (100.9 / 400.0, 0.5, 49.3 / 400.0, 0.0625), 0).unwrap()).0, 51);
    }

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn a_pen_stroke_inks_its_path_and_nothing_far_from_it() {
        let mut img = white(400, 800);
        let pen = Stroke { tool: Tool::Pen, points: vec![(0.1, 0.5), (0.9, 0.5)] };
        composite(&mut img, &[pen]);
        assert_eq!(*img.get_pixel(200, 400), INK, "on the line");
        assert_eq!(*img.get_pixel(200, 100), Rgba([255, 255, 255, 255]), "far away");
        assert_eq!(*img.get_pixel(10, 400), Rgba([255, 255, 255, 255]), "before its start");
    }

    #[test]
    fn a_rectangle_draws_its_four_edges_and_leaves_the_inside() {
        let mut img = white(400, 800);
        composite(&mut img, &[Stroke { tool: Tool::Rect, points: vec![(0.25, 0.25), (0.75, 0.75)] }]);
        for (x, y) in [(100, 400), (300, 400), (200, 200), (200, 600)] {
            assert_eq!(*img.get_pixel(x, y), INK, "edge at {x},{y}");
        }
        assert_eq!(*img.get_pixel(200, 400), Rgba([255, 255, 255, 255]), "inside");
    }

    #[test]
    fn an_arrow_has_a_shaft_and_a_two_sided_head() {
        let arrow = Stroke { tool: Tool::Arrow, points: vec![(0.1, 0.5), (0.9, 0.5)] };
        let segs = arrow.segments(0.5);
        assert_eq!(segs.len(), 3);
        // The two head lines leave the tip on opposite sides of the shaft.
        let (up, down) = (segs[1].1 .1, segs[2].1 .1);
        assert!((up - 0.5) * (down - 0.5) < 0.0, "{up} {down}");
        assert!(segs[1].1 .0 < 0.9 && segs[2].1 .0 < 0.9, "heads point back");
        // A zero-length arrow is just a dot.
        assert_eq!(Stroke { tool: Tool::Arrow, points: vec![(0.5, 0.5)] }.segments(1.0).len(), 1);
    }

    #[test]
    fn fit_long_edge_scales_only_what_is_too_big() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(white(100, 400))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = fit_long_edge(&png, 200).unwrap();
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (50, 200), "aspect kept");
        assert_eq!(fit_long_edge(&png, 400).unwrap(), png, "already fits");
    }

    #[test]
    fn annotate_png_round_trips_and_marks_the_image() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(white(60, 120))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = annotate_png(&png, &[Stroke { tool: Tool::Pen, points: vec![(0.5, 0.5)] }]).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (60, 120));
        assert_eq!(*img.get_pixel(30, 60), INK);
        assert!(annotate_png(b"not a png", &[]).is_err());
    }

    #[test]
    fn thickness_scales_to_image_width() {
        assert_eq!(thickness(100), 2.0);
        assert_eq!(thickness(1000), 8.0_f32);
        assert_eq!(thickness(250), 2.0_f32);
        assert!(thickness(500) > 2.0);
    }

    #[test]
    fn composite_with_empty_strokes_does_not_crash() {
        let mut img = white(400, 800);
        composite(&mut img, &[]);
        assert_eq!(*img.get_pixel(200, 400), Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn stroke_segments_with_reversed_rect_corners() {
        // Rectangle with bottom-right corner above/left of top-left corner
        let rect = Stroke { tool: Tool::Rect, points: vec![(0.75, 0.75), (0.25, 0.25)] };
        let segs = rect.segments(1.0);
        assert_eq!(segs.len(), 4, "rectangle has four edges regardless of corner order");
        // Should still form a closed path
        assert_eq!(segs[0].0, (0.75, 0.75));
        assert_eq!(segs[3].1, (0.75, 0.75));
    }

    #[test]
    fn stroke_segments_empty_points() {
        let empty = Stroke { tool: Tool::Pen, points: vec![] };
        assert_eq!(empty.segments(1.0).len(), 0);
        let empty_arrow = Stroke { tool: Tool::Arrow, points: vec![] };
        assert_eq!(empty_arrow.segments(1.0).len(), 0);
        let empty_rect = Stroke { tool: Tool::Rect, points: vec![] };
        assert_eq!(empty_rect.segments(1.0).len(), 0);
    }
}
