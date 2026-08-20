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
    MouseDownEvent, MouseMoveEvent, ParentElement, SharedString, Styled, Window, div,
};
use oximux_markdown::{BlockTree, IncrementalParser};
use oximux_syntax::{HighlightCache, HighlightedDocument, LanguageId};

use super::AgentChatView;
use super::markdown_render::{self, CodeHighlights, MarkdownStyle};
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
    fn entry(&self) -> Option<usize> {
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
        markdown_render::render_document(&tree, style, &hl)
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
        self.state.borrow_mut().tree(key, text).blocks.len()
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
        let (tree, hl) = self.tree_and_highlights(key, text);
        let body = markdown_render::render_block_at(&tree, block_ix, style, &hl)?;
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

    pub fn dispatch_highlighting(&self, cx: &mut Context<AgentChatView>) {
        let highlights = Rc::clone(&self.state.borrow().highlights);
        MarkdownState::dispatch(highlights, cx);
    }
}

/// Per-view markdown state: one parser per live message, one highlight store.
#[derive(Default)]
struct MarkdownState {
    parsers: HashMap<MdKey, IncrementalParser>,
    highlights: Rc<RefCell<Highlights>>,
}

impl MarkdownState {
    /// The block tree for `text`, reusing whatever the last parse of this
    /// message already established.
    ///
    /// Returns a clone rather than a borrow because the caller renders while
    /// holding it, and rendering reaches back into the view.
    fn tree(&mut self, key: MdKey, text: &str) -> BlockTree {
        let parser = self.parsers.entry(key).or_default();
        parser.set_text(text);
        parser.tree().clone()
    }

    /// Drop parser state for messages that no longer exist.
    ///
    /// A rewind or a context compaction shortens the transcript, and the
    /// parsers behind the removed messages would otherwise be retained for the
    /// life of the view holding the full text of each. Indices below the new
    /// length keep their parsers: they are the same messages, and re-reading
    /// them from scratch is the cost this cache exists to avoid.
    fn retain_entries(&mut self, len: usize) {
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
