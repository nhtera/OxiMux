//! Text selection over the owned markdown renderer.
//!
//! ## What is selectable, and why that is not a compromise
//!
//! A selection spans the many elements *inside* one message — a paragraph, then
//! a fence, then a list item — and stops at the message boundary. That is what
//! the previous renderer did (its selection state was one entity per rendered
//! document, and a mouse-down anywhere else cleared it), and it is what this
//! reproduces. Dragging from one reply into the next has never selected across
//! both.
//!
//! ## The model: anchors, not indices
//!
//! Two window-space points. Every text element independently asks its own laid
//! out text which byte the first point falls on and which byte the second falls
//! on, and takes what lies between. No element needs to know that any other
//! element exists, in what order, or how long its text is — which is exactly
//! what makes selection across a tree of elements tractable at all.
//!
//! The cost is two `index_for_position` calls per selected element per paint —
//! not one per character. Resolving through the text layout also gives *flow*
//! order rather than a rectangle: dragging from the middle of one line to the
//! middle of the next selects the end of the first and the start of the second,
//! the way text selection is supposed to behave and the way a geometric band
//! test does not.
//!
//! ## What it costs when idle
//!
//! Nothing. With no anchors set, every element's paint is one `Option` check.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClipboardItem, Edges, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, StyledText, Window, point, px, quad,
};

use super::markdown_state::MdKey;

/// A text element's position in its document: `(top-level block, position
/// within that block)`.
///
/// A pair rather than one running count because a message's blocks are rendered
/// independently — each is its own transcript row — so no renderer sees a
/// document-wide running total. Lexicographic order on the pair *is* document
/// order, which is all the copy reassembly ever wanted from it.
pub(super) type TextOrd = (usize, usize);

/// What kind of text an element holds, for the one decision that depends on it:
/// how a copy re-joins it to the piece before it.
///
/// Blocks of prose are separated by a blank line, because the separator between
/// two blocks is in neither of them and pasting a heading onto its paragraph
/// would be wrong. Lines of a fence are not: they are one thing the author
/// wrote, and blank-lining them turns copied code into something that no longer
/// runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TextPart {
    /// A block of prose — paragraph, heading, table cell, list item.
    Prose,
    /// One line of the `fence`-th code fence of its block.
    CodeLine { fence: usize },
}

/// A drag in progress or a selection that has settled.
#[derive(Clone, Copy)]
struct Anchors {
    /// Whose document this selection belongs to. An element from any other
    /// document ignores it entirely — that is where the message boundary is
    /// enforced.
    key: MdKey,
    down: Point<Pixels>,
    now: Point<Pixels>,
}

impl Anchors {
    /// The two points in reading order.
    ///
    /// A drag upward has its anchors reversed, and every consumer wants
    /// first-then-last. Compared by line before column, because two points on
    /// the same line differ only in `x` while two on different lines differ
    /// only meaningfully in `y`.
    fn ordered(&self) -> (Point<Pixels>, Point<Pixels>) {
        let backwards = self.now.y < self.down.y
            || (self.now.y == self.down.y && self.now.x < self.down.x);
        if backwards { (self.now, self.down) } else { (self.down, self.now) }
    }
}

/// One element's contribution to the selection.
struct Captured {
    text: String,
    part: TextPart,
}

#[derive(Default)]
struct State {
    anchors: Option<Anchors>,
    /// What each text element found selected, keyed by [`TextOrd`] so a copy
    /// reassembles in reading order. Rebuilt every paint — an element that
    /// scrolled out of view stops contributing, which is correct: it is no
    /// longer laid out, so what it holds is no longer known.
    captured: BTreeMap<TextOrd, Captured>,
}

/// The chat view's selection, shared by handle.
#[derive(Clone, Default)]
pub(super) struct Selection(Rc<RefCell<State>>);

impl Selection {
    /// Begin a drag inside `key`'s document. Any selection in another message
    /// is dropped — there is one selection at a time.
    pub fn begin(&self, key: MdKey, at: Point<Pixels>) {
        let mut state = self.0.borrow_mut();
        state.anchors = Some(Anchors { key, down: at, now: at });
        state.captured.clear();
    }

    /// Extend the drag. Ignored unless a drag started in this document, which
    /// is what keeps a drag that wandered into the next message from selecting
    /// there.
    pub fn extend(&self, key: MdKey, to: Point<Pixels>) -> bool {
        let mut state = self.0.borrow_mut();
        match &mut state.anchors {
            Some(a) if a.key == key => {
                a.now = to;
                true
            }
            _ => false,
        }
    }

    pub fn clear(&self) {
        let mut state = self.0.borrow_mut();
        state.anchors = None;
        state.captured.clear();
    }

    /// A mouse-down landed outside one of this document's elements: drop the
    /// selection unless that same press is the one that just started a new one.
    ///
    /// The guard exists because a message is no longer a single element. Its
    /// blocks are separate transcript rows, each carrying its own
    /// `on_mouse_down_out`, so pressing inside block 2 is "outside" blocks 1
    /// and 3 of the very same message — and an unguarded clear would wipe the
    /// selection the press was in the middle of starting.
    ///
    /// Comparing against the press position rather than tracking dispatch order
    /// is what makes it order-independent, and gpui does not promise an order
    /// between a sibling's out-handler and this element's own down-handler. If
    /// the out-handler runs first there is nothing to clear yet and
    /// [`Self::begin`] anchors afterwards; if it runs second the anchors are
    /// already at `at` and this returns. A press genuinely outside the message
    /// began nothing, so its position matches no fresh anchor and the selection
    /// goes.
    pub fn dismiss(&self, at: Point<Pixels>) {
        let just_begun_here =
            self.0.borrow().anchors.is_some_and(|a| a.down == at && a.now == at);
        if just_begun_here {
            return;
        }
        self.clear();
    }

    /// Whether anything is actually selected — not merely whether a click
    /// happened. A bare click sets both anchors to the same point and selects
    /// no characters, and must not make ⌘C copy an empty string.
    pub fn is_active(&self) -> bool {
        !self.0.borrow().captured.is_empty()
    }

    /// The selected text, in document order.
    ///
    /// Blocks are joined with a blank line rather than concatenated: the source
    /// separator between two blocks is not in either of them, and pasting a
    /// heading run into its paragraph would be wrong.
    ///
    /// **Consecutive lines of one fence are the exception**, and joining those
    /// with a blank line too — which this did until it was caught — double-spaces
    /// every copied snippet and can break the code outright, since indentation-
    /// sensitive languages read a blank line as the end of a block. They are one
    /// thing the author wrote, so they are rejoined with the single newline that
    /// was between them.
    ///
    /// "One fence" is decided by block AND fence ordinal, not by "both are code":
    /// two fences inside a single list item are one top-level block, and running
    /// them together would lose the boundary.
    pub fn selected_text(&self) -> String {
        let state = self.0.borrow();
        let mut out = String::new();
        let mut prev: Option<(&TextOrd, &Captured)> = None;
        for (ord, cap) in &state.captured {
            if let Some((prev_ord, prev_cap)) = prev {
                let same_fence = prev_ord.0 == ord.0
                    && matches!(
                        (prev_cap.part, cap.part),
                        (TextPart::CodeLine { fence: a }, TextPart::CodeLine { fence: b })
                            if a == b
                    );
                out.push_str(if same_fence { "\n" } else { "\n\n" });
            }
            out.push_str(&cap.text);
            prev = Some((ord, cap));
        }
        out
    }

    /// Copy the selection, reporting whether there was one to copy.
    ///
    /// Reached by the menu's `Copy` action — Edit ▸ Copy — and **not by ⌘C**.
    /// Measured in a live window: with the chat focused, neither a
    /// `capture_key_down` nor an `on_action` handler fires on ⌘C, because the
    /// Edit menu declares Copy as an `OsAction` and AppKit consumes the key
    /// equivalent before gpui dispatches anything. Restoring ⌘C here means
    /// either giving the transcript its own binding or dropping `OsAction`
    /// from the menu item, which changes what ⌘C means in every other pane —
    /// a product decision, not a rendering one.
    pub fn copy(&self, cx: &mut App) -> bool {
        if !self.is_active() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text()));
        true
    }

    fn anchors_for(&self, key: MdKey) -> Option<(Point<Pixels>, Point<Pixels>)> {
        let state = self.0.borrow();
        state.anchors.filter(|a| a.key == key).map(|a| a.ordered())
    }

    fn capture(&self, ord: TextOrd, part: TextPart, text: Option<String>) {
        let mut state = self.0.borrow_mut();
        match text {
            Some(text) => {
                state.captured.insert(ord, Captured { text, part });
            }
            None => {
                state.captured.remove(&ord);
            }
        }
    }
}

/// Where a find match sits in this element's text, and how loudly to say so.
pub(super) struct MatchBand {
    pub range: std::ops::Range<usize>,
    pub tint: Hsla,
}

/// A `StyledText` that participates in the message's selection and shows the
/// find bar's matches.
///
/// Everything about layout is delegated untouched. Both jobs are the same
/// primitive — a background quad behind a byte range — and neither can move a
/// glyph, which is why find highlighting is safe to add to text that is already
/// laid out.
pub(super) struct ChatText {
    inner: StyledText,
    key: MdKey,
    /// This element's position in its document, for reassembling a copy.
    ord: TextOrd,
    /// How a copy rejoins this element to the one before it.
    part: TextPart,
    selection: Selection,
    tint: Hsla,
    /// Find matches inside this element's own text. Computed per element, so
    /// nothing has to map an offset in the source markdown onto an offset in
    /// the rendered text.
    matches: Vec<MatchBand>,
}

impl ChatText {
    pub fn new(
        inner: StyledText,
        key: MdKey,
        ord: TextOrd,
        part: TextPart,
        selection: Selection,
        tint: Hsla,
    ) -> Self {
        Self { inner, key, ord, part, selection, tint, matches: Vec::new() }
    }

    pub fn with_matches(mut self, matches: Vec<MatchBand>) -> Self {
        self.matches = matches;
        self
    }

    /// The byte range of this element's text that lies between the anchors.
    ///
    /// `index_for_position` answers for a point above the text with `Err(0)`
    /// and for one below it with the end, so an element wholly inside the
    /// selection resolves to its whole text and one wholly outside resolves to
    /// an empty range — with no special cases here.
    fn selected_range(&self, first: Point<Pixels>, last: Point<Pixels>) -> Option<(usize, usize)> {
        let layout = self.inner.layout();
        let len = layout.len();
        let resolve = |p| match layout.index_for_position(p) {
            Ok(ix) | Err(ix) => ix.min(len),
        };
        let (start, end) = (resolve(first), resolve(last));
        (start < end).then_some((start, end))
    }

    /// Paint the selection background: a partial first line, a full block of
    /// whole lines, and a partial last line — at most three quads however long
    /// the selection is.
    fn paint_bands(
        &self,
        range: (usize, usize),
        tint: Hsla,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) {
        let layout = self.inner.layout();
        let (Some(from), Some(to)) =
            (layout.position_for_index(range.0), layout.position_for_index(range.1))
        else {
            return;
        };
        let line_height = layout.line_height();
        let mut band = |a: Point<Pixels>, b: Point<Pixels>| {
            if b.x <= a.x {
                return;
            }
            window.paint_quad(quad(
                Bounds::from_corners(a, b),
                px(0.0),
                tint,
                Edges::default(),
                gpui::transparent_black(),
                gpui::BorderStyle::default(),
            ));
        };
        if from.y == to.y {
            band(from, point(to.x, to.y + line_height));
            return;
        }
        band(from, point(bounds.right(), from.y + line_height));
        if to.y > from.y + line_height {
            band(point(bounds.left(), from.y + line_height), point(bounds.right(), to.y));
        }
        band(point(bounds.left(), to.y), point(to.x, to.y + line_height));
    }
}

impl gpui::Element for ChatText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        gpui::Element::request_layout(&mut self.inner, id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        gpui::Element::prepaint(&mut self.inner, id, inspector_id, bounds, state, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut (),
        prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // The tint goes under the glyphs, so it is painted before them.
        let selected = self
            .selection
            .anchors_for(self.key)
            .and_then(|(a, b)| self.selected_range(a, b));
        // Matches first, so a selection over one still reads as selected.
        for band in &self.matches {
            self.paint_bands((band.range.start, band.range.end), band.tint, bounds, window);
        }
        if let Some(range) = selected {
            self.paint_bands(range, self.tint, bounds, window);
        }
        // Capture from the laid out text rather than from the source markdown:
        // what the reader sees selected is what they expect to have copied, and
        // the two differ wherever the renderer resolved something.
        self.selection.capture(
            self.ord,
            self.part,
            selected.and_then(|(s, e)| {
                let text = self.inner.layout().text();
                text.get(s..e).map(str::to_string)
            }),
        );
        gpui::Element::paint(
            &mut self.inner,
            id,
            inspector_id,
            bounds,
            request,
            prepaint,
            window,
            cx,
        );
    }
}

impl IntoElement for ChatText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// Byte ranges in `text` matching `query`, case-insensitively.
///
/// ASCII-lowercased on both sides rather than `to_lowercase`: a Unicode fold
/// can change a string's *length* (`İ` lowercases to two chars), and a range
/// found in a folded copy would then be the wrong range in the original. The
/// find bar's own matcher folds the same way, so the two agree on what matched.
pub(super) fn match_ranges(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() || query.len() > text.len() {
        return Vec::new();
    }
    let (hay, needle) = (text.to_ascii_lowercase(), query.to_ascii_lowercase());
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        // A match that straddles a character boundary is a match in the folded
        // bytes only — skip it rather than hand a bad range to the layout.
        if text.is_char_boundary(start) && text.is_char_boundary(end) {
            out.push(start..end);
        }
        // Non-overlapping, like every find bar: `aa` occurs once in `aaa`, not
        // twice. Overlapping matches would also paint their tints over each
        // other, so the middle of a run would read darker than its ends.
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    /// The find bar highlights every occurrence, not just the first.
    #[test]
    fn every_occurrence_matches_case_insensitively() {
        assert_eq!(match_ranges("Hi hi HI", "hi"), vec![0..2, 3..5, 6..8]);
        assert!(match_ranges("abc", "zz").is_empty());
        assert!(match_ranges("abc", "").is_empty());
    }

    /// Matches do not overlap. Two tints stacked on the same characters would
    /// read as a third colour, and no find bar counts `aa` twice in `aaa`.
    #[test]
    fn occurrences_do_not_overlap() {
        assert_eq!(match_ranges("aaa", "aa"), vec![0..2]);
        assert_eq!(match_ranges("aaaa", "aa"), vec![0..2, 2..4]);
    }

    /// A range landing mid-character would trip the text layout. Multibyte text
    /// is the normal case in a transcript, not an edge case.
    #[test]
    fn matches_never_straddle_a_character_boundary() {
        let text = "héllo héllo";
        let found = match_ranges(text, "héllo");
        assert_eq!(found.len(), 2);
        for r in found {
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
            assert_eq!(&text[r], "héllo");
        }
    }

    /// A drag upward has its anchors in the order they were made, not the order
    /// the text runs in. Every consumer wants first-then-last.
    #[test]
    fn a_backwards_drag_is_ordered_for_reading() {
        let up = Anchors { key: MdKey::Reply(0), down: p(50.0, 100.0), now: p(10.0, 20.0) };
        assert_eq!(up.ordered(), (p(10.0, 20.0), p(50.0, 100.0)));

        // ...and on one line, column decides.
        let back = Anchors { key: MdKey::Reply(0), down: p(90.0, 20.0), now: p(10.0, 20.0) };
        assert_eq!(back.ordered(), (p(10.0, 20.0), p(90.0, 20.0)));
    }

    /// The message boundary. A drag that began in one reply must not extend
    /// into the next one it passes over.
    #[test]
    fn a_drag_does_not_extend_into_another_message() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(1), p(0.0, 0.0));
        assert!(sel.extend(MdKey::Reply(1), p(10.0, 10.0)));
        assert!(!sel.extend(MdKey::Reply(2), p(10.0, 10.0)), "crossed a message");
        assert!(sel.anchors_for(MdKey::Reply(2)).is_none());
    }

    /// A bare click sets both anchors to one point and selects nothing. ⌘C then
    /// has to leave the clipboard alone rather than replacing it with "".
    #[test]
    fn a_click_that_selected_nothing_is_not_a_selection() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(5.0, 5.0));
        assert!(!sel.is_active());
        assert_eq!(sel.selected_text(), "");
    }

    /// Blocks are reassembled in document order, separated the way blocks are.
    ///
    /// Captured out of order on purpose: block rows paint in whatever order the
    /// list reaches them, so arrival order says nothing about reading order.
    /// The ordinal is the only thing that does.
    #[test]
    fn a_copy_reassembles_in_document_order() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture((1, 1), TextPart::Prose, Some("fourth".into()));
        sel.capture((0, 1), TextPart::Prose, Some("second".into()));
        sel.capture((1, 0), TextPart::Prose, Some("third".into()));
        sel.capture((0, 0), TextPart::Prose, Some("first".into()));
        assert_eq!(sel.selected_text(), "first\n\nsecond\n\nthird\n\nfourth");
        assert!(sel.is_active());
    }

    /// A press inside another block of the SAME message reaches the sibling
    /// blocks' `on_mouse_down_out` — the selection it is starting must survive
    /// that, whichever handler gpui happens to dispatch first.
    #[test]
    fn a_press_that_just_began_a_selection_is_not_dismissed_by_its_siblings() {
        let sel = Selection::default();
        let at = p(10.0, 20.0);

        // Out-handler after the down-handler.
        sel.begin(MdKey::Reply(0), at);
        sel.capture((0, 0), TextPart::Prose, Some("live".into()));
        sel.dismiss(at);
        assert!(sel.is_active(), "the press that began this selection cleared it");

        // Out-handler before the down-handler: nothing to lose, and the
        // `begin` that follows anchors normally.
        sel.clear();
        sel.dismiss(at);
        sel.begin(MdKey::Reply(0), at);
        sel.capture((0, 0), TextPart::Prose, Some("live".into()));
        assert!(sel.is_active());
    }

    /// ...but a press that began nothing still drops the selection, which is
    /// the entire job the out-handler is there to do.
    #[test]
    fn a_press_elsewhere_still_dismisses_the_selection() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(10.0, 20.0));
        sel.extend(MdKey::Reply(0), p(90.0, 40.0));
        sel.capture((0, 0), TextPart::Prose, Some("live".into()));
        sel.dismiss(p(400.0, 500.0));
        assert!(!sel.is_active());
        assert!(sel.anchors_for(MdKey::Reply(0)).is_none());
    }

    /// Lines of one fence rejoin with a single newline, not a blank line —
    /// double-spacing a copied snippet is at best ugly and at worst breaks the
    /// code, since an indentation-sensitive language reads a blank line as the
    /// end of the block.
    ///
    /// Two fences in ONE top-level block (a list item holding both) must still
    /// be separated, which is why the ordinal alone cannot decide this.
    #[test]
    fn lines_of_one_fence_rejoin_without_a_blank_line() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        let code = |fence| TextPart::CodeLine { fence };

        // Block 0: a paragraph, then a two-line fence, then a second fence.
        sel.capture((0, 0), TextPart::Prose, Some("intro".into()));
        sel.capture((0, 1), code(0), Some("fn main() {".into()));
        sel.capture((0, 2), code(0), Some("}".into()));
        sel.capture((0, 3), code(1), Some("second fence".into()));
        // Block 1: prose again.
        sel.capture((1, 0), TextPart::Prose, Some("outro".into()));

        assert_eq!(
            sel.selected_text(),
            "intro\n\nfn main() {\n}\n\nsecond fence\n\noutro",
        );
    }

    /// Same block, both code, but the fence ordinal differs — the boundary
    /// survives. Guards the half of the rule that "are they both code?" misses.
    #[test]
    fn two_fences_in_one_block_stay_separated() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture((0, 0), TextPart::CodeLine { fence: 0 }, Some("a".into()));
        sel.capture((0, 1), TextPart::CodeLine { fence: 1 }, Some("b".into()));
        assert_eq!(sel.selected_text(), "a\n\nb");
    }

    /// ...and the same fence ordinal in DIFFERENT blocks is two fences too.
    #[test]
    fn the_first_fence_of_two_blocks_is_not_one_fence() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture((0, 0), TextPart::CodeLine { fence: 0 }, Some("a".into()));
        sel.capture((1, 0), TextPart::CodeLine { fence: 0 }, Some("b".into()));
        assert_eq!(sel.selected_text(), "a\n\nb");
    }

    /// An element that stops being selected must stop contributing, or a
    /// shrinking drag would keep copying text it no longer covers.
    #[test]
    fn dropping_out_of_the_selection_removes_the_capture() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture((0, 0), TextPart::Prose, Some("kept".into()));
        sel.capture((0, 1), TextPart::Prose, Some("dropped".into()));
        sel.capture((0, 1), TextPart::Prose, None);
        assert_eq!(sel.selected_text(), "kept");
    }

    /// Starting a new drag anywhere abandons the old selection outright — there
    /// is one at a time, and a stale capture would ride along into the copy.
    #[test]
    fn a_new_drag_abandons_the_previous_selection() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture((0, 0), TextPart::Prose, Some("old".into()));
        sel.begin(MdKey::Reply(4), p(0.0, 0.0));
        assert!(!sel.is_active());
        assert!(sel.anchors_for(MdKey::Reply(0)).is_none());
    }
}
