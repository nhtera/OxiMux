//! Image-attachment helpers for the agent chat.
//!
//! Turns a picked / pasted / dropped image into a wire-ready [`ChatImage`]
//! (standard-base64 data + a Claude-supported media type) paired with a decoded
//! [`gpui::Image`] for the thumbnail, and decodes a persisted [`ChatImage`] back
//! for rendering in restored transcripts.
//!
//! Claude's Messages API accepts only png/jpeg/gif/webp, so any other encoding
//! (a TIFF clipboard image, a bmp file, …) is transcoded to PNG here — the agent
//! and the persisted blob then only ever see the four supported types.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gpui::{Image, ImageFormat};
use oximux_agents::thread::ChatImage;

/// The image media types Claude accepts as base64 blocks.
fn is_supported(f: ImageFormat) -> bool {
    matches!(f, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp)
}

/// A staged attachment held in the composer: the wire/persist form plus a
/// decoded image for the chip thumbnail — decoded ONCE here, not per keystroke.
#[derive(Clone)]
pub struct PendingImage {
    pub chat: ChatImage,
    pub render: Arc<Image>,
}

/// Sniff the encoded image's format from its magic bytes, mapping the `image`
/// crate's format to gpui's. `None` for anything we can't name (it'll be
/// transcoded to PNG upstream).
fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some(ImageFormat::Png),
        image::ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
        image::ImageFormat::Gif => Some(ImageFormat::Gif),
        image::ImageFormat::WebP => Some(ImageFormat::Webp),
        image::ImageFormat::Bmp => Some(ImageFormat::Bmp),
        image::ImageFormat::Tiff => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// Re-encode arbitrary image bytes as PNG (for formats Claude rejects).
fn transcode_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

/// Build a [`PendingImage`] from encoded image bytes. The actual bytes are
/// sniffed (the `hint`, e.g. a clipboard tag, is only a fallback); an
/// unsupported or unrecognized encoding is transcoded to PNG so the resulting
/// [`ChatImage`] always carries png/jpeg/gif/webp.
pub fn pending_from_bytes(bytes: Vec<u8>, hint: Option<ImageFormat>) -> Option<PendingImage> {
    let (fmt, bytes) = match sniff(&bytes).or(hint) {
        Some(f) if is_supported(f) => (f, bytes),
        _ => (ImageFormat::Png, transcode_to_png(&bytes)?),
    };
    let chat = ChatImage { media_type: fmt.mime_type().to_string(), data: STANDARD.encode(&bytes) };
    let render = Arc::new(Image::from_bytes(fmt, bytes));
    Some(PendingImage { chat, render })
}

/// Read an image file and stage it. `None` if the file can't be read or decoded.
pub fn pending_from_path(path: &Path) -> Option<PendingImage> {
    let bytes = std::fs::read(path).ok()?;
    pending_from_bytes(bytes, None)
}

/// Whether a dropped/picked path looks like an image we can attach (cheap
/// extension gate before reading the file).
pub fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff")
    )
}

/// Decode a persisted [`ChatImage`] into a renderable image (for transcript
/// bubbles on restore). `None` if the base64 is corrupt.
pub fn decode_render(chat: &ChatImage) -> Option<Arc<Image>> {
    let bytes = STANDARD.decode(&chat.data).ok()?;
    let fmt = ImageFormat::from_mime_type(&chat.media_type).unwrap_or(ImageFormat::Png);
    Some(Arc::new(Image::from_bytes(fmt, bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 red PNG (valid magic bytes).
    fn red_png() -> Vec<u8> {
        // Minimal but real PNG so `image` can decode it.
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        STANDARD.decode(b64).unwrap()
    }

    #[test]
    fn png_bytes_stage_as_png_without_transcode() {
        let p = pending_from_bytes(red_png(), None).expect("stage png");
        assert_eq!(p.chat.media_type, "image/png");
        // Round-trips: the base64 decodes back to the same bytes we fed in.
        assert_eq!(STANDARD.decode(&p.chat.data).unwrap(), red_png());
    }

    #[test]
    fn decode_render_reads_back_a_staged_image() {
        let p = pending_from_bytes(red_png(), None).unwrap();
        assert!(decode_render(&p.chat).is_some());
    }

    #[test]
    fn garbage_bytes_do_not_panic() {
        assert!(pending_from_bytes(vec![0, 1, 2, 3], None).is_none());
    }
}
