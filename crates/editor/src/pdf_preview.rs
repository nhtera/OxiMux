//! PDF preview: parse the document once, rasterize one page at a time into a
//! GPUI [`RenderImage`], and draw the breadcrumb page navigator.
//!
//! Page rendering runs off the UI thread. `Pdf` is `Send + Sync`, so the
//! parsed document is shared behind an `Arc` and each render call builds its
//! own scratch `RenderCache` (that cache is `Rc`-based and stays on the thread
//! that made it). A heavy page — a scanned image, a JPEG 2000 stream —
//! therefore does not stall the window; the pane shows "Rendering…" until the
//! bitmap lands. A `.pdf` is also read and parsed off the UI thread (see
//! `spawn_pdf_load` in `editor_view`); only a PDF recognised by its header
//! under another extension is parsed inline, from bytes already in hand.
//!
//! The page is painted on an opaque white ground, so every pixel hayro hands
//! back has alpha 255 and premultiplied equals straight. The only conversion
//! before `RenderImage::new` is the RGBA→BGRA swap gpui expects.

use std::path::Path;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, RenderImage,
    ScrollHandle, StatefulInteractiveElement as _, Styled, Task, WeakEntity, img, point, px,
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

/// Largest bitmap requested from the rasterizer, in pixels of area: 16 Mpx
/// is 64 MB of BGRA. A scanner that declares its page box in pixels (2480 ×
/// 3508 pt for an A4 scan) would otherwise ask for 139 MB per page at retina
/// scale — measured, see the Phase 11 follow-up report. A real A4 page at
/// retina is ~2 Mpx and never meets this cap.
const MAX_AREA_PX: f32 = 16_000_000.0;

/// Hard ceiling on either edge — the GPU texture limit on every platform
/// GPUI targets. Only a very elongated page can reach it under the area cap.
const MAX_EDGE_PX: f32 = 16_384.0;

/// Horizontal padding around the page in the pane, in logical px per side.
/// `ensure_pdf_page` subtracts it when fitting the page to the pane width.
pub const PAGE_PADDING_PX: f32 = 16.0;

/// Vertical travel of one ↑/↓ press inside a page, in logical px.
pub const SCROLL_STEP_PX: f32 = 64.0;

/// Overlap kept when PageUp/PageDown scroll by a viewport, in logical px,
/// so the line at the edge stays in view as an anchor.
pub const SCROLL_PAGE_OVERLAP_PX: f32 = 24.0;

/// The width probe reports in steps of this many logical px. The fit scale
/// follows the pane width, so an unquantised report would turn a resize drag
/// into one full rasterization per pixel of travel (a rasterization that has
/// started cannot be cancelled); 32 px keeps a drag to a handful.
pub const PANE_WIDTH_STEP_PX: f32 = 32.0;

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

    /// Size of page `index` in PDF points, after the page's own rotation.
    /// `None` when out of range. What the fit-to-pane scale is derived from.
    pub fn page_size(&self, index: usize) -> Option<(f32, f32)> {
        Some(self.pdf.pages().get(index)?.render_dimensions())
    }

    /// Rasterize page `index` (0-based) at `scale` device pixels per PDF
    /// point, as a BGRA bitmap ready for gpui. `None` when the index is out
    /// of range or the result has no area (a sub-point page at minimum zoom).
    pub fn render_page(&self, index: usize, scale: f32) -> Option<Arc<RenderImage>> {
        let page = self.pdf.pages().get(index)?;
        let (w, h) = page.render_dimensions();
        let scale = clamp_scale(scale, w, h);
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

/// Floor `scale`, then shrink it until the bitmap fits the area cap and the
/// edge cap. Floor first: the caps must win for an absurd page, never the
/// floor. Pure so the arithmetic is testable without a document.
fn clamp_scale(scale: f32, page_w: f32, page_h: f32) -> f32 {
    let (w, h) = (page_w.max(1.0), page_h.max(1.0));
    let mut scale = scale.max(0.05);
    let area = w * h * scale * scale;
    if area > MAX_AREA_PX {
        scale *= (MAX_AREA_PX / area).sqrt();
    }
    let edge = w.max(h) * scale;
    if edge > MAX_EDGE_PX {
        scale *= MAX_EDGE_PX / edge;
    }
    scale
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
    /// Scroll state of the page body. Reset to the top on every page step so
    /// the next page opens at its head, not wherever the last one was left;
    /// the key handler scrolls it for ↑/↓ and PageUp/PageDown.
    pub(crate) scroll: ScrollHandle,
    /// A backwards PageUp wants the *new* page's bottom. The scroll handle
    /// can only aim at what is laid out — still the old page — so the intent
    /// waits here and is applied when the new bitmap lands.
    pub(crate) land_at_bottom: bool,
    /// Width of the page body in logical px, rounded down to a
    /// `PANE_WIDTH_STEP_PX` step, reported by the probe in `page_body` after
    /// layout. `None` until the first frame has laid out — and no render is
    /// requested until then, so an oversized page is never rasterized at a
    /// size the pane cannot show.
    pub(crate) pane_width: Option<f32>,
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
            scroll: ScrollHandle::new(),
            land_at_bottom: false,
            pane_width: None,
        }
    }

    /// Scroll the page body by `dy` logical px (positive = down), clamped to
    /// the content. Returns `false` when already at that edge, so a caller
    /// can turn the press into a page step instead.
    pub(crate) fn scroll_by(&self, dy: f32) -> bool {
        let offset = self.scroll.offset();
        let max_down = f32::from(self.scroll.max_offset().y).max(0.0);
        // gpui offsets grow negative as the content scrolls up.
        let target = (f32::from(offset.y) - dy).clamp(-max_down, 0.0);
        if (target - f32::from(offset.y)).abs() < 0.5 {
            return false;
        }
        self.scroll.set_offset(point(offset.x, px(target)));
        true
    }

    /// One viewport of travel for PageUp/PageDown, less the overlap.
    pub(crate) fn viewport_step(&self) -> f32 {
        (f32::from(self.scroll.bounds().size.height) - SCROLL_PAGE_OVERLAP_PX).max(SCROLL_STEP_PX)
    }

    pub(crate) fn scroll_to_top(&self) {
        self.scroll.set_offset(point(px(0.0), px(0.0)));
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
                .on_click(cx.listener(|view, _, window, cx| {
                    view.pdf_step(-1, window, cx);
                })),
        )
        .child(gpui::div().px(px(4.0)).child(format!("{} / {}", page + 1, count)))
        .child(
            Button::new(("pdf-next", id))
                .ghost()
                .xsmall()
                .icon(IconName::ChevronRight)
                .tooltip("Next page (→)")
                .disabled(page + 1 >= count)
                .on_click(cx.listener(|view, _, window, cx| {
                    view.pdf_step(1, window, cx);
                })),
        )
        .into_any_element()
}

/// The page body: the current bitmap centered in a scrollable surface, sized
/// in logical pixels so the window's scale factor does not double it. Before
/// the first render (or after a failed one) a muted placeholder stands in.
///
/// A zero-paint canvas sits over the body to report its laid-out width back
/// to the view (`PdfContent::pane_width`) — the fit-to-pane scale needs it,
/// and layout is the only place it exists. The width is quantised, compared
/// against what the view already holds, and only a change is deferred out
/// of layout and notified, so a resize cannot spin a render loop.
pub fn page_body(
    content: &PdfContent,
    scale_factor: f32,
    view: WeakEntity<EditorView>,
    muted_fg: gpui::Hsla,
    border: gpui::Hsla,
    text_size: f32,
) -> AnyElement {
    let view_id = view.entity_id();
    let known_width = content.pane_width;
    let probe = gpui::canvas(
        move |bounds, _window, cx| {
            // Floor to a step, never below one: a sliver of a pane would
            // otherwise report zero and pass as "known".
            let width = ((f32::from(bounds.size.width) / PANE_WIDTH_STEP_PX).floor()
                * PANE_WIDTH_STEP_PX)
                .max(PANE_WIDTH_STEP_PX);
            if known_width == Some(width) {
                return;
            }
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    if let Some(p) = this.pdf_content_mut()
                        && p.pane_width != Some(width)
                    {
                        p.pane_width = Some(width);
                        cx.notify();
                    }
                });
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let inner: AnyElement = match &content.bitmap {
        None => {
            let label = if content.failed {
                "This page could not be rendered"
            } else {
                "Rendering…"
            };
            gpui::div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_size(px(text_size))
                .text_color(muted_fg)
                .child(label)
                .into_any_element()
        }
        Some(bitmap) => {
            let size = bitmap.size(0);
            let scale = scale_factor.max(0.1);
            let (w, h) = (size.width.0 as f32 / scale, size.height.0 as f32 / scale);
            // The wrapper is sized explicitly: pane-wide while the page fits
            // (so `items_center` centres it), content-wide once it is wider
            // (zoomed in). A stretched, auto-width wrapper would report the
            // pane's width as the scroll content, leaving the overflow
            // clipped and unreachable in the horizontal direction.
            let content_w = w + 2.0 * PAGE_PADDING_PX;
            let wrapper_w = content.pane_width.map_or(content_w, |pane| content_w.max(pane));
            gpui::div()
                .id(("pdf-page-scroll", view_id))
                .size_full()
                .overflow_scroll()
                .track_scroll(&content.scroll)
                .child(
                    gpui::div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .w(px(wrapper_w))
                        .p(px(PAGE_PADDING_PX))
                        .child(
                            // Paper on a surface: a hairline edge and a soft
                            // shadow read as a page under both themes.
                            img(bitmap.clone())
                                .flex_none()
                                .w(px(w))
                                .h(px(h))
                                .border_1()
                                .border_color(border)
                                .shadow_md(),
                        ),
                )
                .into_any_element()
        }
    };
    gpui::div()
        .flex_1()
        .min_h_0()
        .relative()
        .child(inner)
        .child(probe)
        .into_any_element()
}

/// Smallest well-formed document hayro accepts: `pages` empty 200×100 pt
/// pages. Shared by this module's tests and the view tests in `editor_view`.
#[cfg(test)]
pub(crate) fn test_pdf(pages: usize) -> Vec<u8> {
    test_pdf_sized(pages, 200, 100)
}

/// `test_pdf` with an explicit page box in points — a pixel-sized box
/// (2480 × 3508) is what a scanner driver emits and what the area cap is for.
#[cfg(test)]
pub(crate) fn test_pdf_sized(pages: usize, w: u32, h: u32) -> Vec<u8> {
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
        objs.push(
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] >>").into_bytes(),
        );
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
    fn scale_clamp_caps_area_then_edge_and_floors_first() {
        // A4 at retina: untouched.
        assert_eq!(clamp_scale(2.0, 595.0, 842.0), 2.0);
        // Pixel-sized A4 scan at retina: 34.8 Mpx → shrunk to the 16 Mpx cap.
        let s = clamp_scale(2.0, 2480.0, 3508.0);
        let area = 2480.0 * 3508.0 * s * s;
        assert!((area - MAX_AREA_PX).abs() < MAX_AREA_PX * 0.001, "area {area}");
        // A ribbon 100 000 × 10 pt: area is fine at 1.0, the edge cap bites.
        let s = clamp_scale(1.0, 100_000.0, 10.0);
        assert!((100_000.0 * s - MAX_EDGE_PX).abs() < 1.0);
        // The floor never re-raises past a cap.
        assert!(clamp_scale(0.001, 2480.0, 3508.0) >= 0.05);
        assert!(clamp_scale(0.001, 1_000_000.0, 1_000_000.0) < 0.05);
    }

    #[test]
    fn oversized_page_renders_under_the_area_cap() {
        let doc = PdfDocument::parse(test_pdf_sized(1, 2480, 3508)).expect("valid pdf");
        assert_eq!(doc.page_size(0), Some((2480.0, 3508.0)));
        let img = doc.render_page(0, 2.0).expect("renders");
        let size = img.size(0);
        let area = size.width.0 as f32 * size.height.0 as f32;
        assert!(area <= MAX_AREA_PX, "area {area} exceeds the cap");
        assert!(area > MAX_AREA_PX * 0.98, "area {area} far under the cap");
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
