//! Renders a pairing ticket as a scannable QR image.
//!
//! The payload is the ticket's `oximux://connect?ticket=…` deep link, so scanning
//! it with the phone camera can hand off directly to the app. Encoding is done
//! here as raw PNG bytes rather than via `qrcode`'s own image renderer, so this
//! crate's `image` version stays the only one in the graph.
//!
//! The bytes carry the handshake secret (that is the point of the code — it is the
//! bearer credential a phone proves possession of), so the rendered image is shown
//! on screen and never written to disk or logged.

use image::{ImageFormat, Luma};
use qrcode::{Color, QrCode};

/// Modules of white margin around the code. The QR spec requires 4; without it
/// many scanners fail to locate the finder patterns against a dark UI.
const QUIET_ZONE: usize = 4;

/// Encode `data` as a PNG QR code, `scale` device pixels per module.
///
/// `None` when the payload is too large for any QR version, which for a pairing
/// URL should not happen — the caller degrades to showing the endpoint id instead
/// of panicking.
pub(super) fn qr_png(data: &str, scale: usize) -> Option<Vec<u8>> {
    let scale = scale.max(1);
    let code = QrCode::new(data.as_bytes()).ok()?;
    let modules = code.to_colors();
    let width = code.width();

    let side = (width + QUIET_ZONE * 2) * scale;
    // Start all-white so the quiet zone needs no special casing; only dark
    // modules are painted in.
    let mut img = image::ImageBuffer::from_pixel(side as u32, side as u32, Luma([255u8]));
    for (i, color) in modules.iter().enumerate() {
        if *color != Color::Dark {
            continue;
        }
        let (mx, my) = (i % width, i / width);
        for dy in 0..scale {
            for dx in 0..scale {
                let x = ((mx + QUIET_ZONE) * scale + dx) as u32;
                let y = ((my + QUIET_ZONE) * scale + dy) as u32;
                img.put_pixel(x, y, Luma([0u8]));
            }
        }
    }

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::{QUIET_ZONE, qr_png};
    use qrcode::QrCode;

    /// The pairing deep link encodes, and the PNG is a real image of the expected
    /// size — `width + 2 quiet zones`, scaled.
    #[test]
    fn encodes_a_pairing_url_at_the_expected_size() {
        let url = "oximux://connect?ticket=AAAABBBBCCCCDDDD";
        let scale = 4;
        let png = qr_png(url, scale).expect("encodes");

        let decoded = image::load_from_memory(&png).expect("valid PNG").to_luma8();
        let modules = QrCode::new(url.as_bytes()).unwrap().width();
        let expected = ((modules + QUIET_ZONE * 2) * scale) as u32;
        assert_eq!(decoded.dimensions(), (expected, expected));
    }

    /// The margin is actually white all the way around — a scanner needs it to
    /// find the finder patterns.
    #[test]
    fn leaves_a_white_quiet_zone_on_every_edge() {
        let png = qr_png("oximux://connect?ticket=X", 3).expect("encodes");
        let img = image::load_from_memory(&png).expect("valid PNG").to_luma8();
        let (w, h) = img.dimensions();
        let margin = (QUIET_ZONE * 3) as u32;

        for i in 0..w {
            for edge in [0, h - 1] {
                assert_eq!(img.get_pixel(i, edge).0[0], 255, "top/bottom edge is white");
            }
            assert_eq!(img.get_pixel(i, margin - 1).0[0], 255, "inside the top quiet zone");
        }
        for j in 0..h {
            for edge in [0, w - 1] {
                assert_eq!(img.get_pixel(edge, j).0[0], 255, "left/right edge is white");
            }
        }
    }

    /// Dark modules are actually painted — a blank image would "look" fine at the
    /// size assertions above but scan as nothing.
    #[test]
    fn paints_dark_modules() {
        let png = qr_png("oximux://connect?ticket=Y", 2).expect("encodes");
        let img = image::load_from_memory(&png).expect("valid PNG").to_luma8();
        let dark = img.pixels().filter(|p| p.0[0] == 0).count();
        assert!(dark > 0, "the code has dark modules");
        let total = (img.width() * img.height()) as usize;
        assert!(dark < total / 2, "but is not mostly dark: {dark}/{total}");
    }

    /// The load-bearing test: a real QR decoder reads the rendered image back and
    /// gets the exact payload. Size/darkness assertions can pass on an image that
    /// scans as nothing; this is what proves a phone can actually pair from it.
    #[test]
    fn the_rendered_code_decodes_back_to_the_payload() {
        let url = "oximux://connect?ticket=k7Qm2xR9vB3nL5pT8wZ1yA4cE6gH0jK2";
        let png = qr_png(url, 6).expect("encodes");

        let img = image::load_from_memory(&png).expect("valid PNG").to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1, "exactly one code is found in the image");

        let (_meta, content) = grids[0].decode().expect("decodes");
        assert_eq!(content, url, "the scanned payload is the pairing deep link");
    }

    /// Scale changes the pixel size without changing the module count.
    #[test]
    fn scale_multiplies_the_pixel_size_only() {
        let url = "oximux://connect?ticket=ZZZZ";
        let small = image::load_from_memory(&qr_png(url, 2).unwrap()).unwrap();
        let large = image::load_from_memory(&qr_png(url, 6).unwrap()).unwrap();
        assert_eq!(large.width(), small.width() * 3);
    }
}
