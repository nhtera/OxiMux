//! Binary-content detection + previewable-image MIME lookup.
//!
//! `EditorView::new` reads a file as bytes (rather than `read_to_string`)
//! so a non-UTF-8 sequence does not silently produce an empty buffer. The
//! decision tree:
//!
//! 1. Sniff the first 8 KiB for a NUL byte. NUL in the first chunk is
//!    `git`'s own "is this binary" heuristic — matches what other tools
//!    expect for diff/grep filtering and is cheap (single linear scan).
//! 2. If NUL found → binary. If the extension is in the previewable-image
//!    table, route to the image viewer; otherwise show a generic
//!    "Binary file — cannot display" placeholder.
//! 3. If no NUL and the bytes round-trip through `String::from_utf8` →
//!    text. Render in the normal code-editor `InputState`.
//! 4. If no NUL but `from_utf8` fails (UTF-16 BOM, latin-1, etc.) →
//!    still surface as binary. UTF-16 contains NUL bytes in ASCII ranges
//!    so it actually trips rule 1; the fall-through here covers other
//!    encodings that have no NUL but aren't UTF-8 either.
//!
//! Kept in its own module (rather than inlined in `editor_view.rs`) so the
//! sniffer is unit-testable without spinning up a GPUI runtime, and so the
//! editor view file stays close to the 200-line modularization guideline.

use std::path::Path;

/// Number of bytes inspected for NUL when deciding text vs. binary.
/// Matches git's default — see `xdiff/xutils.c::buffer_is_binary`.
const BINARY_SNIFF_BYTES: usize = 8192;

/// `true` when the leading sniff window of `bytes` contains at least one
/// NUL byte. Designed to be cheap (single linear scan, bounded length)
/// so it runs on every file open without measurable cost.
///
/// Empty input is treated as text — an empty file is a 0-byte text file,
/// not a binary blob.
pub fn is_binary_buffer(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    window.contains(&0)
}

/// MIME type for previewable image extensions, or `None` when the file is
/// not in the renderable image set. Lowercased extension compare —
/// `IMG.PNG` and `img.png` are equivalent.
///
/// `svg` is included because GPUI's `img(...)` element can rasterize SVGs
/// loaded from disk. Animated formats (`apng`, `webp` animation) render as
/// the first frame.
pub fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension().and_then(|s| s.to_str())?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/x-icon"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Convenience for callers that only care whether an image preview is
/// available — equivalent to `image_mime_for_path(path).is_some()`.
pub fn is_previewable_image(path: &Path) -> bool {
    image_mime_for_path(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn empty_buffer_is_text() {
        assert!(!is_binary_buffer(&[]));
    }

    #[test]
    fn ascii_buffer_is_text() {
        assert!(!is_binary_buffer(b"hello world\nfoo bar\n"));
    }

    #[test]
    fn nul_byte_in_first_chunk_is_binary() {
        let mut buf = vec![b'a'; 100];
        buf[42] = 0;
        assert!(is_binary_buffer(&buf));
    }

    #[test]
    fn nul_byte_just_outside_window_is_text() {
        // Edge case: NUL right past the sniff window must NOT trip the
        // heuristic. Confirms the bounded scan actually stops at
        // BINARY_SNIFF_BYTES.
        let mut buf = vec![b'a'; BINARY_SNIFF_BYTES + 16];
        buf[BINARY_SNIFF_BYTES + 4] = 0;
        assert!(!is_binary_buffer(&buf));
    }

    #[test]
    fn nul_byte_at_last_window_position_is_binary() {
        let mut buf = vec![b'a'; BINARY_SNIFF_BYTES + 16];
        buf[BINARY_SNIFF_BYTES - 1] = 0;
        assert!(is_binary_buffer(&buf));
    }

    #[test]
    fn utf8_multibyte_is_text() {
        // Non-ASCII multi-byte UTF-8 must not false-positive as binary.
        assert!(!is_binary_buffer("こんにちは — 日本語".as_bytes()));
    }

    #[test]
    fn png_extension_maps_to_image_mime() {
        assert_eq!(image_mime_for_path(&PathBuf::from("logo.png")), Some("image/png"));
        assert_eq!(image_mime_for_path(&PathBuf::from("LOGO.PNG")), Some("image/png"));
    }

    #[test]
    fn jpeg_aliases_share_mime() {
        assert_eq!(image_mime_for_path(&PathBuf::from("a.jpg")), Some("image/jpeg"));
        assert_eq!(image_mime_for_path(&PathBuf::from("a.jpeg")), Some("image/jpeg"));
    }

    #[test]
    fn svg_is_previewable() {
        assert!(is_previewable_image(&PathBuf::from("icon.svg")));
    }

    #[test]
    fn unknown_extension_is_not_image() {
        assert_eq!(image_mime_for_path(&PathBuf::from("data.bin")), None);
        assert!(!is_previewable_image(&PathBuf::from("data.bin")));
    }

    #[test]
    fn extensionless_is_not_image() {
        assert_eq!(image_mime_for_path(&PathBuf::from("Makefile")), None);
    }
}
