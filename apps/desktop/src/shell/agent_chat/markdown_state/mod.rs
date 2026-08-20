//! The mutable half of chat markdown: parser state per message, and code
//! highlighting that arrives when it arrives.
//!
//! [`super::markdown_render`] is pure and knows none of this. It asks a
//! [`CodeHighlights`] whether a fence's colors exist yet and draws plain if they
//! do not — which is the whole reason highlighting is allowed to be slow.
//!
//! ## Why a parser is kept per message
//!
//! A streaming reply is re-rendered on every batch of tokens. Re-reading the
//! whole message each time is quadratic in the length of the reply, and long
//! replies are exactly where that hurts. Keeping the parser alive lets it reuse
//! the blocks it already read and re-read only the tail.
//!
//! ## Why highlighting does not happen here
//!
//! Tokenizing a long fence on the UI thread stalls the frame that asked for it.
//! So a miss returns `None`, the fence draws as plain text at its final size and
//! position, and the work is dispatched to the background executor; the result
//! arrives as a repaint that changes colors and nothing else.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, EntityId, FocusHandle, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement, ScrollHandle, SharedString, Styled, Window, div,
};
use oximux_markdown::{BlockTree, IncrementalParser, TopBlock};
use oximux_syntax::{HighlightCache, HighlightedDocument, LanguageId};

use super::AgentChatView;
mod row_cache;

use super::markdown_render::{self, CodeHighlights, FenceKey, FenceScrolls, MarkdownStyle};
use row_cache::RowCache;
use super::markdown_select::Selection;

/// Which document's markdown this is.
///
/// A transcript position alone would collide two ways: an assistant entry has
/// both a reply and a thinking trace, and they are different documents; and a
/// plan card is identified by the tool call that raised it rather than by where
/// it happens to sit, which is what keeps it addressable when the transcript
/// above it shifts.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum MdKey {
    Reply(usize),
    Thinking(usize),
    Plan(u64),
}

/// The `part` of an undivided document, for [`Markdown::with_selection`]. Not a
/// block index — a message rendered whole and the same message's block 0 are
/// different elements and must not share an id.
const WHOLE_DOCUMENT: usize = usize::MAX;

impl MdKey {
    /// The transcript position this document belongs to, for pruning. `None`
    /// for a document that is not addressed by position.
    pub fn entry(&self) -> Option<usize> {
        match self {
            Self::Reply(ix) | Self::Thinking(ix) => Some(*ix),
            Self::Plan(_) => None,
        }
    }

    /// A stable per-document seed for element ids inside it.
    pub fn seed(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// A view's markdown state, shared by handle.
///
/// Shared because the pieces that render markdown are free functions holding a
/// `Context`, not methods on the view, and none of them can borrow the view
/// while the view is rendering them. Cloning a handle is an `Rc` bump.
#[derive(Clone)]
pub(super) struct Markdown {
    /// The view to repaint when a selection changes.
    ///
    /// Captured once at construction, NOT read from the window inside the
    /// handlers: `Window::current_view` is only legal during request_layout,
    /// prepaint or paint, and a mouse callback is none of those — calling it
    /// there aborts the process rather than returning an error.
    owner: EntityId,
    /// The chat's focus handle. Starting a selection focuses the chat, which is
    /// what puts it on the dispatch path for the `Copy` action — a selection
    /// nothing can copy is not a selection.
    focus: FocusHandle,
    state: Rc<RefCell<MarkdownState>>,
    /// The chat's one text selection. Held here rather than on the view because
    /// the elements that paint it are built by cx-free renderers that already
    /// carry this handle.
    pub selection: Selection,
}

impl Markdown {
    /// Bind this handle to the view that owns it.
    pub fn new(owner: EntityId, focus: FocusHandle) -> Self {
        Self { owner, focus, state: Rc::default(), selection: Selection::default() }
    }

    /// Render one message's markdown through the owned renderer.
    fn render_document(&self, key: MdKey, text: &str, style: &MarkdownStyle) -> AnyElement {
        let (tree, hl) = self.tree_and_highlights(key, text);
        let scrolls = self.fence_scrolls();
        #[cfg(test)]
        self.state.borrow().blocks_rendered.set(
            self.state.borrow().blocks_rendered.get() + tree.blocks.len(),
        );
        markdown_render::render_document(&tree, style, &hl, &scrolls)
    }

    /// The tree and the highlight handle, both taken before rendering starts so
    /// no borrow of this state is held while the renderer runs — it reaches back
    /// into the highlight store on every fence.
    fn tree_and_highlights(&self, key: MdKey, text: &str) -> (BlockTree, CodeHighlightStore) {
        let mut state = self.state.borrow_mut();
        let tree = state.tree(key, text);
        let hl = state.highlights();
        (tree, hl)
    }

    /// How many top-level blocks this message currently has — that is, how many
    /// transcript rows it wants.
    ///
    /// Asked by the row builder before anything renders, and cheap for the
    /// reason the parser is kept per message at all: re-setting text the parser
    /// already holds does no work, so the row builder and the renderer that
    /// follows it parse the message once between them.
    ///
    /// The legacy renderer has no block tree, so it reports one block — its
    /// whole body on a single row, which is how it drew before this existed.
    pub fn block_count(&self, key: MdKey, text: &str) -> usize {
        if !owned_renderer() {
            return 1;
        }
        self.state.borrow_mut().block_count(key, text)
    }

    /// Which top-level block the source byte at `offset` belongs to.
    ///
    /// The find bar's jump target: a match forty paragraphs into a reply should
    /// scroll to the paragraph, not to the reply's header. `None` when the
    /// legacy renderer is on — it has no blocks, so its whole body is one row
    /// and the head row is already the right answer.
    pub fn block_of_offset(&self, key: MdKey, text: &str, offset: usize) -> Option<usize> {
        if !owned_renderer() {
            return None;
        }
        block_at_offset(&self.state.borrow_mut().tree(key, text), offset)
    }

    /// One top-level block of a message, as its own element.
    ///
    /// Carries the same per-message mouse handling as [`Self::render`]: the
    /// selection is scoped to the [`MdKey`], not to the element, so a drag that
    /// starts in one block and continues into the next block of the same
    /// message keeps extending — the moves land on a different element, but they
    /// report the same key.
    pub fn render_block(
        &self,
        key: MdKey,
        text: &str,
        block_ix: usize,
        style: &MarkdownStyle,
    ) -> Option<AnyElement> {
        // One block, not the tree. Cloning the whole tree here would scale the
        // per-frame cost with the length of the *message* once more — ten
        // visible rows of a two-hundred-block reply would clone two thousand
        // blocks a frame — which is the factor block granularity just removed.
        let (top, hl) = {
            let mut state = self.state.borrow_mut();
            let top = state.block(key, text, block_ix)?;
            let hl = state.highlights();
            (top, hl)
        };
        let scrolls = self.fence_scrolls();
        #[cfg(test)]
        self.state.borrow().blocks_rendered.set(self.state.borrow().blocks_rendered.get() + 1);
        let body = markdown_render::render_block_at(&top, block_ix, style, &hl, &scrolls);
        Some(self.with_selection(key, block_ix, body))
    }

    /// One message's markdown, wrapped in the mouse handling that drives its
    /// selection.
    ///
    /// The handlers sit on the message's own container, which is what scopes a
    /// selection to one message: a drag that wanders into the next reply stops
    /// receiving moves, so it stops extending. `on_mouse_down_out` is the other
    /// half — clicking anywhere else drops the selection rather than leaving a
    /// stale highlight behind.
    pub fn render(&self, key: MdKey, text: &str, style: &MarkdownStyle) -> AnyElement {
        let body = self.render_document(key, text, style);
        self.with_selection(key, WHOLE_DOCUMENT, body)
    }

    /// Wrap one rendered piece of a message — a whole document, or one block of
    /// it — in the mouse handling that drives the message's selection.
    ///
    /// `part` only disambiguates the element id; every other thing here is
    /// keyed by [`MdKey`], which is what keeps one message's selection one
    /// selection however many rows it is spread across.
    fn with_selection(&self, key: MdKey, part: usize, body: AnyElement) -> AnyElement {
        let sel = self.selection.clone();
        let owner = self.owner;
        let focus = self.focus.clone();
        div()
            .id(SharedString::from(format!("chat-md-{}-{part}", key.seed())))
            .w_full()
            .min_w_0()
            .child(body)
            .on_mouse_down(MouseButton::Left, {
                let sel = sel.clone();
                move |ev: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                    // Deferred, not synchronous: gpui's own post-click focus
                    // dispatch runs after this handler and clobbers a focus set
                    // here. Unfocused, the chat is off the dispatch path and
                    // Copy reaches nothing.
                    let focus = focus.clone();
                    window.defer(cx, move |window, cx| window.focus(&focus, cx));
                    sel.begin(key, ev.position);
                    cx.notify(owner);
                }
            })
            .on_mouse_move({
                let sel = sel.clone();
                move |ev: &MouseMoveEvent, _window, cx: &mut App| {
                    // Only while the button is held. A bare hover crossing a
                    // settled selection must not redraw it.
                    if ev.pressed_button == Some(MouseButton::Left)
                        && sel.extend(key, ev.position)
                    {
                        cx.notify(owner);
                    }
                }
            })
            .on_mouse_down_out(move |ev: &MouseDownEvent, _window, cx: &mut App| {
                // Guarded, because the sibling blocks of THIS message are
                // "out" of this element too — see [`Selection::dismiss`].
                if sel.is_active() {
                    sel.dismiss(ev.position);
                    cx.notify(owner);
                }
            })
            .into_any_element()
    }

    pub fn retain_entries(&self, len: usize) {
        self.state.borrow_mut().retain_entries(len);
    }

    /// A handle the pure renderer can ask for a fence's scroll position.
    ///
    /// Taken outside the `state` borrow the renderer runs under, like the
    /// highlight store: it is its own `Rc<RefCell<_>>`, so asking it for a
    /// handle mid-render is not a re-entrant borrow of [`MarkdownState`].
    fn fence_scrolls(&self) -> FenceScrollStore {
        FenceScrollStore(Rc::clone(&self.state.borrow().fence_scrolls))
    }

    /// Lookups that fell through to the parser. Test-only — see [`RowCache`].
    #[cfg(test)]
    pub fn row_cache_probes(&self) -> usize {
        self.state.borrow().rows.probes()
    }

    /// Markdown blocks turned into elements so far. Test-only, and the honest
    /// unit for what block granularity bought: the *row* count went up, and the
    /// work behind a frame is what went down.
    #[cfg(test)]
    pub fn blocks_rendered(&self) -> usize {
        self.state.borrow().blocks_rendered.get()
    }

    /// Test-only, so a measurement can start from a known point.
    #[cfg(test)]
    pub fn reset_blocks_rendered(&self) {
        self.state.borrow().blocks_rendered.set(0);
    }

    pub fn dispatch_highlighting(&self, cx: &mut Context<AgentChatView>) {
        let highlights = Rc::clone(&self.state.borrow().highlights);
        MarkdownState::dispatch(highlights, cx);
    }
}

/// The first block that ends after `offset`.
///
/// "Ends after" rather than "contains", because a block's range covers the
/// block and not the blank line that separates it from the next — an offset
/// landing in that gap belongs to nothing, and answering with the block the
/// reader is heading towards beats answering `None` and jumping to the top of
/// the message instead.
fn block_at_offset(tree: &BlockTree, offset: usize) -> Option<usize> {
    tree.blocks.iter().position(|b| offset < b.range.end)
}

/// Per-view markdown state: one parser per live message, one highlight store.
#[derive(Default)]
struct MarkdownState {
    parsers: HashMap<MdKey, IncrementalParser>,
    highlights: Rc<RefCell<Highlights>>,
    /// Block counts, so building the row list does not re-read every message on
    /// every frame. See [`RowCache`].
    rows: RowCache,
    /// One horizontal scroll position per fence, surviving the frame that made
    /// it. Its own `Rc` so the renderer can reach it while this struct is
    /// borrowed — see [`Markdown::fence_scrolls`].
    fence_scrolls: Rc<RefCell<HashMap<FenceKey, ScrollHandle>>>,
    /// Blocks turned into elements, for tests that measure frame cost.
    #[cfg(test)]
    blocks_rendered: std::cell::Cell<usize>,
}

impl MarkdownState {
    /// The block tree for `text`, reusing whatever the last parse of this
    /// message already established.
    ///
    /// Returns a clone rather than a borrow because the caller renders while
    /// holding it, and rendering reaches back into the view.
    fn tree(&mut self, key: MdKey, text: &str) -> BlockTree {
        self.parse(key, text).tree().clone()
    }

    /// One top-level block of `key`'s document, cloned on its own.
    fn block(&mut self, key: MdKey, text: &str, ix: usize) -> Option<TopBlock> {
        self.parse(key, text).tree().blocks.get(ix).cloned()
    }

    /// How many top-level blocks `key`'s document has, answered from the row
    /// cache when the message's length says nothing can have changed.
    ///
    /// The cache is the whole point: this is asked for *every* message on every
    /// frame, and the parser's own "has this changed?" is a full comparison of
    /// the text.
    fn block_count(&mut self, key: MdKey, text: &str) -> usize {
        if let Some(n) = self.rows.hit(key, text.len()) {
            return n;
        }
        let n = self.parse(key, text).tree().blocks.len();
        self.rows.store(key, text.len(), n);
        n
    }

    /// The parser for `key`, brought up to date with `text`.
    fn parse(&mut self, key: MdKey, text: &str) -> &IncrementalParser {
        let parser = self.parsers.entry(key).or_default();
        parser.set_text(text);
        parser
    }

    /// Drop parser state for messages that no longer exist.
    ///
    /// A rewind or a context compaction shortens the transcript, and the
    /// parsers behind the removed messages would otherwise be retained for the
    /// life of the view holding the full text of each. Indices below the new
    /// length keep their parsers: they are the same messages, and re-reading
    /// them from scratch is the cost this cache exists to avoid.
    fn retain_entries(&mut self, len: usize) {
        // Eagerly, unlike the parsers below: a retained count is a wrong answer
        // rather than a wasted allocation. See [`RowCache::retain_entries`].
        self.rows.retain_entries(len);
        // Fence scroll positions belong to messages too, and a rewind frees the
        // indices they are keyed under. Left behind, the next reply to land on a
        // reused index would open its fence already scrolled sideways.
        self.fence_scrolls
            .borrow_mut()
            .retain(|(key, _, _), _| key.entry().is_none_or(|ix| ix < len));
        if self.parsers.len() > len.saturating_mul(2) {
            self.parsers.retain(|k, _| k.entry().is_none_or(|ix| ix < len));
        }
    }

    /// A handle the pure renderer can ask for fence colors.
    fn highlights(&self) -> CodeHighlightStore {
        CodeHighlightStore(Rc::clone(&self.highlights))
    }

    /// Start background work for every fence that missed while rendering.
    ///
    /// Called after the frame is built, not during it: dispatching mid-render
    /// would be spawning tasks from inside the closure that is deciding what
    /// the frame looks like.
    fn dispatch(highlights: Rc<RefCell<Highlights>>, cx: &mut Context<AgentChatView>) {
        let wanted = std::mem::take(&mut highlights.borrow_mut().wanted);
        for (key, lang, code) in wanted {
            let store = Rc::clone(&highlights);
            cx.spawn(async move |view, cx| {
                let task = cx.update(|cx| {
                    let (lang, code) = (lang.clone(), code.clone());
                    cx.background_executor()
                        .spawn(async move { oximux_syntax::highlight(&lang, &code) })
                });
                let doc = Arc::new(task.await);
                {
                    let mut store = store.borrow_mut();
                    store.cache.insert(&lang, &code, doc);
                    store.in_flight.remove(&key);
                }
                // Colors changed; nothing measured did. The repaint is the
                // whole delivery mechanism.
                let _ = view.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
    }
}

#[derive(Default)]
struct Highlights {
    cache: HighlightCache,
    /// Fences that missed during the frame being built, awaiting dispatch.
    wanted: Vec<(u64, LanguageId, String)>,
    /// Fences already dispatched. Without this, a fence visible for twenty
    /// frames before its result lands is tokenized twenty times.
    in_flight: HashSet<u64>,
}

/// The renderer's view of the fence scroll positions: ask for a fence's handle
/// and get the same one every frame, or a fresh one the first time.
#[derive(Clone)]
struct FenceScrollStore(Rc<RefCell<HashMap<FenceKey, ScrollHandle>>>);

impl FenceScrolls for FenceScrollStore {
    fn handle(&self, key: FenceKey) -> ScrollHandle {
        self.0.borrow_mut().entry(key).or_default().clone()
    }
}

/// The renderer's view of the highlight store: ask, and either get colors or
/// get the work started.
#[derive(Clone)]
struct CodeHighlightStore(Rc<RefCell<Highlights>>);

impl CodeHighlights for CodeHighlightStore {
    fn colors(&self, lang: &LanguageId, code: &str) -> Option<Arc<HighlightedDocument>> {
        let mut store = self.0.borrow_mut();
        if let Some(doc) = store.cache.peek(lang, code) {
            return Some(doc);
        }
        let key = job_key(lang, code);
        if store.in_flight.insert(key) {
            store.wanted.push((key, lang.clone(), code.to_string()));
        }
        None
    }
}

/// Identifies one dispatch. Content-keyed like the cache itself, so the same
/// fence rendered in two places is tokenized once.
fn job_key(lang: &LanguageId, code: &str) -> u64 {
    let mut h = DefaultHasher::new();
    lang.name().hash(&mut h);
    code.hash(&mut h);
    h.finish()
}

/// Whether the chat renders markdown with the owned renderer or the previous
/// `TextView`.
///
/// An escape hatch, not a feature, and the same shape as the transcript's:
/// chat markdown is the most-looked-at surface in the product, and
/// `OXIMUX_LEGACY_MARKDOWN=1` is the way back without waiting for a release.
/// Read once per process so a mid-session flip cannot leave half the transcript
/// rendered each way.
pub(super) fn owned_renderer() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OXIMUX_LEGACY_MARKDOWN").is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply and the thinking trace of one entry are different documents
    /// and must not share a parser — sharing would make each `set_text` a full
    /// reparse of unrelated text, and land the wrong tree in whichever rendered
    /// second.
    #[test]
    fn a_reply_and_its_thinking_trace_do_not_share_a_parser() {
        let mut state = MarkdownState::default();
        let reply = state.tree(MdKey::Reply(3), "# reply\n");
        let thinking = state.tree(MdKey::Thinking(3), "plain thinking\n");

        assert!(matches!(
            reply.blocks[0].block,
            oximux_markdown::Block::Heading { .. }
        ));
        assert!(matches!(
            thinking.blocks[0].block,
            oximux_markdown::Block::Paragraph { .. }
        ));
        assert_eq!(state.parsers.len(), 2);
    }

    /// A match deep in a long reply has to resolve to the block it is in, or
    /// the find bar scrolls to the top of the message and the reader is left
    /// looking for it.
    #[test]
    fn an_offset_resolves_to_the_block_it_falls_in() {
        let doc = "# Title\n\nfirst para\n\nsecond para\n";
        let tree = MarkdownState::default().tree(MdKey::Reply(0), doc);
        assert_eq!(tree.blocks.len(), 3);

        let at = |needle: &str| block_at_offset(&tree, doc.find(needle).unwrap());
        assert_eq!(at("Title"), Some(0));
        assert_eq!(at("first"), Some(1));
        assert_eq!(at("second"), Some(2));

        // The gap between two blocks belongs to neither; answer with the one
        // the reader is heading towards rather than with nothing.
        let gap = doc.find("first para").unwrap() - 1;
        assert_eq!(block_at_offset(&tree, gap), Some(1));

        // Past the end resolves to nothing rather than to the last block — the
        // caller falls back to the message's head row, which is honest.
        assert_eq!(block_at_offset(&tree, doc.len()), None);
    }

    /// The point of keeping the parser: appending to a reply must not re-read
    /// the blocks already settled above the append.
    #[test]
    fn appending_to_a_reply_reuses_the_settled_prefix() {
        let mut state = MarkdownState::default();
        let key = MdKey::Reply(0);
        let long = "# Title\n\nfirst para\n\nsecond para\n\nthird ";
        state.tree(key, long);
        state.tree(key, &format!("{long}para"));

        let parser = &state.parsers[&key];
        assert!(parser.stable_prefix_blocks() > 0, "nothing was reused");
        assert!(
            parser.last_parse_bytes() < parser.text().len(),
            "the whole document was re-read"
        );
    }

    /// A rewind shortens the transcript; the parsers behind the removed
    /// messages hold the full text of each and must not be retained.
    #[test]
    fn pruning_drops_parsers_past_the_end() {
        let mut state = MarkdownState::default();
        for i in 0..20 {
            state.tree(MdKey::Reply(i), "text\n");
        }
        state.tree(MdKey::Plan(7), "a plan\n");
        state.retain_entries(3);
        assert!(
            state.parsers.keys().all(|k| k.entry().is_none_or(|ix| ix < 3)),
            "stale parsers kept",
        );
        assert!(
            state.parsers.contains_key(&MdKey::Plan(7)),
            "a card addressed by tool id was pruned by transcript length",
        );
    }

    /// A miss must enqueue the work exactly once, however many frames the fence
    /// stays on screen before its colors land.
    #[test]
    fn a_fence_is_dispatched_once_not_once_per_frame() {
        let state = MarkdownState::default();
        let store = state.highlights();
        let lang = oximux_syntax::detect(None, Some("rust"), "").expect("rust grammar");

        assert!(store.colors(&lang, "let x = 1;\n").is_none());
        assert!(store.colors(&lang, "let x = 1;\n").is_none());
        assert!(store.colors(&lang, "let x = 1;\n").is_none());

        assert_eq!(state.highlights.borrow().wanted.len(), 1);
    }
}
