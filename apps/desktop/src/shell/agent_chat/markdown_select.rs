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
    IntoElement, KeyDownEvent, LayoutId, Pixels, Point, StyledText, Window, point, px, quad,
};

use super::markdown_state::MdKey;

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

#[derive(Default)]
struct State {
    anchors: Option<Anchors>,
    /// What each text element found selected, keyed by its position in the
    /// document so a copy reassembles in reading order. Rebuilt every paint —
    /// an element that scrolled out of view stops contributing, which is
    /// correct: it is no longer laid out, so what it holds is no longer known.
    captured: BTreeMap<usize, String>,
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
    pub fn selected_text(&self) -> String {
        let state = self.0.borrow();
        state.captured.values().cloned().collect::<Vec<_>>().join("\n\n")
    }

    /// Copy the selection if `ev` is ⌘C over one, reporting whether it did.
    ///
    /// Plain ⌘ only: ⌘⇧C and ⌘⌥C mean other things in other apps and must not
    /// be quietly claimed here. Reports `false` when nothing is selected, which
    /// is what leaves ⌘C in the composer meaning what it always did.
    pub fn copy_on_key(&self, ev: &KeyDownEvent, cx: &mut App) -> bool {
        let ks = &ev.keystroke;
        let plain_cmd = ks.modifiers.platform
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift;
        if ks.key != "c" || !plain_cmd || !self.is_active() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text()));
        true
    }

    fn anchors_for(&self, key: MdKey) -> Option<(Point<Pixels>, Point<Pixels>)> {
        let state = self.0.borrow();
        state.anchors.filter(|a| a.key == key).map(|a| a.ordered())
    }

    fn capture(&self, ord: usize, text: Option<String>) {
        let mut state = self.0.borrow_mut();
        match text {
            Some(t) => {
                state.captured.insert(ord, t);
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
    ord: usize,
    selection: Selection,
    tint: Hsla,
    /// Find matches inside this element's own text. Computed per element, so
    /// nothing has to map an offset in the source markdown onto an offset in
    /// the rendered text.
    matches: Vec<MatchBand>,
}

impl ChatText {
    pub fn new(inner: StyledText, key: MdKey, ord: usize, selection: Selection, tint: Hsla) -> Self {
        Self { inner, key, ord, selection, tint, matches: Vec::new() }
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
    #[test]
    fn a_copy_reassembles_in_document_order() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture(2, Some("third".into()));
        sel.capture(0, Some("first".into()));
        sel.capture(1, Some("second".into()));
        assert_eq!(sel.selected_text(), "first\n\nsecond\n\nthird");
        assert!(sel.is_active());
    }

    /// An element that stops being selected must stop contributing, or a
    /// shrinking drag would keep copying text it no longer covers.
    #[test]
    fn dropping_out_of_the_selection_removes_the_capture() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture(0, Some("kept".into()));
        sel.capture(1, Some("dropped".into()));
        sel.capture(1, None);
        assert_eq!(sel.selected_text(), "kept");
    }

    /// Starting a new drag anywhere abandons the old selection outright — there
    /// is one at a time, and a stale capture would ride along into the copy.
    #[test]
    fn a_new_drag_abandons_the_previous_selection() {
        let sel = Selection::default();
        sel.begin(MdKey::Reply(0), p(0.0, 0.0));
        sel.capture(0, Some("old".into()));
        sel.begin(MdKey::Reply(4), p(0.0, 0.0));
        assert!(!sel.is_active());
        assert!(sel.anchors_for(MdKey::Reply(0)).is_none());
    }
}
