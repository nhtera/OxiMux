//! PDF preview: parse the document once, rasterize one page at a time into a
//! GPUI [`RenderImage`], and draw the breadcrumb page navigator.
//!
//! Page rendering runs off the UI thread. `Pdf` is `Send + Sync`, so the
//! parsed document is shared behind an `Arc` and each render call builds its
//! own scratch `RenderCache` (that cache is `Rc`-based and stays on the thread
//! that made it). A heavy page — a scanned image, a JPEG 2000 stream —
//! therefore does not stall the window; the pane shows "Rendering…" until the
//! bitmap lands. The parse itself (`Pdf::new`, a cross-reference walk) runs
//! synchronously in `decide_content`, alongside the synchronous file read the
//! editor already does on open.
//!
//! The page is painted on an opaque white ground, so every pixel hayro hands
//! back has alpha 255 and premultiplied equals straight. The only conversion
//! before `RenderImage::new` is the RGBA→BGRA swap gpui expects.

use std::path::Path;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, EntityId, InteractiveElement, IntoElement, ParentElement, RenderImage,
    StatefulInteractiveElement as _, Styled, Task, img, px,
};
use gpui_component::{
    Disableable, IconName, Sizable,
    button::{Button, ButtonVariants},
};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings};

use crate::editor_view::EditorView;

/// Bytes inspected for the `%PDF-` header. The spec lets a producer put a
/// few bytes ahead of it, so the sniff is a window rather than a prefix
/// check; 1 KiB is far past anything seen in practice.
const HEADER_SNIFF_BYTES: usize = 1024;

/// Largest pixel edge requested from the rasterizer. Bounds a poster-sized
/// page at a high zoom to something the sprite atlas takes comfortably, and
/// stays well under hayro's `u16` pixmap dimensions.
const MAX_EDGE_PX: f32 = 8192.0;

/// `true` for a `.pdf` extension (case-insensitive). A file named this way
/// is a PDF as far as the user is concerned: a parse failure is reported as
/// such rather than falling back to another viewer.
pub fn has_pdf_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// `true` when a `%PDF-` header appears near the start of the bytes. Catches
/// a PDF with the wrong extension — and a small hand-written one is pure
/// ASCII, so without this it would sail into the text editor. The marker is
/// only a hint: prose can contain it too (this repository's changelog does),
/// so a caller must fall back to the ordinary text/binary path when the parse
/// fails.
pub fn has_pdf_header(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(HEADER_SNIFF_BYTES)];
    window.windows(5).any(|w| w == b"%PDF-")
}

/// A parsed document, shared between the view and its render tasks.
pub struct PdfDocument {
    pdf: Pdf,
}

impl PdfDocument {
    /// Parse `bytes`. The error is hayro's `Debug` form — it is the only
    /// rendering the type offers, and it names the class of failure
    /// (encrypted vs malformed), which is what the user needs to see.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        Pdf::new(bytes)
            .map(|pdf| Self { pdf })
            .map_err(|err| format!("{err:?}"))
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Rasterize page `index` (0-based) at `scale` device pixels per PDF
    /// point, as a BGRA bitmap ready for gpui. `None` when the index is out
    /// of range or the result has no area (a sub-point page at minimum zoom).
    pub fn render_page(&self, index: usize, scale: f32) -> Option<Arc<RenderImage>> {
        let page = self.pdf.pages().get(index)?;
        let (w, h) = page.render_dimensions();
        // Floor first, then cap: the cap must win for an absurdly wide page.
        let scale = scale.max(0.05).min(MAX_EDGE_PX / w.max(h).max(1.0));
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
            ..Default::default()
        };
        let pixmap = hayro::render(
            page,
            &RenderCache::new(),
            &InterpreterSettings::default(),
            &settings,
        );
        let (pw, ph) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
        if pw == 0 || ph == 0 {
            return None;
        }
        let mut bgra = Vec::with_capacity(pw as usize * ph as usize * 4);
        for p in pixmap.data() {
            bgra.extend_from_slice(&[p.b, p.g, p.r, p.a]);
        }
        let buffer = image::RgbaImage::from_raw(pw, ph, bgra)?;
        Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
    }
}

/// State carried by the `EditorContent::Pdf` variant.
pub struct PdfContent {
    pub(crate) doc: Arc<PdfDocument>,
    /// Current page, 0-based. Always `< page_count`.
    pub(crate) page: usize,
    pub(crate) page_count: usize,
    /// The bitmap on screen. `None` before the first render lands or after a
    /// failed one; the body shows a placeholder either way.
    pub(crate) bitmap: Option<Arc<RenderImage>>,
    /// `true` when the last render for the current page produced nothing.
    pub(crate) failed: bool,
    /// `(page, scale × 1000)` of the render most recently requested — either
    /// the one on screen or the one in flight. `ensure_page` is a no-op while
    /// this matches what the view wants, so re-renders only happen on a page
    /// step or a zoom / window-scale change.
    pub(crate) requested: Option<(usize, u32)>,
    /// Bumped per request; a task whose generation is stale drops its result
    /// instead of painting a page the user has already left.
    pub(crate) generation: u64,
    /// The in-flight render. Dropping it cancels the task.
    pub(crate) _render_task: Option<Task<()>>,
}

impl PdfContent {
    pub fn new(doc: Arc<PdfDocument>) -> Self {
        let page_count = doc.page_count();
        Self {
            doc,
            page: 0,
            page_count,
            bitmap: None,
            failed: false,
            requested: None,
            generation: 0,
            _render_task: None,
        }
    }

    /// Render-request key for `page` at `scale`. Quantised so a float that
    /// round-trips through the window's scale factor does not look "new".
    pub(crate) fn request_key(page: usize, scale: f32) -> (usize, u32) {
        (page, (scale * 1000.0).round() as u32)
    }
}

/// `‹ N / M ›` for the breadcrumb row. Buttons disable at the ends so the
/// affordance says where the document stops.
pub fn page_nav(page: usize, count: usize, cx: &Context<EditorView>) -> AnyElement {
    let id = cx.entity_id();
    gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .child(
            Button::new(("pdf-prev", id))
                .ghost()
                .xsmall()
                .icon(IconName::ChevronLeft)
                .tooltip("Previous page (←)")
                .disabled(page == 0)
                .on_click(cx.listener(|view, _, window, cx| view.pdf_step(-1, window, cx))),
        )
        .child(gpui::div().px(px(4.0)).child(format!("{} / {}", page + 1, count)))
        .child(
            Button::new(("pdf-next", id))
                .ghost()
                .xsmall()
                .icon(IconName::ChevronRight)
                .tooltip("Next page (→)")
                .disabled(page + 1 >= count)
                .on_click(cx.listener(|view, _, window, cx| view.pdf_step(1, window, cx))),
        )
        .into_any_element()
}

/// The page body: the current bitmap centered in a scrollable surface, sized
/// in logical pixels so the window's scale factor does not double it. Before
/// the first render (or after a failed one) a muted placeholder stands in.
pub fn page_body(
    content: &PdfContent,
    scale_factor: f32,
    view_id: EntityId,
    muted_fg: gpui::Hsla,
    text_size: f32,
) -> AnyElement {
    let Some(bitmap) = &content.bitmap else {
        let label = if content.failed {
            "This page could not be rendered"
        } else {
            "Rendering…"
        };
        return gpui::div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_size(px(text_size))
            .text_color(muted_fg)
            .child(label)
            .into_any_element();
    };
    let size = bitmap.size(0);
    let scale = scale_factor.max(0.1);
    let (w, h) = (size.width.0 as f32 / scale, size.height.0 as f32 / scale);
    gpui::div()
        .id(("pdf-page-scroll", view_id))
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .child(
            gpui::div()
                .flex()
                .flex_col()
                .items_center()
                .p(px(16.0))
                .child(img(bitmap.clone()).flex_none().w(px(w)).h(px(h))),
        )
        .into_any_element()
}

/// Smallest well-formed document hayro accepts: `pages` empty 200×100 pt
/// pages. Shared by this module's tests and the view tests in `editor_view`.
#[cfg(test)]
pub(crate) fn test_pdf(pages: usize) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            (0..pages)
                .map(|i| format!("{} 0 R", 3 + i))
                .collect::<Vec<_>>()
                .join(" ")
        )
        .into_bytes(),
    ];
    for _ in 0..pages {
        objs.push(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>".to_vec());
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(obj);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_by_extension_case_insensitively() {
        assert!(has_pdf_extension(&PathBuf::from("a.pdf")));
        assert!(has_pdf_extension(&PathBuf::from("A.PDF")));
        assert!(!has_pdf_extension(&PathBuf::from("a.txt")));
        assert!(!has_pdf_extension(&PathBuf::from("pdf")));
    }

    #[test]
    fn detects_header_within_sniff_window_only() {
        // Junk ahead of the header is allowed by the spec.
        let mut bytes = vec![b'\n'; 16];
        bytes.extend_from_slice(b"%PDF-1.7\n");
        assert!(has_pdf_header(&bytes));
        // A header past the window is not sniffed.
        let mut far = vec![b' '; HEADER_SNIFF_BYTES];
        far.extend_from_slice(b"%PDF-1.7");
        assert!(!has_pdf_header(&far));
        assert!(!has_pdf_header(b"plain prose"));
    }

    #[test]
    fn parses_and_renders_a_page_at_scale() {
        let doc = PdfDocument::parse(test_pdf(1)).expect("valid pdf");
        assert_eq!(doc.page_count(), 1);
        let img = doc.render_page(0, 2.0).expect("page 0 renders");
        let size = img.size(0);
        assert_eq!((size.width.0, size.height.0), (400, 200));
        assert!(doc.render_page(1, 1.0).is_none(), "out of range page is None");
    }

    #[test]
    fn rejects_garbage() {
        assert!(PdfDocument::parse(b"not a pdf at all".to_vec()).is_err());
    }

    #[test]
    fn request_key_quantises_scale() {
        assert_eq!(PdfContent::request_key(3, 2.0), (3, 2000));
        assert_eq!(PdfContent::request_key(3, 2.0004), (3, 2000));
        assert_ne!(PdfContent::request_key(3, 2.0), PdfContent::request_key(4, 2.0));
    }
}
