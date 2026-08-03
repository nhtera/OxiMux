//! `xtask icon` — derive the Windows application icon from the macOS one.
//!
//! Windows wants a multi-resolution `.ico`; macOS wants a multi-resolution
//! `.icns`. Rather than keep two hand-made binaries in step by memory, the
//! `.icns` is the source of truth and the `.ico` is generated from it. That is
//! the whole reason this subcommand exists: an icon that drifts between
//! platforms is invisible until someone looks at a taskbar.
//!
//! `xtask icon --check` re-derives the icon and compares bytes, so a change to
//! `assets/AppIcon.icns` that forgets the Windows half fails CI instead of
//! shipping. The comparison is byte-exact, which also means a future `image`
//! bump that changes PNG filtering will trip it — that reads as a false alarm
//! but is cheap to clear: rerun `cargo run -p xtask -- icon` and commit the
//! result.
//!
//! ## Why the frames are encoded two different ways
//!
//! An `.ico` entry is either a PNG or a headerless BMP, and the choice is not
//! stylistic. Every size up to 128 is written as BMP because that is what the
//! platform's own tooling emits and what every consumer back to Windows XP can
//! decode. The 256 entry is written as PNG because a 256×256 BGRA bitmap is
//! 256 KB and the PNG is a tenth of that — which is exactly why PNG entries
//! were added to the format in the first place.
//!
//! ## Why resource ID 1
//!
//! Not arbitrary, and not ours to choose. GPUI's Windows backend calls
//! `LoadImageW(module, PCWSTR(1), IMAGE_ICON, …)` to find the window icon, so
//! the icon that shows in the title bar, the taskbar, and Alt-Tab is whichever
//! one is embedded at ID 1. `apps/desktop/build.rs` writes the `.rc` that puts
//! it there; this file only produces the bytes.

use std::error::Error;
use std::path::Path;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::FilterType;
use image::{ExtendedColorType, RgbaImage};

/// The macOS icon, and the source of truth for both platforms.
const SOURCE: &str = "assets/AppIcon.icns";
/// The derived Windows icon, checked in so a Windows build needs no toolchain
/// beyond cargo.
const OUTPUT: &str = "assets/windows/OxiMux.ico";

/// Sizes Windows actually asks for: 16 in the title bar and tree views, 32 in
/// Alt-Tab and the taskbar, 48 in Explorer's medium view, 256 in its extra-large
/// view and the Start menu. 64 and 128 fill the gap so scaled displays pick a
/// frame to shrink rather than one to blow up.
const SIZES: &[u32] = &[16, 32, 48, 64, 128, 256];

/// The largest size that is written as a BMP frame; above it, PNG.
const LARGEST_BMP: u32 = 128;

/// One PNG member of the icns: its square dimension, and its bytes.
type Member<'a> = (u32, &'a [u8]);

pub fn run(check: bool) -> Result<(), Box<dyn Error>> {
    let root = crate::workspace_root()?;
    let source = root.join(SOURCE);
    let output = root.join(OUTPUT);

    let icns = std::fs::read(&source).map_err(|e| format!("reading {}: {e}", source.display()))?;
    let generated = render(&icns)?;

    if check {
        return compare(&output, &generated);
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output, &generated)?;
    println!(
        "xtask icon: wrote {} ({} bytes, {} frames) from {}",
        OUTPUT,
        generated.len(),
        SIZES.len(),
        SOURCE
    );
    Ok(())
}

/// Byte-compare the checked-in icon against a freshly derived one.
fn compare(output: &Path, generated: &[u8]) -> Result<(), Box<dyn Error>> {
    let existing = std::fs::read(output).map_err(|e| {
        format!(
            "{} is missing or unreadable ({e}) — run `cargo run -p xtask -- icon`",
            OUTPUT
        )
    })?;
    if existing == generated {
        println!("xtask icon: {OUTPUT} is up to date with {SOURCE}");
        return Ok(());
    }
    Err(format!(
        "{OUTPUT} does not match what {SOURCE} generates \
         ({} bytes on disk, {} bytes generated).\n\
         Run `cargo run -p xtask -- icon` and commit the result.",
        existing.len(),
        generated.len(),
    )
    .into())
}

/// Derive the full `.ico` byte stream from `.icns` bytes.
fn render(icns: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let sources = png_members(icns)?;
    if sources.is_empty() {
        return Err(format!("{SOURCE} contains no PNG members — unsupported icns variant").into());
    }

    let mut frames = Vec::with_capacity(SIZES.len());
    for &size in SIZES {
        let png = best_source(&sources, size);
        let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
        let scaled = if decoded.width() == size && decoded.height() == size {
            decoded
        } else {
            image::imageops::resize(&decoded, size, size, FilterType::Lanczos3)
        };

        let frame = if size <= LARGEST_BMP {
            IcoFrame::with_encoded(bmp_frame(&scaled), size, size, ExtendedColorType::Rgba8)?
        } else {
            IcoFrame::as_png(scaled.as_raw(), size, size, ExtendedColorType::Rgba8)?
        };
        frames.push(frame);
    }

    let mut out = Vec::new();
    IcoEncoder::new(&mut out).encode_images(&frames)?;
    Ok(out)
}

/// Pick the member to scale from: an exact match when the icns has one,
/// otherwise the smallest member larger than the target.
///
/// Downscaling only. Scaling *up* from a smaller member would produce a blurry
/// frame while a sharper source sat unused in the same file.
fn best_source<'a>(sources: &[Member<'a>], size: u32) -> &'a [u8] {
    sources
        .iter()
        .filter(|(w, _)| *w >= size)
        .min_by_key(|(w, _)| *w)
        .or_else(|| sources.iter().max_by_key(|(w, _)| *w))
        .map(|(_, png)| *png)
        .expect("caller rejects an empty source list")
}

/// The PNG-encoded members of an icns file, as `(width, bytes)`.
///
/// An icns is a flat sequence of `[4-byte type][4-byte big-endian length]body`
/// chunks after an 8-byte header, where the length *includes* the 8 bytes of
/// chunk header. Members come in several encodings; the legacy raw-ARGB ones
/// (`ic04`, `ic05`) and the `info` metadata blob are skipped rather than
/// decoded, because every size worth having is also present as a PNG.
fn png_members(icns: &[u8]) -> Result<Vec<Member<'_>>, Box<dyn Error>> {
    if icns.len() < 8 || &icns[0..4] != b"icns" {
        return Err(format!("{SOURCE} is not an icns file").into());
    }

    let mut members = Vec::new();
    let mut offset = 8usize;
    while offset + 8 <= icns.len() {
        let length = u32::from_be_bytes(icns[offset + 4..offset + 8].try_into()?) as usize;
        // A zero/short length would loop forever; a long one would run past the
        // buffer. Both mean the file is not what it claims to be.
        if length < 8 || offset + length > icns.len() {
            return Err(format!(
                "{SOURCE} has a malformed chunk at offset {offset} (length {length})"
            )
            .into());
        }
        let body = &icns[offset + 8..offset + length];
        if let Some(width) = png_width(body) {
            members.push((width, body));
        }
        offset += length;
    }
    Ok(members)
}

/// The width of a PNG, or `None` if these bytes are not a square PNG.
///
/// The IHDR payload starts at offset 16 in every PNG, so width and height are
/// at fixed offsets — no chunk walk needed. Non-square members are rejected
/// because an `.ico` frame must be square and silently squashing one would be
/// worse than not using it.
fn png_width(body: &[u8]) -> Option<u32> {
    if body.len() < 24 || &body[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(body[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(body[20..24].try_into().ok()?);
    (width == height).then_some(width)
}

/// Encode one frame as the headerless BMP an `.ico` entry expects.
///
/// Three details are easy to get wrong and produce a blank or upside-down icon
/// rather than an error:
///
/// * `biHeight` is *twice* the real height. An icon bitmap is a colour image
///   stacked on a 1-bit AND mask, and the header describes both.
/// * Rows run bottom-up, and pixels are BGRA, not RGBA.
/// * The AND mask is still required at 32bpp even though the alpha channel is
///   what Windows actually composites with. Writing it all-zero (meaning "every
///   pixel opaque") hands transparency entirely to the alpha channel, which is
///   what modern Windows honours.
fn bmp_frame(img: &RgbaImage) -> Vec<u8> {
    let (width, height) = img.dimensions();
    let mut out = Vec::with_capacity(40 + (width * height * 4) as usize);

    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((height * 2) as i32).to_le_bytes()); // biHeight: colour + mask
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage (0 is legal for BI_RGB)
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // Colour plane: bottom-up BGRA. 32bpp rows are inherently 4-byte aligned,
    // so no padding arithmetic.
    for y in (0..height).rev() {
        for x in 0..width {
            let [r, g, b, a] = img.get_pixel(x, y).0;
            out.extend_from_slice(&[b, g, r, a]);
        }
    }

    // AND mask: 1bpp, each row padded to a 4-byte boundary.
    let mask_stride = width.div_ceil(32) * 4;
    out.resize(out.len() + (mask_stride * height) as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed icns carrying one PNG member of the given size.
    fn icns_with(png: &[u8]) -> Vec<u8> {
        let chunk_len = (png.len() + 8) as u32;
        let total = 8 + chunk_len;
        let mut out = Vec::new();
        out.extend_from_slice(b"icns");
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(b"ic08");
        out.extend_from_slice(&chunk_len.to_be_bytes());
        out.extend_from_slice(png);
        out
    }

    fn square_png(size: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(size, size, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 0x40, 0xff])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    #[test]
    fn members_are_found_by_size() {
        let png = square_png(256);
        let icns = icns_with(&png);
        let members = png_members(&icns).expect("parse");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].0, 256);
    }

    #[test]
    fn a_truncated_chunk_is_an_error_not_a_hang() {
        let png = square_png(32);
        let mut icns = icns_with(&png);
        // Claim far more than the file holds.
        let len_at = 12;
        icns[len_at..len_at + 4].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        assert!(png_members(&icns).is_err());
    }

    #[test]
    fn a_zero_length_chunk_is_an_error_not_a_hang() {
        let png = square_png(32);
        let mut icns = icns_with(&png);
        icns[12..16].copy_from_slice(&0u32.to_be_bytes());
        assert!(png_members(&icns).is_err());
    }

    #[test]
    fn non_square_members_are_skipped() {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(RgbaImage::new(64, 32))
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        assert!(png_width(&bytes).is_none());
    }

    #[test]
    fn the_source_chosen_is_the_smallest_that_does_not_upscale() {
        let small = square_png(32);
        let mid = square_png(128);
        let big = square_png(512);
        let sources = vec![
            (32u32, small.as_slice()),
            (128u32, mid.as_slice()),
            (512u32, big.as_slice()),
        ];
        assert_eq!(best_source(&sources, 128).len(), mid.len());
        assert_eq!(best_source(&sources, 64).len(), mid.len());
        assert_eq!(best_source(&sources, 32).len(), small.len());
        // Nothing large enough: fall back to the largest rather than fail.
        assert_eq!(best_source(&sources, 1024).len(), big.len());
    }

    #[test]
    fn a_bmp_frame_has_the_doubled_height_and_the_mask() {
        let img = RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 4]));
        let bmp = bmp_frame(&img);
        let height = i32::from_le_bytes(bmp[8..12].try_into().unwrap());
        assert_eq!(height, 32, "biHeight must cover colour plane + AND mask");
        // 40 header + 16*16*4 colour + 16 rows of a 4-byte mask stride.
        assert_eq!(bmp.len(), 40 + 16 * 16 * 4 + 16 * 4);
        // BGRA, not RGBA.
        assert_eq!(&bmp[40..44], &[3, 2, 1, 4]);
        // The mask is all zero.
        assert!(bmp[40 + 16 * 16 * 4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn every_declared_size_becomes_a_frame() {
        let icns = icns_with(&square_png(512));
        let ico = render(&icns).expect("render");
        // ICONDIR: reserved, type=1, count.
        assert_eq!(&ico[0..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes(ico[4..6].try_into().unwrap());
        assert_eq!(count as usize, SIZES.len());
    }

    #[test]
    fn the_256_frame_is_a_png_and_the_small_ones_are_not() {
        let icns = icns_with(&square_png(512));
        let ico = render(&icns).expect("render");
        let count = u16::from_le_bytes(ico[4..6].try_into().unwrap()) as usize;
        for i in 0..count {
            let entry = 6 + i * 16;
            // Width is stored as `0 => 256`.
            let declared = ico[entry];
            let offset = u32::from_le_bytes(ico[entry + 12..entry + 16].try_into().unwrap()) as usize;
            let is_png = ico[offset..offset + 8] == *b"\x89PNG\r\n\x1a\n";
            if declared == 0 {
                assert!(is_png, "the 256 frame should be PNG-compressed");
            } else {
                assert!(!is_png, "the {declared} frame should be a BMP");
            }
        }
    }

    /// The real icon, not a fixture: proves the checked-in `.icns` is a shape
    /// this code can actually read, which a synthetic one cannot.
    #[test]
    fn the_shipped_icns_yields_every_size() {
        let root = crate::workspace_root().expect("workspace root");
        let icns = std::fs::read(root.join(SOURCE)).expect("read icns");
        let members = png_members(&icns).expect("parse");
        for &size in SIZES {
            assert!(
                members.iter().any(|(w, _)| *w >= size),
                "no member at or above {size}"
            );
        }
    }
}
