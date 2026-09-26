//! Pure helpers for the agent verbs (`oximux sim …`): coordinates in points,
//! the device's pixel scale, and the input checks that must hold before a verb
//! reaches the device.
//!
//! Agents work in **points** in the screen's current orientation — the space
//! the accessibility tree reports frames in — while the helper takes touches in
//! portrait-normalized framebuffer space. [`points_to_portrait`] bridges the
//! two; it needs the device's scale (pixels per point), which
//! [`device_scale`] reads off the AX tree's root frame.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Orientation;
use crate::geometry::{self, Size};

/// Pixels per point: the screen's long edge in pixels over the AX root
/// frame's long edge in points, rounded (iOS devices are 2× or 3×, and the
/// root frame is the whole screen). Long edges, because the root frame
/// follows the *app's* orientation, which need not be the device's.
/// `None` for a degenerate or implausible ratio.
pub fn device_scale(display_px: Size, ax_root: Size) -> Option<f64> {
    let (px, pt) = (display_px.w.max(display_px.h), ax_root.w.max(ax_root.h));
    if px <= 0.0 || pt <= 0.0 {
        return None;
    }
    let scale = (px / pt).round();
    (1.0..=4.0).contains(&scale).then_some(scale)
}

/// The orientation an AX tree's frames are in. They follow the app's
/// interface, which turns with the device only if the app supports it —
/// Settings on an iPhone stays portrait on a landscape device — so it is read
/// off the root frame's shape (the root is the whole screen). A
/// landscape-shaped root is in the device's landscape, or, on a portrait
/// device, the usual landscape of a landscape-only app; a portrait-shaped one
/// is portrait (iPhone apps do not turn upside down).
pub fn ax_orientation(device: Orientation, root: Size) -> Orientation {
    match (root.w > root.h, device.is_landscape()) {
        (true, true) => device,
        (true, false) => Orientation::LandscapeLeft,
        (false, _) => Orientation::Portrait,
    }
}

/// A point in the AX tree's space as a touch coordinate.
pub fn ax_point_to_portrait(device: Orientation, root: Size, point: (f64, f64)) -> (f64, f64) {
    geometry::logical_points_to_portrait_normalized(ax_orientation(device, root), point, root)
}

/// An AX frame as a rectangle in display points — the space of a default
/// screenshot and of a point tap — so a frame read from `ax` and a position
/// read off the image always agree, whichever way the app is turned.
pub fn ax_rect_to_display(device: Orientation, root: Size, rect: geometry::Rect, display_pts: Size) -> geometry::Rect {
    let corner = |p: (f64, f64)| {
        let (x, y) = geometry::portrait_to_display(device, ax_point_to_portrait(device, root, p));
        (x * display_pts.w, y * display_pts.h)
    };
    let (a, b) = (corner((rect.x, rect.y)), corner((rect.x + rect.w, rect.y + rect.h)));
    geometry::Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
}

/// The screen's size in points, in orientation `o`.
pub fn display_points(o: Orientation, portrait_px: Size, scale: f64) -> Size {
    let px = geometry::display_size(o, portrait_px);
    Size::new(px.w / scale, px.h / scale)
}

/// A position in points (current orientation) as the portrait-normalized
/// coordinate a touch takes. `None` when it lies off the screen: an agent
/// aiming outside it has the wrong coordinates, and clamping would tap an edge
/// it never meant.
pub fn points_to_portrait(o: Orientation, (x, y): (f64, f64), portrait_px: Size, scale: f64) -> Option<(f64, f64)> {
    let pts = display_points(o, portrait_px, scale);
    if pts.w <= 0.0 || pts.h <= 0.0 || !(0.0..=pts.w).contains(&x) || !(0.0..=pts.h).contains(&y) {
        return None;
    }
    Some(geometry::display_to_portrait(o, (x / pts.w, y / pts.h)))
}

/// The intermediate positions of a straight swipe from `from` to `to` over
/// `duration`, one per ~16 ms, ending exactly on `to`. Always at least one.
pub fn swipe_path(from: (f64, f64), to: (f64, f64), duration: Duration) -> Vec<(f64, f64)> {
    let steps = (duration.as_millis() / 16).clamp(1, 240) as usize;
    (1..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// An `open-url` target: `http(s)` or an app's custom scheme. `file:` is
/// refused (it would open host paths inside the simulator), as are scripting
/// schemes and anything without a scheme at all.
pub fn check_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err("a URL needs a scheme, like https://…".into());
    };
    let valid = scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid || rest.is_empty() {
        return Err(format!("`{url}` is not a URL"));
    }
    match scheme.to_ascii_lowercase().as_str() {
        "file" => Err("file: URLs are not allowed; install the app and open its own scheme instead".into()),
        "javascript" | "data" => Err(format!("{scheme}: URLs are not allowed")),
        _ => Ok(()),
    }
}

/// Why an install path was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallPathError {
    /// Not an existing `.app` bundle directory.
    NotAnApp,
    /// Outside the worktree and outside DerivedData.
    Outside,
}

/// An `install` path, canonicalized, when it is a `.app` bundle inside
/// `worktree` or inside `derived_data` (Xcode's build products). Symlinks are
/// resolved first, so a link inside the worktree cannot point elsewhere.
pub fn check_install_path(path: &Path, worktree: &Path, derived_data: Option<&Path>) -> Result<PathBuf, InstallPathError> {
    let real = std::fs::canonicalize(path).map_err(|_| InstallPathError::NotAnApp)?;
    if !real.is_dir() || real.extension().and_then(|e| e.to_str()) != Some("app") {
        return Err(InstallPathError::NotAnApp);
    }
    let inside = |root: &Path| std::fs::canonicalize(root).is_ok_and(|root| real.starts_with(root));
    if inside(worktree) || derived_data.is_some_and(inside) {
        Ok(real)
    } else {
        Err(InstallPathError::Outside)
    }
}

/// Xcode's default DerivedData folder, under `home`.
pub fn derived_data(home: &Path) -> PathBuf {
    home.join("Library/Developer/Xcode/DerivedData")
}

/// The PNG's pixel size, from its header. `None` when it is not a PNG.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// iPhone 17 Pro: 1206×2622 px, 402×874 pt.
    const PORTRAIT: Size = Size { w: 1206.0, h: 2622.0 };

    #[test]
    fn the_scale_comes_from_the_ax_root_frame() {
        let (portrait, landscape) = (Size::new(1206.0, 2622.0), Size::new(2622.0, 1206.0));
        assert_eq!(device_scale(portrait, Size::new(402.0, 874.0)), Some(3.0));
        assert_eq!(device_scale(landscape, Size::new(874.0, 402.0)), Some(3.0), "a rotated app");
        assert_eq!(device_scale(landscape, Size::new(402.0, 874.0)), Some(3.0), "an app that stayed portrait");
        assert_eq!(device_scale(Size::new(1640.0, 2360.0), Size::new(820.0, 1180.0)), Some(2.0));
        assert_eq!(device_scale(portrait, Size::new(0.0, 0.0)), None);
        assert_eq!(device_scale(portrait, Size::new(10.0, 10.0)), None, "a partial frame is not the screen");
    }

    /// Settings on a landscape iPhone: the screen turned, the app did not.
    /// Its "General" row (portrait frame) must land where the sideways
    /// screenshot shows it — measured live: the column at x ≈ 293–345.
    #[test]
    fn an_app_that_did_not_turn_still_maps_onto_the_screenshot() {
        let (device, root) = (Orientation::LandscapeLeft, Size::new(402.0, 874.0));
        assert_eq!(ax_orientation(device, root), Orientation::Portrait);
        let pts = display_points(device, PORTRAIT, 3.0);
        let general = geometry::Rect::new(16.0, 293.0, 370.0, 52.0);
        let shown = ax_rect_to_display(device, root, general, pts);
        let r = |v: f64| (v * 1000.0).round() / 1000.0;
        assert_eq!((r(shown.x), r(shown.y), r(shown.w), r(shown.h)), (293.0, 16.0, 52.0, 370.0));
        // Tapping its centre by label and by the point read off the image is
        // one touch.
        let by_label = ax_point_to_portrait(device, root, general.center());
        let by_point = points_to_portrait(device, shown.center(), PORTRAIT, 3.0).unwrap();
        assert!((by_label.0 - by_point.0).abs() < 1e-9 && (by_label.1 - by_point.1).abs() < 1e-9);
    }

    /// An app that turns (Safari): the tree is already in the screen's space.
    #[test]
    fn an_app_that_turned_maps_straight_through() {
        let (device, root) = (Orientation::LandscapeRight, Size::new(874.0, 402.0));
        assert_eq!(ax_orientation(device, root), Orientation::LandscapeRight);
        let pts = display_points(device, PORTRAIT, 3.0);
        let rect = geometry::Rect::new(100.0, 50.0, 40.0, 20.0);
        let shown = ax_rect_to_display(device, root, rect, pts);
        assert!((shown.x - 100.0).abs() < 1e-9 && (shown.y - 50.0).abs() < 1e-9 && (shown.w - 40.0).abs() < 1e-9);
        assert_eq!(ax_orientation(Orientation::Portrait, Size::new(402.0, 874.0)), Orientation::Portrait);
    }

    #[test]
    fn points_map_to_portrait_in_every_orientation() {
        // The centre is the centre everywhere.
        for o in [Orientation::Portrait, Orientation::PortraitUpsideDown, Orientation::LandscapeLeft, Orientation::LandscapeRight] {
            let pts = display_points(o, PORTRAIT, 3.0);
            let (x, y) = points_to_portrait(o, (pts.w / 2.0, pts.h / 2.0), PORTRAIT, 3.0).unwrap();
            assert!((x - 0.5).abs() < 1e-9 && (y - 0.5).abs() < 1e-9, "{o:?}");
        }
        // Portrait: the top-left point quarter is the portrait quarter.
        assert_eq!(points_to_portrait(Orientation::Portrait, (100.5, 218.5), PORTRAIT, 3.0), Some((0.25, 0.25)));
        // Landscape: the screen is 874×402 pt, and the display's top-left maps
        // where `display_to_portrait` says.
        let pts = display_points(Orientation::LandscapeRight, PORTRAIT, 3.0);
        assert_eq!((pts.w, pts.h), (874.0, 402.0));
        assert_eq!(
            points_to_portrait(Orientation::LandscapeRight, (0.0, 0.0), PORTRAIT, 3.0),
            Some(geometry::display_to_portrait(Orientation::LandscapeRight, (0.0, 0.0)))
        );
    }

    #[test]
    fn a_point_off_the_screen_is_refused_not_clamped() {
        assert_eq!(points_to_portrait(Orientation::Portrait, (403.0, 10.0), PORTRAIT, 3.0), None);
        assert_eq!(points_to_portrait(Orientation::Portrait, (-1.0, 10.0), PORTRAIT, 3.0), None);
        // In landscape, x up to 874 is on screen.
        assert!(points_to_portrait(Orientation::LandscapeLeft, (800.0, 10.0), PORTRAIT, 3.0).is_some());
    }

    #[test]
    fn a_swipe_ends_on_its_target() {
        let path = swipe_path((0.0, 0.0), (100.0, 50.0), Duration::from_millis(160));
        assert_eq!(path.len(), 10);
        assert_eq!(*path.last().unwrap(), (100.0, 50.0));
        assert_eq!(swipe_path((0.0, 0.0), (1.0, 1.0), Duration::ZERO), vec![(1.0, 1.0)]);
    }

    #[test]
    fn urls_need_a_safe_scheme() {
        for ok in ["https://example.com", "http://localhost:3000/a", "myapp://open?id=1", "maps:q=cafe"] {
            assert_eq!(check_url(ok), Ok(()), "{ok}");
        }
        for bad in ["file:///etc/passwd", "FILE:///x", "javascript:alert(1)", "example.com", "://x", "1http://x", "https:"] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn install_paths_stay_inside_the_worktree_or_derived_data() {
        let root = tempfile::tempdir().unwrap();
        let (worktree, derived, elsewhere) =
            (root.path().join("wt"), root.path().join("dd"), root.path().join("elsewhere"));
        for dir in [&worktree, &derived, &elsewhere] {
            std::fs::create_dir_all(dir.join("Build/App.app")).unwrap();
        }
        let app = |d: &Path| d.join("Build/App.app");
        assert!(check_install_path(&app(&worktree), &worktree, Some(&derived)).is_ok());
        assert!(check_install_path(&app(&derived), &worktree, Some(&derived)).is_ok());
        assert_eq!(check_install_path(&app(&elsewhere), &worktree, Some(&derived)), Err(InstallPathError::Outside));
        assert_eq!(check_install_path(&app(&derived), &worktree, None), Err(InstallPathError::Outside));
        // `..` and symlinks are resolved before the check.
        let sneaky = worktree.join("../elsewhere/Build/App.app");
        assert_eq!(check_install_path(&sneaky, &worktree, None), Err(InstallPathError::Outside));
        std::os::unix::fs::symlink(app(&elsewhere), worktree.join("Link.app")).unwrap();
        assert_eq!(check_install_path(&worktree.join("Link.app"), &worktree, None), Err(InstallPathError::Outside));
        // Not an app bundle.
        assert_eq!(check_install_path(&worktree.join("Build"), &worktree, None), Err(InstallPathError::NotAnApp));
        assert_eq!(check_install_path(&worktree.join("missing.app"), &worktree, None), Err(InstallPathError::NotAnApp));
    }

    #[test]
    fn png_size_reads_the_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&402u32.to_be_bytes());
        png.extend_from_slice(&874u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((402, 874)));
        assert_eq!(png_size(b"GIF89a"), None);
    }
}
