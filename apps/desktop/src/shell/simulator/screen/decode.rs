//! One helper frame (JPEG) → a GPUI image, on the background executor.
//! zune-jpeg writes BGRA — what GPUI uploads — straight from YCbCr, so no
//! per-pixel pass follows the decode.

use std::sync::Arc;

use gpui::RenderImage;
use oximux_simulator::protocol::Frame;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Largest frame side we accept; the helper's frames are the device screen
/// (an iPad Pro 13" is 2752 px tall), so anything bigger is corrupt.
const MAX_SIDE: usize = 4096;

/// Decode `frame` into an image ready to paint.
pub(super) fn decode(frame: &Frame) -> Result<Arc<RenderImage>, String> {
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::BGRA)
        .set_max_width(MAX_SIDE)
        .set_max_height(MAX_SIDE);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&frame.jpeg), options);
    let pixels = decoder.decode().map_err(|e| format!("frame did not decode: {e:?}"))?;
    let (w, h) = decoder.dimensions().ok_or("frame has no size")?;
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, pixels).ok_or("frame size and data disagree")?;
    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_jpeg_to_bgra_and_rejects_garbage() {
        let mut jpeg = Vec::new();
        let img = image::RgbImage::from_pixel(6, 4, image::Rgb([200, 10, 20]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let image = decode(&Frame { width: 6, height: 4, jpeg }).unwrap();
        let size = image.size(0);
        assert_eq!((size.width.0, size.height.0), (6, 4));
        let bgra = image.as_bytes(0).unwrap();
        // Mostly red in, so blue-first out: B low, R high, opaque.
        assert!(bgra[0] < 60 && bgra[2] > 150 && bgra[3] == 255, "{:?}", &bgra[..4]);
        assert!(decode(&Frame { width: 1, height: 1, jpeg: vec![0xff, 0xd8, 0, 1] }).is_err());
    }
}
