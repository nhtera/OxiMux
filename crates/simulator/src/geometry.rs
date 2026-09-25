//! Pure coordinate math: aspect-fit letterboxing for the panel's image view,
//! and the portrait ↔ display coordinate mapping the rest of the crate needs
//! because **the simulator framebuffer never rotates** — see
//! [`crate::Orientation`]'s doc comment.
//!
//! ## Rotation directions
//! Pinned by two measurements against a live helper
//! (`plans/260924-1433-ios-simulator-panel/reports/spike-report.md` §2,
//! "Finding that changes P6", and the P3 plan's "Carried in from P1"): in
//! [`Orientation::LandscapeRight`] the portrait buffer's point `(0.08, 0.50)`
//! is the same physical pixel as the displayed point `(0.50, 0.08)` (Safari's
//! address bar, top-center in landscape) — i.e. **display = buffer rotated
//! 90° clockwise**. In [`Orientation::LandscapeLeft`] the buffer's *right*
//! edge shows the display's top row, i.e. **display = buffer rotated 90°
//! counter-clockwise**. `Portrait` and `PortraitUpsideDown` are 0°/180°.
//!
//! ## `edge` code
//! [`edge_for`] mirrors upstream serve-sim's `HID_EDGE_*` constants, its
//! home-indicator hot-zone band, and its per-orientation edge remap:
//! `packages/serve-sim/src/client/simulator/orientation.ts:17-20` (edge
//! codes), `:34` (0.93 threshold), `:42-44` (`homeIndicatorEdge`), `:154-201`
//! (`rawEdgeForDisplayEdge`). Upstream's `landscape_left` / `landscape_right`
//! labels are the *opposite* of this crate's [`Orientation::LandscapeLeft`] /
//! [`Orientation::LandscapeRight`]: upstream's `rawPointForDisplayPoint` for
//! `landscape_left` (`orientation.ts:121-122`, `{x: y, y: 1 - x}`) matches
//! this file's `LandscapeRight` case once you substitute display/portrait
//! variable names, and its `landscape_right` case matches `LandscapeLeft`.
//! The swap was confirmed against the spike's cross-check above before
//! trusting the upstream table for `edge_for`.

use crate::Orientation;

/// A width/height pair. Callers track whether it's pixels, points, or a
/// normalized `0..1` extent — this type carries no unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

impl Size {
    pub fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }

    /// `w`/`h` swapped, e.g. to go from a portrait size to its landscape
    /// display size.
    pub fn swapped(self) -> Self {
        Self { w: self.h, h: self.w }
    }
}

/// An axis-aligned rectangle, top-left origin, `y` increasing downward
/// (screen convention).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// Aspect-fits `image` inside `container`, centered (CSS `object-fit:
/// contain`), and returns the painted rect in container coordinates. Returns
/// a zero rect for a degenerate (non-positive) container or image.
pub fn letterbox(container: Size, image: Size) -> Rect {
    if container.w <= 0.0 || container.h <= 0.0 || image.w <= 0.0 || image.h <= 0.0 {
        return Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    let scale = (container.w / image.w).min(container.h / image.h);
    let w = image.w * scale;
    let h = image.h * scale;
    Rect::new((container.w - w) / 2.0, (container.h - h) / 2.0, w, h)
}

/// Normalizes a point in container coordinates to `0..1` of `rect`, or
/// `None` when it falls outside `rect` (the letterbox bars, or a degenerate
/// rect) — the caller's cue to ignore the input rather than clamp it onto an
/// edge the user didn't touch.
pub fn to_normalized(p: (f64, f64), r: Rect) -> Option<(f64, f64)> {
    if r.w <= 0.0 || r.h <= 0.0 {
        return None;
    }
    let (x, y) = p;
    if x < r.x || x > r.x + r.w || y < r.y || y > r.y + r.h {
        return None;
    }
    Some((clamp01((x - r.x) / r.w), clamp01((y - r.y) / r.h)))
}

/// Maps a display-normalized point (the on-screen image, in the orientation
/// currently commanded via `configure{orientation}`) to portrait-normalized
/// framebuffer space — what the helper's `touch`/`multi_touch` commands take
/// in every orientation. See the module doc for the rotation directions.
pub fn display_to_portrait(o: Orientation, (dx, dy): (f64, f64)) -> (f64, f64) {
    let (px, py) = match o {
        Orientation::Portrait => (dx, dy),
        Orientation::PortraitUpsideDown => (1.0 - dx, 1.0 - dy),
        Orientation::LandscapeLeft => (1.0 - dy, dx),
        Orientation::LandscapeRight => (dy, 1.0 - dx),
    };
    (clamp01(px), clamp01(py))
}

/// The inverse of [`display_to_portrait`]: portrait-normalized (framebuffer)
/// to display-normalized (on-screen image) space.
pub fn portrait_to_display(o: Orientation, (px, py): (f64, f64)) -> (f64, f64) {
    let (dx, dy) = match o {
        Orientation::Portrait => (px, py),
        Orientation::PortraitUpsideDown => (1.0 - px, 1.0 - py),
        Orientation::LandscapeLeft => (py, 1.0 - px),
        Orientation::LandscapeRight => (1.0 - py, px),
    };
    (clamp01(dx), clamp01(dy))
}

/// The on-screen display size for a `portrait`-space framebuffer size, given
/// `o`: unchanged in portrait orientations, `w`/`h` swapped in landscape
/// (the `size` event always reports the portrait buffer's own `w`/`h`).
pub fn display_size(o: Orientation, portrait: Size) -> Size {
    if o.is_landscape() { portrait.swapped() } else { portrait }
}

/// Wire codes for [`edge_for`], matching upstream's `HID_EDGE_*`
/// (`orientation.ts:17-20`).
pub const HID_EDGE_LEFT: u32 = 1;
pub const HID_EDGE_TOP: u32 = 2;
pub const HID_EDGE_BOTTOM: u32 = 3;
pub const HID_EDGE_RIGHT: u32 = 4;

/// Bottom fraction of the *display*, in display-normalized `y`, treated as
/// the home-indicator hot zone (upstream `HOME_INDICATOR_BAND_NORM`,
/// `orientation.ts:34`; upstream's comment there explains why it's a narrow
/// 7% band and not the more obvious-looking 12%).
pub const HOME_INDICATOR_BAND_NORM: f64 = 0.93;

/// The portrait-space `edge` code to send with a touch that *begins* at
/// display point `(dx, dy)`, or `None` when it starts outside the
/// home-indicator band and is just a plain touch.
///
/// Only the begin point decides the edge for a whole gesture: upstream's
/// `moveDuoTouch` (`duo-home-gesture.ts:14-16`) carries the begin step's edge
/// through every subsequent `move`/`end` unchanged, even once the finger has
/// moved out of the band — a system edge gesture keeps its edge for its
/// whole lifetime. Callers building a [`crate::gesture`] swipe that starts in
/// the band must do the same.
pub fn edge_for(o: Orientation, (_dx, dy): (f64, f64)) -> Option<u32> {
    if dy < HOME_INDICATOR_BAND_NORM {
        return None;
    }
    Some(match o {
        Orientation::Portrait => HID_EDGE_BOTTOM,
        Orientation::PortraitUpsideDown => HID_EDGE_TOP,
        Orientation::LandscapeLeft => HID_EDGE_LEFT,
        Orientation::LandscapeRight => HID_EDGE_RIGHT,
    })
}

/// Converts an AX node's frame origin (logical display points, in the
/// orientation the frame was captured in — see [`crate::ax`]) to
/// portrait-normalized coordinates, for tapping something found by
/// accessibility label or id. `logical_display_size` is the AX tree's root
/// frame size (e.g. `402×874` portrait, `874×402` landscape in the test
/// fixtures).
pub fn logical_points_to_portrait_normalized(
    o: Orientation,
    point: (f64, f64),
    logical_display_size: Size,
) -> (f64, f64) {
    if logical_display_size.w <= 0.0 || logical_display_size.h <= 0.0 {
        return (0.0, 0.0);
    }
    let display_normalized =
        (clamp01(point.0 / logical_display_size.w), clamp01(point.1 / logical_display_size.h));
    display_to_portrait(o, display_normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ORIENTATIONS: [Orientation; 4] = [
        Orientation::Portrait,
        Orientation::PortraitUpsideDown,
        Orientation::LandscapeLeft,
        Orientation::LandscapeRight,
    ];

    fn approx(a: (f64, f64), b: (f64, f64)) {
        assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9, "{a:?} != {b:?}");
    }

    #[test]
    fn letterbox_centers_and_scales_to_fit() {
        // Wider container than image: image is height-limited, centered
        // horizontally.
        let r = letterbox(Size::new(200.0, 100.0), Size::new(100.0, 100.0));
        assert_eq!(r, Rect::new(50.0, 0.0, 100.0, 100.0));

        // Taller container than image: image is width-limited, centered
        // vertically.
        let r = letterbox(Size::new(100.0, 200.0), Size::new(100.0, 100.0));
        assert_eq!(r, Rect::new(0.0, 50.0, 100.0, 100.0));

        // A real panel-ish case: Half-res portrait buffer in a ~310x680 pane.
        let r = letterbox(Size::new(310.0, 680.0), Size::new(603.0, 1311.0));
        assert!((r.w - 310.0).abs() < 0.5);
        assert!(r.h < 680.0);
        assert!(r.x.abs() < 1e-9);
    }

    #[test]
    fn letterbox_degenerate_inputs_are_a_zero_rect() {
        assert_eq!(letterbox(Size::new(0.0, 100.0), Size::new(10.0, 10.0)), Rect::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(letterbox(Size::new(100.0, 100.0), Size::new(-1.0, 10.0)), Rect::new(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn to_normalized_maps_inside_the_rect() {
        let r = Rect::new(50.0, 0.0, 100.0, 100.0);
        approx(to_normalized((50.0, 0.0), r).unwrap(), (0.0, 0.0));
        approx(to_normalized((150.0, 100.0), r).unwrap(), (1.0, 1.0));
        approx(to_normalized((100.0, 50.0), r).unwrap(), (0.5, 0.5));
    }

    #[test]
    fn to_normalized_rejects_the_letterbox_bars() {
        let r = Rect::new(50.0, 0.0, 100.0, 100.0);
        assert_eq!(to_normalized((49.0, 50.0), r), None);
        assert_eq!(to_normalized((151.0, 50.0), r), None);
        assert_eq!(to_normalized((10.0, 10.0), Rect::new(0.0, 0.0, 0.0, 50.0)), None);
    }

    #[test]
    fn portrait_and_display_are_identical_in_portrait_orientation() {
        approx(display_to_portrait(Orientation::Portrait, (0.3, 0.7)), (0.3, 0.7));
        approx(portrait_to_display(Orientation::Portrait, (0.3, 0.7)), (0.3, 0.7));
    }

    #[test]
    fn portrait_upside_down_is_a_180_rotation() {
        approx(display_to_portrait(Orientation::PortraitUpsideDown, (0.2, 0.9)), (0.8, 0.1));
        approx(portrait_to_display(Orientation::PortraitUpsideDown, (0.2, 0.9)), (0.8, 0.1));
    }

    /// The exact spike measurement: in `LandscapeRight`, portrait (0.08,
    /// 0.50) is the Safari address bar, which displays at (0.50, 0.08).
    #[test]
    fn landscape_right_matches_the_spike_cross_check() {
        approx(portrait_to_display(Orientation::LandscapeRight, (0.08, 0.50)), (0.50, 0.08));
        approx(display_to_portrait(Orientation::LandscapeRight, (0.50, 0.08)), (0.08, 0.50));
    }

    /// `LandscapeLeft`'s buffer right edge (`px == 1`) shows the display's
    /// top row (`dy == 0`).
    #[test]
    fn landscape_left_buffer_right_edge_is_display_top() {
        approx(portrait_to_display(Orientation::LandscapeLeft, (1.0, 0.42)), (0.42, 0.0));
        approx(display_to_portrait(Orientation::LandscapeLeft, (0.42, 0.0)), (1.0, 0.42));
    }

    #[test]
    fn display_and_portrait_round_trip_for_every_orientation() {
        let samples = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.123, 0.876), (0.5, 0.5)];
        for o in ALL_ORIENTATIONS {
            for p in samples {
                approx(display_to_portrait(o, portrait_to_display(o, p)), p);
                approx(portrait_to_display(o, display_to_portrait(o, p)), p);
            }
        }
    }

    #[test]
    fn display_to_portrait_clamps_out_of_range_input() {
        let (x, y) = display_to_portrait(Orientation::Portrait, (-0.5, 1.5));
        assert_eq!((x, y), (0.0, 1.0));
    }

    #[test]
    fn display_size_swaps_only_in_landscape() {
        let portrait = Size::new(402.0, 874.0);
        assert_eq!(display_size(Orientation::Portrait, portrait), portrait);
        assert_eq!(display_size(Orientation::PortraitUpsideDown, portrait), portrait);
        assert_eq!(display_size(Orientation::LandscapeLeft, portrait), Size::new(874.0, 402.0));
        assert_eq!(display_size(Orientation::LandscapeRight, portrait), Size::new(874.0, 402.0));
    }

    #[test]
    fn edge_for_is_none_outside_the_home_indicator_band() {
        assert_eq!(edge_for(Orientation::Portrait, (0.5, 0.92)), None);
        assert_eq!(edge_for(Orientation::Portrait, (0.5, 0.0)), None);
    }

    #[test]
    fn edge_for_matches_upstream_per_orientation() {
        let y = 0.99;
        assert_eq!(edge_for(Orientation::Portrait, (0.5, y)), Some(HID_EDGE_BOTTOM));
        assert_eq!(edge_for(Orientation::PortraitUpsideDown, (0.5, y)), Some(HID_EDGE_TOP));
        assert_eq!(edge_for(Orientation::LandscapeLeft, (0.5, y)), Some(HID_EDGE_LEFT));
        assert_eq!(edge_for(Orientation::LandscapeRight, (0.5, y)), Some(HID_EDGE_RIGHT));
    }

    #[test]
    fn edge_for_boundary_is_inclusive() {
        assert_eq!(edge_for(Orientation::Portrait, (0.5, HOME_INDICATOR_BAND_NORM)), Some(HID_EDGE_BOTTOM));
    }

    /// The landscape-left fixture's Address button frame center
    /// (`{x:278,y:10,w:317.667,h:44}` in a 874×402 logical display) sits near
    /// the display's top-center; converted to portrait it should land near
    /// the buffer's *right* edge, per `landscape_left_buffer_right_edge_is_display_top`.
    #[test]
    fn logical_points_matches_the_ax_landscape_fixture_sanity_check() {
        let center = (278.0 + 317.666_666_666_666_74 / 2.0, 10.0 + 44.0 / 2.0);
        let (px, py) = logical_points_to_portrait_normalized(
            Orientation::LandscapeLeft,
            center,
            Size::new(874.0, 402.0),
        );
        assert!(px > 0.85, "expected near the buffer's right edge, got {px}");
        assert!((py - 0.5).abs() < 0.05, "expected mid-height, got {py}");
    }

    #[test]
    fn logical_points_degenerate_size_is_origin() {
        assert_eq!(logical_points_to_portrait_normalized(Orientation::Portrait, (10.0, 10.0), Size::new(0.0, 0.0)), (0.0, 0.0));
    }
}
