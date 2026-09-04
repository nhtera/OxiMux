//! PDF preview: a continuously scrolling document, a thumbnail rail, and a
//! page/zoom toolbar — the shape every browser and OS viewer has settled on.
//!
//! **Why a render worker.** hayro's `RenderCache<'a>` borrows the document and
//! is `Rc`-based, so it can neither be shared between threads nor stored
//! beside the `Arc<PdfDocument>` every view holds. Building a fresh one per
//! page throws away decoded fonts and images: measured on a 651-page
//! text-heavy book, 50 ms a page with a fresh cache against 27.7 ms sharing
//! one. So each open document gets a [`PdfRenderer`] — a couple of threads
//! that each own the document and one warm cache for as long as the pane is
//! open, fed by a priority queue and answering over a channel.
//!
//! **Why a list, not a page.** Rendering is fast enough only if it happens
//! before the reader arrives. A virtualized `uniform_list` over every page
//! makes "what is about to be visible" a fact the layout already knows, so
//! [`PageStore::request_window`] can queue exactly that and nothing else.
//! Scrolling within a page then costs nothing at all.
//!
//! The page is painted on an opaque white ground, so every pixel hayro hands
//! back has alpha 255 and premultiplied equals straight. The only conversion
//! before `RenderImage::new` is the RGBA→BGRA swap gpui expects.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};

use gpui::{
    AnyElement, App, Context, Entity, Global, InteractiveElement, IntoElement, ParentElement,
    RenderImage, ScrollStrategy, StatefulInteractiveElement as _, Styled, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, img, point, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_component::{
    ActiveTheme as _, Disableable, IconName, Selectable as _, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
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
/// scale. A real A4 page at retina is ~2 Mpx and never meets this cap.
const MAX_AREA_PX: f32 = 16_000_000.0;

/// Hard ceiling on either edge — the GPU texture limit on every platform
/// GPUI targets. Only a very elongated page can reach it under the area cap.
const MAX_EDGE_PX: f32 = 16_384.0;

/// Padding around the page column, in logical px.
pub const PAGE_PADDING_PX: f32 = 16.0;

/// Gap above each page, in logical px. Part of every list row's height, so
/// the list's own arithmetic places every page correctly.
pub const PAGE_GAP_PX: f32 = 12.0;

/// Vertical travel of one ↑/↓ press, in logical px.
pub const SCROLL_STEP_PX: f32 = 64.0;

/// Overlap kept when PageUp/PageDown scroll by a viewport, in logical px,
/// so the line at the edge stays in view as an anchor.
pub const SCROLL_PAGE_OVERLAP_PX: f32 = 24.0;

/// The size probe reports in steps of this many logical px. The fit scale
/// follows the pane size, so an unquantised report would turn a resize drag
/// into one full rasterization per pixel of travel (a rasterization that has
/// started cannot be cancelled); 32 px keeps a drag to a handful.
pub const PANE_STEP_PX: f32 = 32.0;

/// Pages queued either side of the visible range. Four is roughly a second
/// of fast scrolling at the measured page cost, which is what it takes for
/// the reader never to meet a blank sheet.
pub const PREFETCH_MARGIN: usize = 4;

/// Resident full-size pages, in bytes. A retina fit-width A4 is ~20–40 MB,
/// so this holds a dozen or so — the visible pages plus both margins, which
/// is every page the reader can reach before the workers catch up. Pages
/// furthest from the viewport are evicted first.
const PAGE_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// Width of a thumbnail in the rail, in logical px.
pub const THUMB_WIDTH_PX: f32 = 108.0;

/// Width of the whole thumbnail rail, in logical px.
pub const THUMB_RAIL_WIDTH_PX: f32 = 152.0;

/// The selection ring around a thumbnail, and the gap it leaves between the
/// ring and the page, in logical px. Both are always laid out — the ring is
/// merely transparent when the page is not current — so selecting a page
/// cannot nudge the rail's layout.
const THUMB_RING_PX: f32 = 2.0;
const THUMB_INSET_PX: f32 = 3.0;

/// Height of the page-number pill under a thumbnail, in logical px.
const THUMB_LABEL_H_PX: f32 = 16.0;

/// Zoom rungs the −/+ buttons and ⌘+/⌘− walk, as scale factors (logical px
/// per PDF point). A browser-style ladder: coarse at the extremes, fine
/// around 100 % where reading actually happens.
const ZOOM_LADDER: [f32; 16] = [
    0.25, 0.33, 0.50, 0.67, 0.75, 0.80, 0.90, 1.00, 1.10, 1.25, 1.50, 1.75, 2.00, 2.50, 3.00, 4.00,
];

/// Hard bounds on the effective scale. Wider than the ladder so a fit mode
/// can still shrink a poster or enlarge a stamp beyond the −/+ range.
const SCALE_MIN: f32 = 0.05;
const SCALE_MAX: f32 = 8.0;

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

// ---------------------------------------------------------------- document

/// A parsed document, shared between the view and its render workers.
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
    /// `None` when out of range.
    pub fn page_size(&self, index: usize) -> Option<(f32, f32)> {
        Some(self.pdf.pages().get(index)?.render_dimensions())
    }

    /// The largest page box in the document, in points. Every list row is
    /// this tall, so a document whose pages differ in size still lays out
    /// correctly — the smaller pages sit centred in their row. Cheap:
    /// reading all 651 sizes of a real book measured 8.6 µs.
    pub fn max_page_size(&self) -> (f32, f32) {
        let mut max = (1.0f32, 1.0f32);
        for index in 0..self.page_count() {
            if let Some((w, h)) = self.page_size(index) {
                max = (max.0.max(w), max.1.max(h));
            }
        }
        max
    }

    /// Rasterize page `index` (0-based) at `scale` device pixels per PDF
    /// point, as a BGRA bitmap ready for gpui, through `cache`. `None` when
    /// the index is out of range or the result has no area.
    fn render_page_cached<'a>(
        &'a self,
        index: usize,
        scale: f32,
        cache: &RenderCache<'a>,
    ) -> Option<Arc<RenderImage>> {
        let page = self.pdf.pages().get(index)?;
        let (w, h) = page.render_dimensions();
        let scale = clamp_scale(scale, w, h);
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
            ..Default::default()
        };
        let pixmap = hayro::render(page, cache, &InterpreterSettings::default(), &settings);
        let (pw, ph) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
        if pw == 0 || ph == 0 {
            return None;
        }
        let buffer = image::RgbaImage::from_raw(pw, ph, to_bgra(pixmap.data()))?;
        Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
    }

    /// One page, on this thread, with a throwaway cache. For callers outside
    /// the worker pool — tests, and any one-shot use.
    pub fn render_page(&self, index: usize, scale: f32) -> Option<Arc<RenderImage>> {
        self.render_page_cached(index, scale, &RenderCache::new())
    }

    /// The worker loop: take jobs until the queue closes, rendering through
    /// one cache that lives as long as this call. `&self` is what ties that
    /// cache's lifetime to the document, which is the whole reason this is a
    /// method and not a free function.
    fn serve(&self, queue: &JobQueue, results: &ResultSink) {
        let cache = RenderCache::new();
        while let Some(job) = queue.take() {
            let done = match job {
                Job::Page(key) => {
                    Done::Page(key, self.render_page_cached(key.page, key.scale(), &cache))
                }
                Job::Thumb { page, scale } => {
                    Done::Thumb(page, self.render_page_cached(page, scale, &cache))
                }
            };
            if results.send(done).is_err() {
                return;
            }
        }
    }
}

/// hayro hands back premultiplied RGBA; gpui wants BGRA. Writing into a
/// pre-sized buffer (rather than pushing four bytes a pixel) is what lets
/// this vectorize — it was 18.6 ms of a 48 ms page before.
fn to_bgra(src: &[hayro::vello_cpu::color::PremulRgba8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len() * 4];
    for (dst, p) in out.chunks_exact_mut(4).zip(src) {
        dst[0] = p.b;
        dst[1] = p.g;
        dst[2] = p.r;
        dst[3] = p.a;
    }
    out
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

/// Bytes a decoded page occupies: BGRA, four bytes a pixel.
fn image_bytes(img: &RenderImage) -> usize {
    let size = img.size(0);
    (size.width.0.max(0) as usize) * (size.height.0.max(0) as usize) * 4
}

// ---------------------------------------------------------------- renderer

/// A page identified by its index and the scale it was rendered at. The
/// scale is quantised so a float that round-trips through the window's scale
/// factor does not look like a different request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub page: usize,
    /// Device pixels per PDF point, ×1000.
    pub scale_milli: u32,
}

impl PageKey {
    pub fn new(page: usize, scale: f32) -> Self {
        Self {
            page,
            scale_milli: (scale * 1000.0).round().max(0.0) as u32,
        }
    }

    fn scale(self) -> f32 {
        self.scale_milli as f32 / 1000.0
    }
}

enum Job {
    Page(PageKey),
    Thumb { page: usize, scale: f32 },
}

enum Done {
    Page(PageKey, Option<Arc<RenderImage>>),
    Thumb(usize, Option<Arc<RenderImage>>),
}

/// Jobs waiting for a worker. Served **last-in first-out**: the most recent
/// request is the one closest to where the reader now is, and a queue that
/// served oldest-first would spend a fast scroll rendering pages nobody is
/// looking at any more.
struct JobQueue {
    inner: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    jobs: Vec<Job>,
    closed: bool,
}

impl JobQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(QueueState {
                jobs: Vec::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Push jobs lowest-priority first, so the highest-priority one is taken
    /// next. A poisoned lock means a worker panicked mid-render; the pane
    /// then stops rendering rather than propagating the panic into the UI.
    fn push(&self, jobs: impl IntoIterator<Item = Job>) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.jobs.extend(jobs);
        self.ready.notify_all();
    }

    /// Pop a job if one is waiting, never blocking. The inline drain the
    /// tests use needs this: [`Self::take`] parks on the condvar, which with
    /// no workers and an open queue would simply hang.
    #[cfg(test)]
    fn try_take(&self) -> Option<Job> {
        self.inner.lock().ok()?.jobs.pop()
    }

    fn take(&self) -> Option<Job> {
        let mut state = self.inner.lock().ok()?;
        loop {
            if let Some(job) = state.jobs.pop() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    /// Drop everything still queued — used when the zoom changes, since
    /// every pending job is then for a scale nobody wants.
    fn clear(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.jobs.clear();
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.jobs.clear();
            state.closed = true;
        }
        self.ready.notify_all();
    }
}

type ResultSink = tokio::sync::mpsc::UnboundedSender<Done>;

/// The render pool for one open document. Dropping it closes the queue, so
/// each worker finishes the page in its hands and exits.
pub struct PdfRenderer {
    queue: Arc<JobQueue>,
}

impl PdfRenderer {
    /// Start rendering `doc` on `workers` threads. Each worker owns its own
    /// warm cache; more than a couple buys little, because the cost that
    /// matters is the one a cold cache pays.
    ///
    /// `workers: 0` starts none, and the queue is then drained by whoever
    /// calls [`PdfContent::render_pending_inline`] — which is how the view
    /// tests run, since gpui's test scheduler treats any activity on a
    /// thread it does not own as non-determinism and panics.
    fn start(
        doc: Arc<PdfDocument>,
        workers: usize,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Done>) {
        let queue = Arc::new(JobQueue::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for n in 0..workers {
            let doc = doc.clone();
            let queue = queue.clone();
            let tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("oximux-pdf-render-{n}"))
                .spawn(move || doc.serve(&queue, &tx));
            if let Err(err) = spawned {
                tracing::warn!(?err, "pdf: could not start a render worker");
            }
        }
        (Self { queue }, rx)
    }
}

impl Drop for PdfRenderer {
    fn drop(&mut self) {
        self.queue.close();
    }
}

// -------------------------------------------------------------------- zoom

/// How the page is sized in the pane.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum PdfZoom {
    /// Page width fills the pane (less padding). The default — it is what a
    /// reader wants for a text document.
    #[default]
    FitWidth,
    /// The whole page fits, both axes.
    FitPage,
    /// An explicit scale in logical px per PDF point. `1.0` is 100 %.
    Scale(f32),
}

impl PdfZoom {
    /// Logical px per PDF point for `page` (points) shown in `pane`
    /// (logical px). Fit modes need the pane; `Scale` ignores it.
    pub fn effective_scale(self, page: (f32, f32), pane: (f32, f32)) -> f32 {
        let (page_w, page_h) = (page.0.max(1.0), page.1.max(1.0));
        let avail_w = (pane.0 - 2.0 * PAGE_PADDING_PX).max(64.0);
        // Fit page must leave room for the gap above the page, or "the whole
        // page" would still be one gap taller than the viewport.
        let avail_h = (pane.1 - PAGE_GAP_PX).max(64.0);
        let raw = match self {
            Self::FitWidth => avail_w / page_w,
            Self::FitPage => (avail_w / page_w).min(avail_h / page_h),
            Self::Scale(s) => s,
        };
        raw.clamp(SCALE_MIN, SCALE_MAX)
    }
}

/// The ladder rung `steps` away from `current`, in the direction of `steps`.
/// Walking from the *effective* scale (not from a stored rung) is what makes
/// ⌘+ from a fit mode land on the next rung above what is on screen.
pub fn zoom_stepped(current: f32, steps: i32) -> f32 {
    let mut value = current.clamp(SCALE_MIN, SCALE_MAX);
    let up = steps > 0;
    for _ in 0..steps.unsigned_abs() {
        let next = if up {
            ZOOM_LADDER.iter().copied().find(|rung| *rung > value + 1e-4)
        } else {
            ZOOM_LADDER
                .iter()
                .rev()
                .copied()
                .find(|rung| *rung < value - 1e-4)
        };
        match next {
            Some(rung) => value = rung,
            None => break,
        }
    }
    value
}

// -------------------------------------------------------------- page store

/// Rendered pages and thumbnails, plus what has been asked for. Shared by
/// `Rc` between the view's state and the lists' render closures: those
/// closures are `'static` and cannot borrow the view, yet they are the only
/// place that knows which pages are about to be visible.
pub struct PageStore {
    pages: RefCell<HashMap<PageKey, Arc<RenderImage>>>,
    thumbs: RefCell<HashMap<usize, Arc<RenderImage>>>,
    pending: RefCell<HashSet<PageKey>>,
    thumbs_pending: RefCell<HashSet<usize>>,
    /// Pages the renderer handed back empty. Not retried — the sheet says so
    /// instead of blinking forever.
    failed: RefCell<HashSet<PageKey>>,
    /// The page the reader is on. Derived from the scroll offset by
    /// [`PdfContent::page`] and cached here; a pending jump holds it until
    /// the list has actually moved.
    ///
    /// It is deliberately *not* written from a list's render closure:
    /// `uniform_list` invokes that closure for its measurement pass as well,
    /// with a range of a single item at index 0, which is indistinguishable
    /// from a reader sitting on page 1.
    top_page: Cell<usize>,
    renderer: PdfRenderer,
}

impl PageStore {
    fn new(renderer: PdfRenderer) -> Self {
        Self {
            pages: RefCell::new(HashMap::new()),
            thumbs: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashSet::new()),
            thumbs_pending: RefCell::new(HashSet::new()),
            failed: RefCell::new(HashSet::new()),
            top_page: Cell::new(0),
            renderer,
        }
    }

    pub fn page(&self, key: PageKey) -> Option<Arc<RenderImage>> {
        self.pages.borrow().get(&key).cloned()
    }

    pub fn thumb(&self, page: usize) -> Option<Arc<RenderImage>> {
        self.thumbs.borrow().get(&page).cloned()
    }

    pub fn failed(&self, key: PageKey) -> bool {
        self.failed.borrow().contains(&key)
    }

    pub fn top_page(&self) -> usize {
        self.top_page.get()
    }

    /// How many full-size pages are resident. Read by tests.
    pub fn resident_pages(&self) -> usize {
        self.pages.borrow().len()
    }

    /// Queue every page in `visible`, plus [`PREFETCH_MARGIN`] either side,
    /// that is neither rendered, already asked for, nor known bad. Pushed
    /// furthest-first so the queue's LIFO order serves the visible pages
    /// first. Idempotent — the list calls this on every layout.
    pub fn request_window(&self, visible: std::ops::Range<usize>, scale: f32, page_count: usize) {
        if page_count == 0 {
            return;
        }
        let lo = visible.start.saturating_sub(PREFETCH_MARGIN);
        let hi = (visible.end + PREFETCH_MARGIN).min(page_count);
        let centre = visible.start;
        let mut wanted: Vec<usize> = {
            let pages = self.pages.borrow();
            let pending = self.pending.borrow();
            let failed = self.failed.borrow();
            (lo..hi)
                .filter(|page| {
                    let key = PageKey::new(*page, scale);
                    !pages.contains_key(&key) && !pending.contains(&key) && !failed.contains(&key)
                })
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        // Furthest from the top of the viewport first; the worker pops last.
        wanted.sort_by_key(|page| std::cmp::Reverse(page.abs_diff(centre)));
        let mut pending = self.pending.borrow_mut();
        let jobs: Vec<Job> = wanted
            .into_iter()
            .map(|page| {
                let key = PageKey::new(page, scale);
                pending.insert(key);
                Job::Page(key)
            })
            .collect();
        drop(pending);
        self.renderer.queue.push(jobs);
    }

    /// Queue thumbnails for the rail's visible range. A thumbnail costs
    /// ~3 ms and is never evicted — a 651-page book's whole rail is ~50 MB.
    pub fn request_thumbs(&self, visible: std::ops::Range<usize>, scale: f32, page_count: usize) {
        let hi = visible.end.min(page_count);
        let wanted: Vec<usize> = {
            let thumbs = self.thumbs.borrow();
            let pending = self.thumbs_pending.borrow();
            (visible.start..hi)
                .filter(|page| !thumbs.contains_key(page) && !pending.contains(page))
                .collect()
        };
        let mut pending = self.thumbs_pending.borrow_mut();
        let jobs: Vec<Job> = wanted
            .into_iter()
            .map(|page| {
                pending.insert(page);
                Job::Thumb { page, scale }
            })
            .collect();
        drop(pending);
        if !jobs.is_empty() {
            self.renderer.queue.push(jobs);
        }
    }

    /// Forget every page rendered at a scale other than `keep`, and drop the
    /// jobs still queued for those scales. Returns the bitmaps so the caller
    /// can release their textures — gpui frees a `RenderImage` from the
    /// sprite atlas only on request.
    fn drop_other_scales(&self, keep: u32) -> Vec<Arc<RenderImage>> {
        let mut pages = self.pages.borrow_mut();
        let stale: Vec<PageKey> = pages
            .keys()
            .filter(|k| k.scale_milli != keep)
            .copied()
            .collect();
        if stale.is_empty() {
            return Vec::new();
        }
        self.renderer.queue.clear();
        self.pending.borrow_mut().retain(|k| k.scale_milli == keep);
        self.failed.borrow_mut().retain(|k| k.scale_milli == keep);
        stale.into_iter().filter_map(|k| pages.remove(&k)).collect()
    }

    /// Evict pages furthest from `centre` until the resident set is back
    /// inside [`PAGE_BUDGET_BYTES`]. Returns them for release.
    fn evict(&self, centre: usize) -> Vec<Arc<RenderImage>> {
        let mut pages = self.pages.borrow_mut();
        let mut total: usize = pages.values().map(|img| image_bytes(img)).sum();
        if total <= PAGE_BUDGET_BYTES {
            return Vec::new();
        }
        let mut order: Vec<PageKey> = pages.keys().copied().collect();
        // Furthest first; the visible pages are the last thing to go.
        order.sort_by_key(|k| std::cmp::Reverse(k.page.abs_diff(centre)));
        let mut dropped = Vec::new();
        for key in order {
            if total <= PAGE_BUDGET_BYTES {
                break;
            }
            if let Some(img) = pages.remove(&key) {
                total = total.saturating_sub(image_bytes(&img));
                dropped.push(img);
            }
        }
        dropped
    }

    /// Everything resident, for release when the pane closes.
    fn take_all(&self) -> Vec<Arc<RenderImage>> {
        let mut out: Vec<Arc<RenderImage>> =
            self.pages.borrow_mut().drain().map(|(_, v)| v).collect();
        out.extend(self.thumbs.borrow_mut().drain().map(|(_, v)| v));
        out
    }
}

// ------------------------------------------------------------------- state

/// The open go-to-page editor that replaces the `N / M` counter while the
/// user is typing. The subscription is held here so it dies with the editor.
pub struct GotoPage {
    pub(crate) input: Entity<InputState>,
    pub(crate) _enter_sub: Subscription,
}

/// State carried by the `EditorContent::Pdf` variant.
pub struct PdfContent {
    pub(crate) doc: Arc<PdfDocument>,
    pub(crate) page_count: usize,
    /// The largest page box in the document, in points. Every row is sized
    /// from this so the list's arithmetic is exact.
    pub(crate) page_size: (f32, f32),
    /// How the page is sized in the pane. Per pane — it does not ride the
    /// editor-global font zoom, so ⌘+ on a PDF leaves every text tab alone.
    pub(crate) zoom: PdfZoom,
    /// Size of the page column in logical px, each axis floored to a
    /// `PANE_STEP_PX` step, reported by the probe after layout. `None` until
    /// the first frame has laid out, and nothing is requested until then.
    pub(crate) pane_size: Option<(f32, f32)>,
    pub(crate) list: UniformListScrollHandle,
    pub(crate) thumb_list: UniformListScrollHandle,
    pub(crate) store: Rc<PageStore>,
    /// `true` while the thumbnail rail is showing.
    pub(crate) show_thumbs: bool,
    pub(crate) goto: Option<GotoPage>,
    /// Page the rail was last pointed at, so it is nudged only on a change.
    rail_synced: Cell<usize>,
    /// The scale (×1000) the column's scroll offset was last anchored at.
    /// `0` until the first frame that has a scale.
    anchored_scale: Cell<u32>,
    /// Page last written to the page memory, likewise.
    pub(crate) remembered: Cell<usize>,
    /// Receives finished pages and folds them into the store.
    pub(crate) _pump: Task<()>,
}

impl PdfContent {
    /// A freshly opened document, showing `page` (clamped into range — a
    /// remembered page survives the file shrinking).
    pub fn new(
        doc: Arc<PdfDocument>,
        page: usize,
        window: &mut Window,
        cx: &mut Context<EditorView>,
    ) -> Self {
        let page_count = doc.page_count();
        let page_size = doc.max_page_size();
        // Two workers: enough to keep a fast scroll fed, few enough that
        // each of them keeps a hot cache. None under `cargo test` — see
        // `PdfRenderer::start`.
        let workers = if cfg!(test) { 0 } else { 2 };
        let (renderer, mut results) = PdfRenderer::start(doc.clone(), workers);
        let store = Rc::new(PageStore::new(renderer));
        let list = UniformListScrollHandle::new();
        let start = page.min(page_count.saturating_sub(1));
        store.top_page.set(start);
        list.scroll_to_item(start, ScrollStrategy::Top);

        let pump = cx.spawn_in(window, {
            let store = store.clone();
            async move |weak, cx| {
                while let Some(done) = results.recv().await {
                    let applied = weak.update_in(cx, |_view, window, cx| {
                        let centre = store.top_page.get();
                        let mut release = Vec::new();
                        match done {
                            Done::Page(key, image) => {
                                store.pending.borrow_mut().remove(&key);
                                match image {
                                    Some(img) => {
                                        store.pages.borrow_mut().insert(key, img);
                                        release = store.evict(centre);
                                    }
                                    None => {
                                        store.failed.borrow_mut().insert(key);
                                    }
                                }
                            }
                            Done::Thumb(page, image) => {
                                store.thumbs_pending.borrow_mut().remove(&page);
                                if let Some(img) = image {
                                    store.thumbs.borrow_mut().insert(page, img);
                                }
                            }
                        }
                        for img in release {
                            cx.drop_image(img, Some(window));
                        }
                        cx.notify();
                    });
                    if applied.is_err() {
                        return;
                    }
                }
            }
        });

        Self {
            doc,
            page_count,
            page_size,
            zoom: PdfZoom::default(),
            pane_size: None,
            list,
            thumb_list: UniformListScrollHandle::new(),
            store,
            show_thumbs: true,
            goto: None,
            rail_synced: Cell::new(usize::MAX),
            anchored_scale: Cell::new(0),
            remembered: Cell::new(start),
            _pump: pump,
        }
    }

    /// Logical px per PDF point at the current zoom. `None` until the pane
    /// has been measured.
    pub(crate) fn effective_scale(&self) -> Option<f32> {
        let pane = self.pane_size?;
        Some(self.zoom.effective_scale(self.page_size, pane))
    }

    /// Height of one list row in logical px at the current zoom, or `None`
    /// before the pane has been measured.
    fn row_height(&self) -> Option<f32> {
        Some(self.page_size.1 * self.effective_scale()? + PAGE_GAP_PX)
    }

    /// The page the reader is on, derived from the column's scroll offset.
    /// A jump that has not been applied yet wins, so a caller reading this
    /// immediately after [`Self::scroll_to_page`] sees where it asked to go.
    pub(crate) fn page(&self) -> usize {
        let last = self.page_count.saturating_sub(1);
        let (Some(scale), Some(row_h)) = (
            self.effective_scale(),
            self.row_height().filter(|h| *h > 1.0),
        ) else {
            return self.store.top_page().min(last);
        };
        // Two windows where the offset does not yet mean what it will:
        // a jump that has not been applied, and a scale change that
        // `keep_place_across_scale_change` has not re-anchored yet. Reading
        // through either would cache a page nobody asked for — and since the
        // re-anchor reads that cache back, the error would stick.
        let state = self.list.0.borrow();
        if state.deferred_scroll_to_item.is_some()
            || self.anchored_scale.get() != PageKey::new(0, scale).scale_milli
        {
            return self.store.top_page().min(last);
        }
        let offset = -f32::from(state.base_handle.offset().y);
        let page = ((offset / row_h).floor().max(0.0) as usize).min(last);
        self.store.top_page.set(page);
        page
    }

    /// Keep the reader's place when the rows change height.
    ///
    /// The list stores a scroll offset in **pixels**, and every row's height
    /// is the page height times the effective scale. So any scale change —
    /// a zoom, but also a pane resize, which is what a session restore does
    /// while the panes settle — leaves the offset pointing at a different
    /// page. Restoring at page 3 and finding page 14 is this, and nothing
    /// else. Re-anchor on the page we were on *before* the rows moved.
    ///
    /// Must run before [`Self::page`] in a frame: `page` recomputes from the
    /// offset and would otherwise cache the drifted value first.
    pub(crate) fn keep_place_across_scale_change(&self) {
        let Some(scale) = self.effective_scale() else {
            return;
        };
        let milli = PageKey::new(0, scale).scale_milli;
        if self.anchored_scale.replace(milli) == milli {
            return;
        }
        // Also fires on the first frame that has a scale, which is what puts
        // a restored tab on its remembered page rather than wherever the
        // constructor's scroll landed at whatever height was measured then.
        self.scroll_to_page(self.store.top_page());
    }

    /// Centre the rail on `page`. Called from `render`, where the deferred
    /// scroll is consumed by the very next layout — no extra frame, and so
    /// no repaint loop.
    ///
    /// **Strict** on purpose. The guard below records the page the rail was
    /// *asked* to show; a non-strict scroll declines to move whenever the
    /// row is already visible, so the guard would then remember an intent
    /// that never happened and never ask again — the rail creeps a few rows
    /// behind the document and stays there.
    pub(crate) fn sync_rail(&self, page: usize) {
        if self.rail_synced.replace(page) != page {
            self.thumb_list
                .scroll_to_item_strict(page, ScrollStrategy::Center);
        }
    }

    /// Scroll so `page` is at the top of the viewport, clamped into range.
    /// Returns the page actually landed on.
    pub(crate) fn scroll_to_page(&self, page: usize) -> usize {
        let page = page.min(self.page_count.saturating_sub(1));
        self.store.top_page.set(page);
        self.list.scroll_to_item_strict(page, ScrollStrategy::Top);
        self.thumb_list.scroll_to_item(page, ScrollStrategy::Center);
        page
    }

    /// Scroll the page column by `dy` logical px (positive = down), clamped
    /// to the document.
    pub(crate) fn scroll_by(&self, dy: f32) {
        let state = self.list.0.borrow();
        let handle = &state.base_handle;
        let offset = handle.offset();
        let max_down = f32::from(handle.max_offset().y).max(0.0);
        // gpui offsets grow negative as the content scrolls up.
        let target = (f32::from(offset.y) - dy).clamp(-max_down, 0.0);
        handle.set_offset(point(offset.x, px(target)));
    }

    /// One viewport of travel for PageUp/PageDown, less the overlap.
    pub(crate) fn viewport_step(&self) -> f32 {
        let height = f32::from(self.list.0.borrow().base_handle.bounds().size.height);
        (height - SCROLL_PAGE_OVERLAP_PX).max(SCROLL_STEP_PX)
    }

    /// Forget every page rendered at another scale, for release by the
    /// caller. Called after a zoom change.
    pub(crate) fn drop_other_scales(&self) -> Vec<Arc<RenderImage>> {
        match self.effective_scale() {
            Some(scale) => self
                .store
                .drop_other_scales(PageKey::new(0, scale).scale_milli),
            None => Vec::new(),
        }
    }

    /// Everything resident, for release when the pane closes.
    pub(crate) fn take_all_images(&self) -> Vec<Arc<RenderImage>> {
        self.store.take_all()
    }

    /// Render everything the lists have queued, here and now. Only used by
    /// the view tests, which run with no worker threads.
    #[cfg(test)]
    pub(crate) fn render_pending_inline(&self) {
        let cache = RenderCache::new();
        while let Some(job) = self.store.renderer.queue.try_take() {
            match job {
                Job::Page(key) => {
                    self.store.pending.borrow_mut().remove(&key);
                    match self.doc.render_page_cached(key.page, key.scale(), &cache) {
                        Some(img) => {
                            self.store.pages.borrow_mut().insert(key, img);
                        }
                        None => {
                            self.store.failed.borrow_mut().insert(key);
                        }
                    }
                }
                Job::Thumb { page, scale } => {
                    self.store.thumbs_pending.borrow_mut().remove(&page);
                    if let Some(img) = self.doc.render_page_cached(page, scale, &cache) {
                        self.store.thumbs.borrow_mut().insert(page, img);
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ memory

/// Last page each PDF was left on, keyed by absolute path. A GPUI
/// [`Global`], so reopening a file in a new tab — or restoring one after a
/// relaunch, once the tab record has seeded it — lands where the user was.
#[derive(Default)]
pub struct PdfPageMemory(HashMap<PathBuf, usize>);

impl Global for PdfPageMemory {}

/// Record `page` (0-based) as where `path` was left.
pub fn remember_pdf_page(cx: &mut App, path: &Path, page: usize) {
    cx.default_global::<PdfPageMemory>()
        .0
        .insert(path.to_path_buf(), page);
}

/// The page `path` was left on, if this session has seen it.
pub fn remembered_pdf_page(cx: &App, path: &Path) -> Option<usize> {
    cx.try_global::<PdfPageMemory>()
        .and_then(|m| m.0.get(path).copied())
}

// -------------------------------------------------------------------- view

/// The page toolbar for the breadcrumb row: the thumbnail-rail toggle,
/// `‹ N / M ›`, and zoom −/+ with a percentage readout plus the Fit width /
/// Fit page / 100 % presets. Clicking the counter opens a go-to editor.
pub fn page_toolbar(content: &PdfContent, cx: &Context<EditorView>) -> AnyElement {
    let id = cx.entity_id();
    let (page, count) = (content.page(), content.page_count);
    let percent = content
        .effective_scale()
        .map(|s| (s * 100.0).round() as i32)
        .unwrap_or(100);
    let zoom = content.zoom;
    let border = cx.theme().border;

    let counter: AnyElement = match &content.goto {
        Some(goto) => Input::new(&goto.input)
            .xsmall()
            .w(px(56.0))
            .into_any_element(),
        None => Button::new(("pdf-goto", id))
            .ghost()
            .xsmall()
            .label(format!("{} / {}", page + 1, count))
            .tooltip("Go to page")
            .on_click(cx.listener(|view, _, window, cx| {
                view.pdf_open_goto(window, cx);
            }))
            .into_any_element(),
    };

    let preset = |label: &'static str, target: PdfZoom, cx: &Context<EditorView>| {
        Button::new((
            match target {
                PdfZoom::FitWidth => "pdf-fit-width",
                PdfZoom::FitPage => "pdf-fit-page",
                PdfZoom::Scale(_) => "pdf-actual-size",
            },
            id,
        ))
        .ghost()
        .xsmall()
        .label(label)
        .selected(zoom == target)
        .on_click(cx.listener(move |view, _, _window, cx| {
            view.pdf_set_zoom(target, cx);
        }))
    };

    let divider = || gpui::div().w(px(1.0)).h(px(14.0)).mx(px(4.0)).bg(border);

    gpui::div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .child(
            Button::new(("pdf-thumbs", id))
                .ghost()
                .xsmall()
                .icon(IconName::PanelLeft)
                .tooltip("Page thumbnails")
                .selected(content.show_thumbs)
                .on_click(cx.listener(|view, _, _window, cx| {
                    view.pdf_toggle_thumbs(cx);
                })),
        )
        .child(divider())
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
        .child(counter)
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
        .child(divider())
        .child(
            Button::new(("pdf-zoom-out", id))
                .ghost()
                .xsmall()
                .icon(IconName::Minus)
                .tooltip("Zoom out (⌘−)")
                .on_click(cx.listener(|view, _, _window, cx| {
                    view.pdf_zoom_by(-1, cx);
                })),
        )
        .child(
            gpui::div()
                .px(px(2.0))
                .min_w(px(34.0))
                .child(format!("{percent} %")),
        )
        .child(
            Button::new(("pdf-zoom-in", id))
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .tooltip("Zoom in (⌘+)")
                .on_click(cx.listener(|view, _, _window, cx| {
                    view.pdf_zoom_by(1, cx);
                })),
        )
        .child(preset("Fit width", PdfZoom::FitWidth, cx))
        .child(preset("Fit page", PdfZoom::FitPage, cx))
        .child(preset("100 %", PdfZoom::Scale(1.0), cx))
        .into_any_element()
}

/// Colours the document body needs, snapshotted by the caller so this
/// module never holds a theme borrow across a `cx.listener`.
#[derive(Clone, Copy)]
pub struct PdfColors {
    pub muted_fg: gpui::Hsla,
    pub border: gpui::Hsla,
    pub surface: gpui::Hsla,
    /// The theme's selection colour: the ring and the label of the page the
    /// reader is on.
    pub accent: gpui::Hsla,
}

/// The document: an optional thumbnail rail beside a continuously scrolling
/// column of every page.
///
/// A zero-paint canvas over the column reports its laid-out size back to the
/// view (`PdfContent::pane_size`) — the fit-to-pane scale needs it, and
/// layout is the only place it exists. The size is quantised, compared
/// against what the view already holds, and only a change is deferred out of
/// layout and notified, so a resize cannot spin a render loop.
pub fn document_body(
    content: &PdfContent,
    scale_factor: f32,
    view: WeakEntity<EditorView>,
    colors: PdfColors,
    text_size: f32,
) -> AnyElement {
    let view_id = view.entity_id();
    let known_size = content.pane_size;
    let probe = gpui::canvas(
        {
            let view = view.clone();
            move |bounds, _window, cx| {
                // Floor to a step, never below one: a sliver of a pane would
                // otherwise report zero and pass as "known".
                let step = |v: gpui::Pixels| {
                    ((f32::from(v) / PANE_STEP_PX).floor() * PANE_STEP_PX).max(PANE_STEP_PX)
                };
                let size = (step(bounds.size.width), step(bounds.size.height));
                if known_size == Some(size) {
                    return;
                }
                let view = view.clone();
                cx.defer(move |cx| {
                    let _ = view.update(cx, |this, cx| {
                        if let Some(p) = this.pdf_content_mut()
                            && p.pane_size != Some(size)
                        {
                            p.pane_size = Some(size);
                            cx.notify();
                        }
                    });
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    let column = match content.effective_scale() {
        // Before the first layout there is no scale, so no page can be
        // sized. One frame of empty pane, then the probe reports.
        None => gpui::div().flex_1().into_any_element(),
        Some(scale) => page_column(content, scale, scale_factor, colors, text_size),
    };

    gpui::div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h_0()
        .when(content.show_thumbs, |row| {
            row.child(thumbnail_rail(content, view.clone(), colors, text_size))
        })
        .child(
            gpui::div()
                .id(("pdf-column", view_id))
                .flex_1()
                .min_w_0()
                .min_h_0()
                .relative()
                .bg(colors.surface)
                .child(column)
                .child(probe),
        )
        .into_any_element()
}

/// The scrolling column of pages. Every row is one page, sized from the
/// document's largest page box so the list's own arithmetic places them all.
fn page_column(
    content: &PdfContent,
    scale: f32,
    scale_factor: f32,
    colors: PdfColors,
    text_size: f32,
) -> AnyElement {
    let (page_w, page_h) = content.page_size;
    let (w, h) = (page_w * scale, page_h * scale);
    let row_h = h + PAGE_GAP_PX;
    // A page zoomed past the pane must still be reachable: the row is as
    // wide as the wider of the two, and the column scrolls horizontally.
    let row_w = content
        .pane_size
        .map_or(w, |(pane_w, _)| w.max(pane_w - 2.0 * PAGE_PADDING_PX));
    let page_count = content.page_count;
    let store = content.store.clone();
    let doc = content.doc.clone();
    let device_scale = scale * scale_factor;

    let list = uniform_list("pdf-pages", page_count, move |visible, _window, _cx| {
        // The list has just told us exactly which pages are about to be
        // painted — the one place that fact exists. Queue them and their
        // margins; already-rendered, in-flight and known-bad pages are
        // skipped, so calling this every layout is free.
        store.request_window(visible.clone(), device_scale, page_count);
        visible
            .map(|index| {
                let key = PageKey::new(index, device_scale);
                let bitmap = store.page(key);
                let failed = bitmap.is_none() && store.failed(key);
                // Rows are uniformly tall (the list's arithmetic needs that),
                // but each sheet is its own page's size, so a document that
                // mixes portrait and landscape still draws every page right.
                let (sheet_w, sheet_h) = doc
                    .page_size(index)
                    .map_or((w, h), |(pw, ph)| (pw * scale, ph * scale));
                let sheet = gpui::div()
                    .w(px(sheet_w))
                    .h(px(sheet_h))
                    .flex_none()
                    .bg(gpui::white())
                    .border_1()
                    .border_color(colors.border)
                    .shadow_md()
                    // A page still rendering is a blank sheet the right size
                    // — the reader is scrolling toward it anyway, and it
                    // never moves under them when the bitmap lands.
                    .when_some(bitmap, |sheet, bitmap| {
                        sheet.child(img(bitmap).w(px(sheet_w)).h(px(sheet_h)))
                    })
                    .when(failed, |sheet| {
                        sheet
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(text_size))
                            .text_color(colors.muted_fg)
                            .child("This page could not be rendered")
                    });
                gpui::div()
                    .h(px(row_h))
                    .w(px(row_w))
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(PAGE_GAP_PX))
                    .child(sheet)
            })
            .collect()
    })
    .track_scroll(&content.list)
    // A page zoomed past the pane has to be reachable sideways. This is also
    // what turns on the list's horizontal scrolling — `FitList`, the default,
    // would clip the overflow with no way to reach it.
    .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
    .size_full();

    gpui::div()
        .size_full()
        .px(px(PAGE_PADDING_PX))
        .child(list)
        .into_any_element()
}

/// The thumbnail rail: a narrow list of every page, the current one outlined,
/// click to jump.
/// `view` is needed only to route a thumbnail click back through the view;
/// the rail's own scroll position is driven from `render`, not from here.
fn thumbnail_rail(
    content: &PdfContent,
    view: WeakEntity<EditorView>,
    colors: PdfColors,
    text_size: f32,
) -> AnyElement {
    let page_count = content.page_count;
    let (page_w, page_h) = content.page_size;
    let thumb_scale = THUMB_WIDTH_PX / page_w.max(1.0);
    let thumb_h = page_h * thumb_scale;
    // The row holds the ring (2 px + a 3 px inset, both sides), the gap and
    // the page label. Getting this exactly right matters: `uniform_list`
    // places every row by arithmetic, so a row taller than its contents
    // leaves a drifting gap and a shorter one clips the label.
    let row_h = thumb_h + THUMB_RING_PX * 2.0 + THUMB_INSET_PX * 2.0 + THUMB_LABEL_H_PX + 10.0;
    let store = content.store.clone();
    let current = content.page();

    let list = uniform_list("pdf-thumbs", page_count, move |visible, _window, _cx| {
        store.request_thumbs(visible.clone(), thumb_scale, page_count);
        visible
            .map(|index| {
                let selected = index == current;
                // The theme's selection colour is tuned to sit *behind* text,
                // so in a dark theme it is a near-black navy — invisible as a
                // ring against the rail. Keep its hue, force it bright: this
                // is the one mark that tells the reader where they are, and
                // it has to read at a glance in either theme.
                let ring = gpui::Hsla {
                    s: 0.85,
                    l: 0.58,
                    a: 1.0,
                    ..colors.accent
                };
                // A faint wash of that same bright colour, so the cell reads
                // as selected without becoming a dark block.
                let tint = gpui::Hsla { a: 0.12, ..ring };
                let sheet = gpui::div()
                    .w(px(THUMB_WIDTH_PX))
                    .h(px(thumb_h))
                    .flex_none()
                    .bg(gpui::white())
                    .border_1()
                    .border_color(colors.border)
                    .when(!selected, |sheet| sheet.shadow_sm())
                    .when_some(store.thumb(index), |sheet, bitmap| {
                        sheet.child(img(bitmap).w(px(THUMB_WIDTH_PX)).h(px(thumb_h)))
                    });
                let view = view.clone();
                gpui::div()
                    .id(("pdf-thumb", index))
                    .h(px(row_h))
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.0))
                    .cursor_pointer()
                    .hover(|row| row.bg(gpui::Hsla { a: 0.08, ..ring }))
                    .child(
                        gpui::div()
                            .p(px(THUMB_INSET_PX))
                            .rounded(px(5.0))
                            .border(px(THUMB_RING_PX))
                            // The ring is always laid out, transparent when
                            // the page is not current, so selecting one
                            // cannot nudge the rail by two pixels.
                            .border_color(if selected {
                                ring
                            } else {
                                gpui::transparent_black()
                            })
                            .when(selected, |cell| cell.bg(tint))
                            .child(sheet),
                    )
                    .child(
                        gpui::div()
                            .h(px(THUMB_LABEL_H_PX))
                            .flex()
                            .items_center()
                            .text_size(px(text_size * 0.85))
                            // The ring carries the selection; the label only
                            // needs to agree with it, not compete.
                            .when(selected, |label| {
                                label
                                    .text_color(ring)
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                            })
                            .when(!selected, |label| label.text_color(colors.muted_fg))
                            .child(format!("{}", index + 1)),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.pdf_goto_page(index, window, cx);
                        });
                    })
            })
            .collect()
    })
    .track_scroll(&content.thumb_list)
    .size_full();

    // A flex column with `min_h_0` around the list: without it the rail's
    // height resolves to its content (651 rows), the list is handed a box it
    // already fits, and nothing scrolls.
    gpui::div()
        .w(px(THUMB_RAIL_WIDTH_PX))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .min_h_0()
        .border_r_1()
        .border_color(colors.border)
        .bg(colors.surface)
        .child(gpui::div().flex_1().min_h_0().py(px(8.0)).child(list))
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
        objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] >>").into_bytes());
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
        assert_eq!(doc.max_page_size(), (200.0, 100.0));
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
    fn page_key_quantises_scale() {
        assert_eq!(PageKey::new(3, 2.0), PageKey::new(3, 2.0004));
        assert_ne!(PageKey::new(3, 2.0), PageKey::new(4, 2.0));
        assert_eq!(PageKey::new(3, 2.0).scale(), 2.0);
    }

    #[test]
    fn zoom_ladder_steps_from_the_effective_scale() {
        // ⌘+ from a fit mode lands on the next rung ABOVE what is on screen,
        // not on the next rung above some stored value.
        assert_eq!(zoom_stepped(0.83, 1), 0.90);
        assert_eq!(zoom_stepped(0.83, -1), 0.80);
        // Exact rungs move off themselves, never stall.
        assert_eq!(zoom_stepped(1.0, 1), 1.10);
        assert_eq!(zoom_stepped(1.0, -1), 0.90);
        // Multi-step (a wheel fling) walks several rungs at once.
        assert_eq!(zoom_stepped(1.0, 3), 1.50);
        // The ends clamp instead of wrapping or overshooting.
        assert_eq!(zoom_stepped(4.0, 1), 4.0);
        assert_eq!(zoom_stepped(0.25, -1), 0.25);
        assert_eq!(zoom_stepped(1.0, 0), 1.0);
    }

    #[test]
    fn fit_modes_use_the_axis_they_name() {
        let page = (600.0, 800.0);
        let pane = (1032.0, 412.0);
        let w = PdfZoom::FitWidth.effective_scale(page, pane);
        let p = PdfZoom::FitPage.effective_scale(page, pane);
        assert!((w - 1000.0 / 600.0).abs() < 1e-4, "fit width fills the pane");
        assert!(
            (p - 400.0 / 800.0).abs() < 1e-4,
            "fit page takes the tighter axis, gap included"
        );
        assert!(p < w, "the whole page is smaller than a width fit here");
        assert_eq!(PdfZoom::Scale(1.25).effective_scale(page, pane), 1.25);
    }

    /// The queue must serve the page the reader is looking at, not one they
    /// scrolled past — which is why it is LIFO and pushed furthest-first.
    #[test]
    fn the_queue_serves_the_most_recent_request_first() {
        let queue = JobQueue::new();
        queue.push([
            Job::Page(PageKey::new(9, 1.0)),
            Job::Page(PageKey::new(5, 1.0)),
        ]);
        let Some(Job::Page(first)) = queue.take() else {
            panic!("a job was queued");
        };
        assert_eq!(first.page, 5, "the last pushed is the first served");
        let Some(Job::Page(second)) = queue.take() else {
            panic!("two jobs were queued");
        };
        assert_eq!(second.page, 9);
        queue.close();
        assert!(queue.take().is_none(), "a closed queue releases its workers");
    }

    #[test]
    fn a_closed_queue_drops_what_is_still_pending() {
        let queue = JobQueue::new();
        queue.push([Job::Page(PageKey::new(1, 1.0))]);
        queue.close();
        assert!(queue.take().is_none());
    }

    /// The whole point of the window: the visible pages are queued last, so
    /// a LIFO worker takes them first, and the margins follow outward.
    #[test]
    fn the_request_window_is_ordered_from_the_viewport_outward() {
        // A renderer with no workers: nothing drains the queue, so the test
        // can read the order the store put it in.
        let store = PageStore::new(PdfRenderer {
            queue: Arc::new(JobQueue::new()),
        });
        store.request_window(10..12, 1.0, 40);
        let mut served = Vec::new();
        while let Some(Job::Page(key)) = store.renderer.queue.try_take() {
            served.push(key.page);
            if served.len() == 4 {
                break;
            }
        }
        assert_eq!(served, vec![10, 11, 9, 12], "the viewport first, then outward");
        assert_eq!(store.pending.borrow().len(), 10, "6..16, asked for once");

        // Asking again queues nothing: those pages are already in flight.
        store.request_window(10..12, 1.0, 40);
        assert_eq!(store.pending.borrow().len(), 10, "no repeat request");
    }

    #[test]
    fn a_zoom_change_forgets_the_pages_rendered_at_the_old_scale() {
        let doc = Arc::new(PdfDocument::parse(test_pdf(3)).expect("valid pdf"));
        let store = PageStore::new(PdfRenderer {
            queue: Arc::new(JobQueue::new()),
        });
        let old = PageKey::new(0, 1.0);
        let new = PageKey::new(0, 2.0);
        store
            .pages
            .borrow_mut()
            .insert(old, doc.render_page(0, 1.0).expect("renders"));
        store
            .pages
            .borrow_mut()
            .insert(new, doc.render_page(0, 2.0).expect("renders"));
        let released = store.drop_other_scales(new.scale_milli);
        assert_eq!(released.len(), 1, "only the old scale is released");
        assert_eq!(store.resident_pages(), 1);
        assert!(store.page(new).is_some(), "the current scale survives");
    }
}
