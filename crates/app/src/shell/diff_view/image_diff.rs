//! Image-binary preview support for the diff viewer.
//!
//! Git reports image files (and any other binary) as `DiffStatus::Binary`
//! with no patch body. Rather than showing only a "Binary file (body
//! suppressed)" notice, the diff view renders the actual picture — the
//! "before" (HEAD) and "after" (working tree) sides side by side, the way
//! VS Code and the reference editors do.
//!
//! This module owns the format-agnostic pieces:
//!   - [`is_image_path`] / [`gpui_format_for`]: decide whether a path is a
//!     previewable image and which `gpui::ImageFormat` decodes it.
//!   - [`image_dimensions`]: read pixel width/height from the file header
//!     only (no full decode) for the common formats, so the caption can show
//!     `W × H`. Returns `None` for formats it doesn't parse — the caller then
//!     shows the byte size alone.
//!   - [`human_size`]: format a byte count as `B` / `KB` / `MB`.
//!   - [`ImageSide`] / [`ImageDiffData`]: the decoded-blob payload the async
//!     fetch stores per file and the renderer paints.
//!
//! The actual GPUI decode happens lazily inside the `img()` element (keyed by
//! the `gpui::Image` id), so building an `ImageSide` is cheap — it only wraps
//! the raw bytes and reads the tiny dimension header.

use std::path::Path;
use std::sync::Arc;

use gpui::{Image, ImageFormat};

/// One side (before or after) of an image diff: the raw-bytes image wrapped
/// for GPUI plus the metadata the caption shows. `width`/`height` are `None`
/// when the format's header parser isn't implemented (the caption then drops
/// the dimensions and shows the size only).
#[derive(Clone)]
pub struct ImageSide {
    pub image: Arc<Image>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: usize,
}

impl ImageSide {
    /// Build a side from raw file bytes + the decode format. Reads the
    /// dimension header before handing the bytes to `Image::from_bytes`
    /// (which takes ownership), so no extra copy is made for the parse.
    pub fn from_bytes(format: ImageFormat, bytes: Vec<u8>) -> Self {
        let (width, height) = match image_dimensions(format, &bytes) {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        let len = bytes.len();
        ImageSide {
            image: Arc::new(Image::from_bytes(format, bytes)),
            width,
            height,
            bytes: len,
        }
    }

    /// `W × H · 12.3 KB` caption, dropping the dimensions when unknown.
    pub fn caption(&self) -> String {
        match (self.width, self.height) {
            (Some(w), Some(h)) => format!("{w} × {h} · {}", human_size(self.bytes)),
            _ => human_size(self.bytes),
        }
    }
}

/// The decoded image blobs for one image-binary file: the `HEAD` side and the
/// working-tree side. Either is `None` when that side is absent — added/
/// untracked files have no `old`, deleted files have no `new`.
#[derive(Clone)]
pub struct ImageDiffData {
    pub old: Option<ImageSide>,
    pub new: Option<ImageSide>,
}

/// Extensions GPUI can decode + render as an inline image preview.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "svg",
];

/// Whether `path` names a previewable image, by lowercased extension.
pub fn is_image_path(path: &Path) -> bool {
    gpui_format_for(path).is_some()
}

/// Map a path's extension to the `gpui::ImageFormat` that decodes it, or
/// `None` when the extension isn't a previewable image.
pub fn gpui_format_for(path: &Path) -> Option<ImageFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    // Guard the allow-list so a renamed `.png.bak` never resolves an image.
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    Some(match ext.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "ico" => ImageFormat::Ico,
        "tif" | "tiff" => ImageFormat::Tiff,
        "svg" => ImageFormat::Svg,
        _ => return None,
    })
}

/// Format a byte count compactly: `498 B`, `12.3 KB`, `2.1 MB`.
pub fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

/// Read an image's pixel dimensions from its header bytes only (no full
/// decode), for the formats with a fixed/easily-scanned header. Returns
/// `None` for SVG/ICO/TIFF and on any malformed/short header — the caller
/// falls back to showing the byte size alone.
pub fn image_dimensions(format: ImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::Bmp => bmp_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Webp => webp_dimensions(bytes),
        // ICO/TIFF/SVG: header layout is variable or vector — size only.
        _ => None,
    }
}

/// PNG: 8-byte signature, then the IHDR chunk carrying width/height as
/// big-endian u32 at fixed offsets 16 and 20.
fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if b.len() < 24 || &b[0..8] != SIG || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    Some((w, h))
}

/// GIF: "GIF87a"/"GIF89a" then logical-screen width/height as little-endian
/// u16 at offsets 6 and 8.
fn gif_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 || (&b[0..6] != b"GIF87a" && &b[0..6] != b"GIF89a") {
        return None;
    }
    let w = u16::from_le_bytes(b[6..8].try_into().ok()?);
    let h = u16::from_le_bytes(b[8..10].try_into().ok()?);
    Some((w as u32, h as u32))
}

/// BMP: "BM" then the DIB header's width/height as little-endian i32 at
/// offsets 18 and 22. Height may be negative (top-down rows) — use the
/// magnitude.
fn bmp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 26 || &b[0..2] != b"BM" {
        return None;
    }
    let w = i32::from_le_bytes(b[18..22].try_into().ok()?);
    let h = i32::from_le_bytes(b[22..26].try_into().ok()?);
    Some((w.unsigned_abs(), h.unsigned_abs()))
}

/// JPEG: scan marker segments from the SOI until a Start-Of-Frame marker
/// (SOF0..SOF15, excluding the non-frame C4/C8/CC), whose payload carries
/// height then width as big-endian u16.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < b.len() {
        // Markers are 0xFF followed by a non-0x00, non-0xFF type byte.
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        if marker == 0xff || marker == 0x00 {
            i += 1;
            continue;
        }
        // Standalone markers (RSTn, SOI, EOI) carry no length payload.
        if (0xd0..=0xd9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        // SOF markers carry the frame size. C4=DHT, C8=JPG, CC=DAC are not
        // frame headers and must be skipped like any other segment.
        let is_sof = (0xc0..=0xcf).contains(&marker)
            && marker != 0xc4
            && marker != 0xc8
            && marker != 0xcc;
        if is_sof {
            // segment: [marker][len:2][precision:1][height:2][width:2]…
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
            return Some((w as u32, h as u32));
        }
        i += 2 + len;
    }
    None
}

/// WebP: a RIFF/WEBP container whose first chunk is `VP8 ` (lossy), `VP8L`
/// (lossless), or `VP8X` (extended). Each encodes the canvas size at a
/// different offset within the header.
fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 30 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    match &b[12..16] {
        b"VP8 " => {
            // Lossy: keyframe start code 0x9d 0x01 0x2a at offset 23, then
            // 14-bit width and height (little-endian) follow.
            if b.len() < 30 || b[23] != 0x9d || b[24] != 0x01 || b[25] != 0x2a {
                return None;
            }
            let w = (u16::from_le_bytes([b[26], b[27]]) & 0x3fff) as u32;
            let h = (u16::from_le_bytes([b[28], b[29]]) & 0x3fff) as u32;
            Some((w, h))
        }
        b"VP8L" => {
            // Lossless: 0x2f signature at offset 20, then 14-bit (width-1) and
            // (height-1) packed across the next 4 bytes.
            if b.len() < 25 || b[20] != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            let w = (bits & 0x3fff) + 1;
            let h = ((bits >> 14) & 0x3fff) + 1;
            Some((w, h))
        }
        b"VP8X" => {
            // Extended: canvas (width-1) and (height-1) as 24-bit
            // little-endian at offsets 24 and 27.
            if b.len() < 30 {
                return None;
            }
            let w = (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16) + 1;
            let h = (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16) + 1;
            Some((w, h))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recognizes_image_extensions_case_insensitively() {
        assert!(is_image_path(Path::new("a/logo.PNG")));
        assert!(is_image_path(Path::new("photo.jpeg")));
        assert!(is_image_path(Path::new("anim.gif")));
        assert!(is_image_path(Path::new("icon.svg")));
        assert!(!is_image_path(Path::new("data.bin")));
        assert!(!is_image_path(Path::new("notes.txt")));
        assert!(!is_image_path(Path::new("no_extension")));
    }

    #[test]
    fn maps_extension_to_gpui_format() {
        assert!(matches!(gpui_format_for(Path::new("x.png")), Some(ImageFormat::Png)));
        assert!(matches!(gpui_format_for(Path::new("x.JPG")), Some(ImageFormat::Jpeg)));
        assert!(matches!(gpui_format_for(Path::new("x.webp")), Some(ImageFormat::Webp)));
        assert!(gpui_format_for(Path::new("x.zip")).is_none());
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn png_header_dimensions() {
        // 8-byte sig + IHDR length(4) + "IHDR" + width(4) + height(4).
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        b.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&800u32.to_be_bytes());
        b.extend_from_slice(&600u32.to_be_bytes());
        assert_eq!(image_dimensions(ImageFormat::Png, &b), Some((800, 600)));
    }

    #[test]
    fn gif_header_dimensions() {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&320u16.to_le_bytes());
        b.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(image_dimensions(ImageFormat::Gif, &b), Some((320, 240)));
    }

    #[test]
    fn bmp_header_dimensions() {
        let mut b = vec![0u8; 26];
        b[0] = b'B';
        b[1] = b'M';
        b[18..22].copy_from_slice(&100i32.to_le_bytes());
        b[22..26].copy_from_slice(&(-50i32).to_le_bytes()); // top-down
        assert_eq!(image_dimensions(ImageFormat::Bmp, &b), Some((100, 50)));
    }

    #[test]
    fn jpeg_scans_to_sof() {
        // SOI, an APP0 segment to skip, then SOF0 with height/width.
        let mut b = vec![0xff, 0xd8];
        b.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]); // APP0 len=4
        b.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]); // SOF0, len, precision
        b.extend_from_slice(&480u16.to_be_bytes()); // height
        b.extend_from_slice(&640u16.to_be_bytes()); // width
        b.extend_from_slice(&[0u8; 4]); // remaining component bytes
        assert_eq!(image_dimensions(ImageFormat::Jpeg, &b), Some((640, 480)));
    }

    #[test]
    fn short_or_wrong_header_is_none() {
        assert_eq!(image_dimensions(ImageFormat::Png, &[0u8; 4]), None);
        assert_eq!(image_dimensions(ImageFormat::Gif, b"NOTGIF"), None);
        assert_eq!(image_dimensions(ImageFormat::Ico, &[0u8; 64]), None);
    }
}
