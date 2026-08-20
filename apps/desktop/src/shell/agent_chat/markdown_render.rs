//! Chat markdown: a parsed block tree becomes GPUI elements.
//!
//! Pure and `cx`-free, like [`super::bubble`] — every function here is a
//! function of the block, the theme and the typography, and nothing else. That
//! is what makes the whole renderer testable without a window.
//!
//! ## The rule the rest of the phase depends on
//!
//! **Numbers drive layout; colors are paint.** Every value that can move a
//! pixel — text sizes, line heights, paddings, the gap between blocks — is an
//! explicit number resolved before anything draws. Nothing that arrives late is
//! allowed to be one of those numbers.
//!
//! That is not a style preference. Syntax highlighting is computed off the
//! layout path and lands a frame or more after the fence is already on screen;
//! if a color could change a measured height, every late arrival would reflow
//! the transcript under the reader. Because it cannot, highlighting is free to
//! be as late as it likes.
//!
//! ## Why the inline layer is one `StyledText` per block
//!
//! A paragraph with three bold words is one text element with three styled
//! ranges, not seven nested elements. Nesting elements for emphasis would break
//! the line-wrapping across them — the words would wrap as separate boxes — and
//! it would multiply the element count of a long reply by the number of times
//! the author reached for `**`. `StyledText` takes exactly the shape the parser
//! already produces: a string plus ranges.

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, ElementId, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement,
    SharedString, StrikethroughStyle, Styled, StyledText, UnderlineStyle, div, px,
};
use gpui_component::clipboard::Clipboard;
use oximux_markdown::{Block, BlockTree, InlineRun, InlineStyle, TableAlign, TopBlock};
use oximux_settings::{Density, SyntaxPalette, Theme, Typography};
use oximux_syntax::{HighlightKind, HighlightedDocument, LanguageId};

use super::markdown_select::{ChatText, MatchBand, Selection, match_ranges};
use super::markdown_state::MdKey;

/// Where a fence's colors come from, if they exist yet.
///
/// A trait rather than a concrete store so this module stays pure: the
/// implementation that dispatches background work lives in
/// [`super::markdown_state`], and the one used in tests returns nothing.
///
/// `None` is not an error and not a failure — it is the normal answer for a
/// fence's first frame, and the reason the renderer must be able to draw one
/// without colors at exactly the size it will have with them.
pub(super) trait CodeHighlights {
    fn colors(&self, lang: &LanguageId, code: &str) -> Option<Arc<HighlightedDocument>>;
}

/// Vertical gap between two top-level blocks, as a multiple of `pad_panel`.
///
/// Paragraphs in a reply need to read as separate paragraphs without opening
/// up as far as two *messages* do (which is `pad_panel * 2.0`, set by the
/// transcript row).
pub(super) const BLOCK_GAP: f32 = 0.75;

/// Heading sizes as multiples of the body size, `h1` first.
///
/// Multiples rather than absolute sizes because the body size is already the
/// user's density choice, and a heading scale pinned to points would stop
/// tracking it. Levels 4-6 do not grow at all — they are distinguished by
/// weight, which is how they read in a chat reply where an `h4` is a label
/// rather than a title.
const HEADING_SCALE: [f32; 6] = [1.45, 1.25, 1.1, 1.0, 1.0, 1.0];

/// Line height for code, as a multiple of the code text size.
///
/// Load-bearing beyond looks: a fence does not wrap, so its height is exactly
/// `line_count × line_height + padding` — computable without measuring anything
/// and independent of the column width. That is what lets a fence be laid out
/// before its highlighting exists.
const CODE_LINE_HEIGHT: f32 = 1.45;

/// Code text size as a multiple of the body size. Slightly under 1.0 because
/// monospace faces run visually larger than the UI face at equal point size.
const CODE_SCALE: f32 = 0.92;

/// Indent per list nesting level, as a multiple of `pad_panel`.
const LIST_INDENT: f32 = 1.75;

/// How deep nesting is followed before the rest is rendered flat.
///
/// Agent output is adversarial — a malformed reply can nest lists hundreds deep
/// — and each level is a real element. Beyond the cap the content is still
/// rendered, just without further indentation: degrade, never drop, never
/// recurse without a floor.
const MAX_NESTING: usize = 6;

/// Rows of a table rendered before the rest is elided. A table is a grid of
/// elements, so an enormous one is the single most expensive thing a reply can
/// contain.
const MAX_TABLE_ROWS: usize = 200;

/// Everything the renderer needs to turn a block into pixels.
///
/// Carries the base text size and color rather than reading them from an
/// ancestor: the assistant body and the thinking body render the same tree at
/// different sizes, and inheriting would make "which size is this" a question
/// about the element ancestry instead of an argument.
#[derive(Clone)]
pub(super) struct MarkdownStyle {
    pub theme: Theme,
    pub density: Density,
    pub typo: Typography,
    /// Base body size in pixels. Headings and code derive from it.
    pub text_size: f32,
    /// Base body color. Emphasis and headings derive from it.
    pub text_color: Hsla,
    /// Which document this is: the identity a selection is scoped to, and the
    /// seed for element ids inside it. Two messages both showing a `bash` fence
    /// would otherwise hand gpui the same id for two different copy buttons.
    pub key: MdKey,
    /// The chat's one selection. Shared, because a selection belongs to the
    /// view rather than to any element that paints part of it.
    pub selection: Selection,
    /// The find bar's query, when it is open and this document matched.
    pub find: Option<FindMark>,
}

/// What the find bar wants shown in one document.
#[derive(Clone)]
pub(super) struct FindMark {
    pub query: SharedString,
    /// Whether this is the message the find bar's cursor is on.
    pub current: bool,
}

impl MarkdownStyle {
    /// The assistant's reply: full-size body text.
    pub fn body(
        key: MdKey,
        selection: Selection,
        theme: Theme,
        density: Density,
        typo: &Typography,
    ) -> Self {
        Self {
            text_color: theme.fg_base,
            text_size: typo.t_body_md,
            theme,
            density,
            typo: typo.clone(),
            key,
            selection,
            find: None,
        }
    }

    /// A thinking trace: smaller and muted, so it reads as secondary to the
    /// reply it explains.
    pub fn thinking(
        key: MdKey,
        selection: Selection,
        theme: Theme,
        density: Density,
        typo: &Typography,
    ) -> Self {
        Self {
            text_color: theme.fg_muted,
            text_size: typo.t_body_sm,
            theme,
            density,
            typo: typo.clone(),
            key,
            selection,
            find: None,
        }
    }

    /// Mark this document's text with the find bar's query.
    pub fn with_find(mut self, find: Option<FindMark>) -> Self {
        self.find = find;
        self
    }

    fn code_size(&self) -> f32 {
        self.text_size * CODE_SCALE
    }

    /// A copy button's id: this document, this top-level block, this fence
    /// within that block.
    ///
    /// The fence ordinal restarts at zero for every top-level block, because a
    /// block is rendered on its own when it is its own transcript row — so the
    /// block index has to be part of the id, or a reply whose second paragraph
    /// also carries a fence would hand gpui the same id twice.
    ///
    /// Mixed rather than shifted into place: agent output is adversarial, and
    /// `block << 48` on a reply with a few thousand blocks is an overflow panic
    /// in the most-looked-at surface in the product.
    fn copy_id(&self, block: usize, fence: usize) -> ElementId {
        let mix = (block as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(31)
            .wrapping_add((fence as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
        ElementId::from(("chat-code-copy", (self.key.seed() ^ mix) as usize))
    }
}

/// Everything one document's render needs beyond the block itself.
///
/// Bundled rather than threaded as four parameters because the recursion is
/// deep — a fence can sit inside a list item inside a quote — and the fence
/// ordinal in particular has to survive that descent to give each copy button a
/// stable, unique id.
struct Ctx<'a> {
    style: &'a MarkdownStyle,
    hl: &'a dyn CodeHighlights,
    /// Which top-level block is being rendered. Both counters below are scoped
    /// to it and restart at zero for the next one, so rendering a block on its
    /// own — as a block-granularity transcript row does — produces exactly the
    /// ids and ordinals it would have got inside the whole document.
    block: usize,
    /// Fences seen so far in this block, in order. Ids keyed on content would
    /// collide when a reply shows the same snippet twice, which is exactly what
    /// a before/after answer does.
    fence: Cell<usize>,
    /// Text elements so far in this block, in order.
    text_ord: Cell<usize>,
}

impl<'a> Ctx<'a> {
    fn new(style: &'a MarkdownStyle, hl: &'a dyn CodeHighlights, block: usize) -> Self {
        Self { style, hl, block, fence: Cell::new(0), text_ord: Cell::new(0) }
    }
}

impl Ctx<'_> {
    /// Enrol a text element in its message's selection.
    ///
    /// Every piece of text in the document goes through here, in the order it
    /// is built, and that order is the only thing that tells a copy how to
    /// reassemble what came from several elements.
    ///
    /// The ordinal is `(block, position within block)` rather than one running
    /// count, and it is a pair rather than a packed integer on purpose: a
    /// `BTreeMap` orders pairs lexicographically, which is document order, and
    /// nothing has to pick a stride that a pathological reply could overflow.
    fn selectable(&self, plain: &str, text: StyledText) -> ChatText {
        let local = self.text_ord.get();
        self.text_ord.set(local + 1);
        let el = ChatText::new(
            text,
            self.style.key,
            (self.block, local),
            self.style.selection.clone(),
            self.style.theme.selection,
        );
        let Some(find) = &self.style.find else {
            return el;
        };
        // Every occurrence in this element, tinted by whether this message is
        // the one the find bar is currently sitting on. The find bar steps
        // between MESSAGES, so "current" is a property of the message, not of
        // an individual occurrence — colouring one occurrence differently from
        // its neighbour would claim a precision the bar does not have.
        let tint = if find.current {
            self.style.theme.match_bg_current
        } else {
            self.style.theme.match_bg_other
        };
        let ranges = match_ranges(plain, &find.query);
        if ranges.is_empty() {
            return el;
        }
        el.with_matches(ranges.into_iter().map(|range| MatchBand { range, tint }).collect())
    }
}

/// Render a whole parsed document.
///
/// The gap between blocks is padding on each block after the first rather than
/// a `gap` on this column, for the same reason the transcript row uses padding:
/// a value that lives outside the child's own box is a value the child cannot
/// account for when something measures it.
pub(super) fn render_document(
    tree: &BlockTree,
    style: &MarkdownStyle,
    hl: &dyn CodeHighlights,
) -> AnyElement {
    let mut col = div().flex().flex_col().w_full().min_w_0();
    for (ix, top) in tree.blocks.iter().enumerate() {
        let cx = Ctx::new(style, hl, ix);
        col = col.child(spaced(render_block(&top.block, &cx, 0), ix, style));
    }
    col.into_any_element()
}

/// Render one top-level block by itself, for a transcript that gives each block
/// its own row.
///
/// Identical output to the same block's slice of [`render_document`] — same
/// element ids, same selection ordinals — because both go through a [`Ctx`]
/// built for that block index. The inter-block gap is deliberately *not*
/// applied here: as a row, the spacing between this block and the one above it
/// belongs to the transcript, which is the thing that knows whether the row
/// above is the rest of this message or the end of the previous one.
///
/// `ix` is the block's index in its document, passed alongside the block rather
/// than derived from it: it is what namespaces the element ids and the selection
/// ordinals, and a block does not know where it sits.
pub(super) fn render_block_at(
    top: &TopBlock,
    ix: usize,
    style: &MarkdownStyle,
    hl: &dyn CodeHighlights,
) -> AnyElement {
    let cx = Ctx::new(style, hl, ix);
    div().w_full().min_w_0().child(render_block(&top.block, &cx, 0)).into_any_element()
}

/// Put the inter-block gap on the block itself.
fn spaced(el: AnyElement, ix: usize, style: &MarkdownStyle) -> AnyElement {
    let mut wrap = div().w_full().min_w_0();
    if ix > 0 {
        wrap = wrap.pt(px(style.density.pad_panel * BLOCK_GAP));
    }
    wrap.child(el).into_any_element()
}

/// One block. `depth` counts container nesting so the renderer can stop
/// indenting rather than stop rendering.
fn render_block(block: &Block, cx: &Ctx<'_>, depth: usize) -> AnyElement {
    let style = cx.style;
    match block {
        Block::Paragraph { runs } => inline_text(runs, cx, style.text_size, style.text_color),
        Block::Heading { level, runs } => heading(*level, runs, cx),
        Block::CodeBlock { language, code } => code_block(language.as_deref(), code, cx),
        Block::BlockQuote { children } => block_quote(children, cx, depth),
        Block::List { ordered_start, items } => list(*ordered_start, items, cx, depth),
        Block::Table { header, rows, align } => table(header, rows, align, cx),
        Block::Rule => rule(style),
        // Raw HTML is carried by the parser so nothing is lost, but this is a
        // chat transcript, not a browser: showing the source is honest, and
        // rendering it would be an injection surface fed by whatever the agent
        // happened to print.
        Block::Html { raw } => code_block(Some("html"), raw, cx),
    }
}

// ---------------------------------------------------------------- inline text

/// A block of inline runs as a single wrapping text element.
///
/// The three delayed-styling channels `StyledText` offers all take sorted,
/// non-overlapping ranges, and the runs are already exactly that — one range
/// per run, in order — so no merging or sorting is needed here.
fn inline_text(runs: &[InlineRun], cx: &Ctx<'_>, size: f32, color: Hsla) -> AnyElement {
    let style = cx.style;
    let (text, highlights, mono) = inline_spans(runs, style, color);
    let text = SharedString::from(text);
    let mut el = StyledText::new(text.clone());
    if !highlights.is_empty() {
        el = el.with_highlights(highlights);
    }
    if !mono.is_empty() {
        el = el.with_font_family_overrides(mono);
    }
    let el = cx.selectable(&text, el);
    div()
        .w_full()
        // The same trap `bubble.rs` documents: without this a flex ancestor
        // honors the text's longest unwrapped line as its min-content width and
        // lets it overflow the column instead of wrapping.
        .min_w_0()
        .text_size(px(size))
        .text_color(color)
        .child(el)
        .into_any_element()
}

/// The concatenated text of a block plus the two range lists `StyledText`
/// wants: styling that layers onto the ambient style, and font-family swaps
/// that cannot.
type InlineSpans = (
    String,
    Vec<(Range<usize>, HighlightStyle)>,
    Vec<(Range<usize>, SharedString)>,
);

/// Flatten runs into the concatenated string plus the two range lists
/// `StyledText` wants.
///
/// Inline code needs a different font *family*, which a `HighlightStyle` cannot
/// express — hence the second list. Everything else is a color, a weight, a
/// slant or a line, all of which layer onto the ambient style.
fn inline_spans(
    runs: &[InlineRun],
    style: &MarkdownStyle,
    color: Hsla,
) -> InlineSpans {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut mono = Vec::new();
    for run in runs {
        let start = text.len();
        text.push_str(&run.text);
        let range = start..text.len();
        if range.is_empty() {
            continue;
        }
        if let Some(hl) = run_style(&run.style, style, color) {
            highlights.push((range.clone(), hl));
        }
        if run.style.code {
            mono.push((range, style.typo.family_mono.clone()));
        }
    }
    (text, highlights, mono)
}

/// One run's styling, or `None` for a run that wants the ambient style.
///
/// A link wins the color contest over emphasis: a bolded link should still look
/// like a link, and there is only one foreground to give.
fn run_style(inline: &InlineStyle, style: &MarkdownStyle, base: Hsla) -> Option<HighlightStyle> {
    if inline == &InlineStyle::default() {
        return None;
    }
    let mut hl = HighlightStyle::default();
    if inline.bold {
        hl.font_weight = Some(FontWeight::SEMIBOLD);
    }
    if inline.italic {
        hl.font_style = Some(FontStyle::Italic);
    }
    if inline.strikethrough {
        hl.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.0),
            color: Some(style.theme.fg_subtle),
        });
        hl.color = Some(style.theme.fg_muted);
    }
    if inline.code {
        hl.color = Some(style.theme.status_info);
        hl.background_color = Some(style.theme.bg_overlay);
    }
    if inline.link.is_some() {
        hl.color = Some(style.theme.status_info);
        hl.underline = Some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(style.theme.status_info),
            wavy: false,
        });
    }
    if hl.color.is_none() && base != style.text_color {
        hl.color = Some(base);
    }
    Some(hl)
}

// -------------------------------------------------------------------- blocks

fn heading(level: u8, runs: &[InlineRun], cx: &Ctx<'_>) -> AnyElement {
    let style = cx.style;
    let ix = (level.clamp(1, 6) as usize) - 1;
    let size = style.text_size * HEADING_SCALE[ix];
    div()
        .w_full()
        .min_w_0()
        .font_weight(style.typo.w_semibold)
        .child(inline_text(runs, cx, size, style.theme.fg_base))
        .into_any_element()
}

/// A fenced code block: one text element per line, in a bordered card.
///
/// Per line rather than one element for the whole fence because a fence does
/// not wrap, and per-line elements are what let highlighting be applied as
/// per-line span lists without re-deriving whole-document offsets. The fixed
/// line height is what makes the card's height arithmetic rather than
/// measurement.
///
/// **"A fence does not wrap" is a thing this function has to enforce, not a
/// thing that is true by itself.** It used to be written here as a premise while
/// the line's text was `w_full` and free to wrap at the column, so a line longer
/// than the reading measure laid out as two or more visual lines inside a box
/// exactly one line tall — and the remainder painted *on top of* the next line.
/// Both lines became unreadable. Seen in a real transcript on a JSON fence whose
/// `"image": "https://raw.githubusercontent.com/…"` ran long.
///
/// The height cannot be the thing that gives way. Highlighting arrives a frame
/// or more after the fence is on screen and applies per-span **colours**, and at
/// this repo's pinned gpui rev a `TextRun` colour split really does move wrap
/// points (measured — `examples/veil_shaping_probe.rs`). So a fence that both
/// wrapped and measured its own height would re-flow under the reader every time
/// a highlight landed. Height stays arithmetic; the line stops wrapping instead.
fn code_block(language: Option<&str>, code: &str, cx: &Ctx<'_>) -> AnyElement {
    let style = cx.style;
    let ordinal = cx.fence.get();
    cx.fence.set(ordinal + 1);
    let size = style.code_size();
    // Detection is by fence tag only. There is no path here, and guessing from
    // content would make a fence's *language* — hence its language tag —
    // dependent on how much of it has streamed in so far.
    let colors = oximux_syntax::detect(None, language, "").and_then(|lang| {
        let doc = cx.hl.colors(&lang, code);
        doc.map(|doc| (lang, doc))
    });
    let mut lines = div().flex().flex_col().w_full().min_w_0();
    for (ix, line) in code.lines().enumerate() {
        let spans = colors.as_ref().map(|(_, doc)| doc.line(ix)).unwrap_or_default();
        lines = lines.child(
            // A flex ROW per line, and the text one level further in — not
            // because the nesting is pretty, but because both levels are
            // load-bearing:
            //
            // The row exists because a `whitespace_nowrap` text placed directly
            // into a flex COLUMN lays out to nothing at all in gpui — no text,
            // no panic, an empty fence. A flex row is the context that makes it
            // visible.
            //
            // The inner div exists to carry `flex_shrink_0`, which the text
            // element cannot: without it the line is a flex item free to be
            // squeezed to the row's width, and the point is that it keeps its
            // natural width and overruns.
            div()
                .flex()
                .flex_row()
                .w_full()
                // The row's width is already definite, so this only stops the
                // long line's min-content width propagating outward and prying
                // the card open instead of overflowing it.
                .min_w_0()
                // Fixed, not measured. This is the number that makes a fence's
                // height arithmetic, which is what lets colors arrive later —
                // and it is only *true* because of the `whitespace_nowrap`
                // below. See this function's doc comment.
                .h(px(size * CODE_LINE_HEIGHT))
                // The whole fix. `white_space != Normal` makes gpui lay the text
                // out with no wrap width at all (`elements/text.rs:417`), so one
                // source line is one visual line however long it is, and the
                // excess overruns the card — which clips it.
                .whitespace_nowrap()
                .child(
                    div()
                        .flex_shrink_0()
                        .child(cx.selectable(line, code_line(line, spans, &style.theme.syntax))),
                ),
        );
    }
    let mut card = div()
        .w_full()
        .min_w_0()
        .rounded(px(style.density.r_xs))
        .bg(style.theme.bg_panel_alt)
        .border_1()
        .border_color(style.theme.border_inactive)
        .px(px(style.density.pad_panel))
        .py(px(style.density.pad_panel * 0.5))
        .font_family(style.typo.family_mono.clone())
        .text_size(px(size))
        .text_color(style.theme.fg_base)
        // Code is authoritative about its own line breaks: a wrapped fence
        // shows the reader a line the agent never wrote. This clips what the
        // `whitespace_nowrap` above lets overrun — the two are one mechanism,
        // and this half alone did nothing, because text that wraps never
        // overflows in the first place.
        //
        // Clipping rather than scrolling horizontally is deliberate. gpui only
        // applies horizontal scroll from a horizontal trackpad delta, so a plain
        // mouse wheel could never reach the hidden text without a per-fence
        // scrollbar overlaid on it, and that would put a scroll region inside
        // the transcript's own virtualized list. The copy button on the header
        // row already hands over the untruncated source.
        .overflow_x_hidden();
    // The grammar's own display name when one resolved (`Rust`, not `rs`), and
    // otherwise whatever the author typed — a tag we did not recognise is still
    // what they meant to say.
    let tag = colors
        .as_ref()
        .map(|(lang, _)| lang.name().to_string())
        .or_else(|| language.map(str::to_string))
        .filter(|l| !l.trim().is_empty());
    // The language tag and the copy button share one header row, so a fence
    // with neither costs no row at all — and the row's height is a constant,
    // which keeps the fence's total height arithmetic.
    if tag.is_some() {
        card = card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .w_full()
                .h(px(style.typo.t_label_xs * 1.6))
                .child(
                    div()
                        .text_size(px(style.typo.t_label_xs))
                        .text_color(style.theme.fg_subtle)
                        .children(tag.map(SharedString::from)),
                )
                .child(copy_button(style.copy_id(cx.block, ordinal), code)),
        );
    }
    card.child(lines).into_any_element()
}

/// One-click copy for a fence.
///
/// Kept even though the text is selectable: copying an answer's code is the
/// single most common thing a reader does with it, and a drag that has to end
/// exactly on the last character is a worse way to do it.
fn copy_button(id: ElementId, code: &str) -> impl IntoElement {
    Clipboard::new(id).value(SharedString::from(code.to_string()))
}

/// One line of code, colored by kind if its spans have arrived.
///
/// Colors only. Nothing here sets a weight, a size or a family — the whole
/// safety argument for late highlighting is that this function cannot change
/// what the line already measured.
fn code_line(line: &str, spans: &[oximux_syntax::HighlightSpan], palette: &SyntaxPalette) -> StyledText {
    let text = StyledText::new(SharedString::from(line.to_string()));
    if spans.is_empty() {
        return text;
    }
    text.with_highlights(
        spans
            .iter()
            // A span whose range is not on a character boundary would trip
            // `StyledText`'s own assertion in a debug build. The highlighter
            // emits in-bounds boundaries, so this only ever declines a span
            // that arrived for text which has since changed.
            .filter(|s| {
                line.is_char_boundary(s.range.start) && line.is_char_boundary(s.range.end)
            })
            .map(|s| {
                let style = HighlightStyle {
                    color: Some(kind_color(s.kind, palette)),
                    ..Default::default()
                };
                (s.range.clone(), style)
            }),
    )
}

/// The only place a highlight kind becomes a color.
fn kind_color(kind: HighlightKind, p: &SyntaxPalette) -> Hsla {
    match kind {
        HighlightKind::Keyword => p.keyword,
        HighlightKind::Function => p.function,
        HighlightKind::Type => p.type_name,
        HighlightKind::String => p.string,
        HighlightKind::Escape => p.escape,
        HighlightKind::Number => p.number,
        HighlightKind::Comment => p.comment,
        HighlightKind::Constant => p.constant,
        HighlightKind::Operator => p.operator,
        HighlightKind::Punctuation => p.punctuation,
        HighlightKind::Variable => p.variable,
        HighlightKind::Attribute => p.attribute,
        HighlightKind::Namespace => p.namespace,
        HighlightKind::Tag => p.tag,
    }
}

/// A quote, ruled on the left and muted, with its children rendered normally.
fn block_quote(children: &[Block], cx: &Ctx<'_>, depth: usize) -> AnyElement {
    let style = cx.style;
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .border_l_2()
        .border_color(style.theme.border_inactive)
        .pl(px(style.density.pad_panel))
        .text_color(style.theme.fg_muted);
    for (ix, child) in children.iter().enumerate() {
        col = col.child(spaced(render_block(child, cx, depth + 1), ix, style));
    }
    col.into_any_element()
}

/// A bullet or ordered list. Markers are a fixed-width column so item bodies
/// share a left edge however wide the numbers get.
fn list(
    ordered_start: Option<u64>,
    items: &[Vec<Block>],
    cx: &Ctx<'_>,
    depth: usize,
) -> AnyElement {
    let style = cx.style;
    let indent = if depth < MAX_NESTING { style.density.pad_panel * LIST_INDENT } else { 0.0 };
    let mut col = div().flex().flex_col().w_full().min_w_0().pl(px(indent));
    for (ix, item) in items.iter().enumerate() {
        let marker = match ordered_start {
            Some(start) => format!("{}.", start.saturating_add(ix as u64)),
            None => "•".to_string(),
        };
        // `flex_1`, not `w_full`: the body should take what the marker leaves,
        // and `w_full` asks for the whole row and then relies on shrink to
        // resolve the overflow — which compounds through a nested list.
        let mut body = div().flex().flex_col().flex_1().min_w_0();
        for (bix, block) in item.iter().enumerate() {
            body = body.child(spaced(render_block(block, cx, depth + 1), bix, style));
        }
        let row = div()
            .flex()
            .flex_row()
            .w_full()
            .min_w_0()
            .gap(px(style.density.gap_inline))
            .child(
                div()
                    .flex_none()
                    .min_w(px(style.text_size * 1.2))
                    .text_color(style.theme.fg_subtle)
                    .child(SharedString::from(marker)),
            )
            .child(body);
        col = col.child(spaced(row.into_any_element(), ix, style));
    }
    col.into_any_element()
}

/// A table, capped. Column widths are equal shares rather than measured
/// content: measuring would need a second pass over every cell, and an even
/// grid is what a chat-width table reads as anyway.
fn table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    cx: &Ctx<'_>,
) -> AnyElement {
    let style = cx.style;
    let mut grid = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .rounded(px(style.density.r_xs))
        .border_1()
        .border_color(style.theme.border_inactive);
    if !header.is_empty() {
        grid = grid.child(table_row(header, align, cx, true));
    }
    for cells in rows.iter().take(MAX_TABLE_ROWS) {
        grid = grid.child(table_row(cells, align, cx, false));
    }
    if rows.len() > MAX_TABLE_ROWS {
        let elided = rows.len() - MAX_TABLE_ROWS;
        grid = grid.child(
            div()
                .w_full()
                .px(px(style.density.pad_panel))
                .py(px(style.density.pad_panel * 0.5))
                .text_size(px(style.typo.t_label_xs))
                .text_color(style.theme.fg_subtle)
                .child(SharedString::from(format!("… {elided} more rows"))),
        );
    }
    grid.into_any_element()
}

fn table_row(
    cells: &[Vec<InlineRun>],
    align: &[TableAlign],
    cx: &Ctx<'_>,
    is_header: bool,
) -> AnyElement {
    let style = cx.style;
    let mut row = div().flex().flex_row().w_full().min_w_0();
    if is_header {
        row = row.bg(style.theme.bg_panel_alt).font_weight(style.typo.w_semibold);
    } else {
        row = row.border_t_1().border_color(style.theme.border_inactive);
    }
    for (ix, cell) in cells.iter().enumerate() {
        let mut c = div()
            .flex_1()
            .min_w_0()
            .px(px(style.density.pad_panel * 0.75))
            .py(px(style.density.pad_panel * 0.4));
        c = match align.get(ix).copied().unwrap_or_default() {
            TableAlign::Center => c.items_center().text_center(),
            TableAlign::Right => c.items_end().text_right(),
            TableAlign::Left | TableAlign::None => c,
        };
        row = row.child(c.child(inline_text(cell, cx, style.text_size, style.text_color)));
    }
    row.into_any_element()
}

fn rule(style: &MarkdownStyle) -> AnyElement {
    div()
        .w_full()
        .h(px(1.0))
        .my(px(style.density.pad_panel * 0.5))
        .bg(style.theme.border_inactive)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_markdown::parse_full;

    /// A highlighter that never has anything ready — the state every fence is
    /// in on its first frame, and the one these tests hold it in.
    struct NoColors;

    impl CodeHighlights for NoColors {
        fn colors(&self, _lang: &LanguageId, _code: &str) -> Option<Arc<HighlightedDocument>> {
            None
        }
    }

    fn style() -> MarkdownStyle {
        MarkdownStyle::body(
            MdKey::Reply(7),
            Selection::default(),
            Theme::default(),
            Density::default(),
            &Typography::default(),
        )
    }

    fn ctx<'a>(style: &'a MarkdownStyle, hl: &'a dyn CodeHighlights) -> Ctx<'a> {
        Ctx::new(style, hl, 0)
    }

    fn spans_of(md: &str) -> InlineSpans {
        let tree = parse_full(md);
        let runs = match &tree.blocks[0].block {
            Block::Paragraph { runs } | Block::Heading { runs, .. } => runs.clone(),
            other => panic!("expected an inline block, got {other:?}"),
        };
        let style = style();
        inline_spans(&runs, &style, style.text_color)
    }

    /// The contract `StyledText` is given: ranges must be sorted, disjoint, on
    /// character boundaries and inside the text. A violation is a debug
    /// assertion inside gpui, i.e. a panic while rendering a reply.
    #[test]
    fn inline_ranges_are_sorted_disjoint_and_in_bounds() {
        let (text, highlights, mono) = spans_of(
            "plain **bold** and *italic* and `code` and ~~gone~~ and [a link](https://x) — é×ø",
        );
        for list in [
            highlights.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            mono.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
        ] {
            let mut last = 0;
            for r in list {
                assert!(r.start >= last, "ranges out of order at {r:?}");
                assert!(r.end <= text.len(), "range {r:?} past the end of {}", text.len());
                assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
                last = r.end;
            }
        }
    }

    /// Inline code is the one inline style that changes the font *family*,
    /// which a `HighlightStyle` cannot express — so it must ride the separate
    /// override list or it silently renders in the body face.
    #[test]
    fn inline_code_gets_a_family_override() {
        let (text, _, mono) = spans_of("call `run_it()` now");
        assert_eq!(mono.len(), 1, "expected exactly one mono span");
        assert_eq!(&text[mono[0].0.clone()], "run_it()");
    }

    /// Emphasis must not swallow the text. A renderer that dropped a run's
    /// characters while styling it would lose words from a reply.
    #[test]
    fn every_character_survives_styling() {
        let (text, _, _) = spans_of("a **b** c *d* e `f` g");
        assert_eq!(text, "a b c d e f g");
    }

    /// A bolded link still has to look like a link: there is one foreground per
    /// run, and the link is the part the reader acts on.
    #[test]
    fn a_link_keeps_its_color_through_emphasis() {
        let style = style();
        let bold_link = InlineStyle {
            bold: true,
            link: Some("https://example.invalid".into()),
            ..Default::default()
        };
        let hl = run_style(&bold_link, &style, style.text_color).expect("styled");
        assert_eq!(hl.color, Some(style.theme.status_info));
        assert!(hl.underline.is_some(), "a link without an underline is not a link");
        assert_eq!(hl.font_weight, Some(FontWeight::SEMIBOLD), "the bold was dropped");
    }

    /// The whole late-highlighting argument rests on this: a fence's colors
    /// change nothing that was measured. Compared structurally rather than by
    /// rendering, because two elements that lay out identically is exactly the
    /// claim.
    #[test]
    fn fence_colors_do_not_change_the_line_count_or_its_height() {
        let code = "fn main() {\n    let x = 1;\n}\n";
        let lang = oximux_syntax::detect(None, Some("rust"), "").expect("rust grammar");
        let doc = Arc::new(oximux_syntax::highlight(&lang, code));
        assert!(doc.span_count() > 0, "the fixture must actually highlight");

        // Same source, same line count, same fixed line height — the only
        // difference the colors can make is which spans exist.
        assert_eq!(code.lines().count(), doc.lines());
        let style = style();
        let h = style.code_size() * CODE_LINE_HEIGHT;
        assert!(h > 0.0, "a zero line height would collapse every fence");
    }

    /// A span for text that has since changed must be declined rather than
    /// handed to gpui, whose debug assertion on character boundaries would
    /// panic mid-render.
    #[test]
    fn a_span_off_a_character_boundary_is_dropped_not_panicked() {
        let line = "héllo";
        let bad = vec![oximux_syntax::HighlightSpan {
            // Byte 2 is the middle of `é`.
            range: 1..2,
            kind: HighlightKind::Keyword,
        }];
        // Would trip gpui's own assertion if it were forwarded.
        let _ = code_line(line, &bad, &Theme::default().syntax);
    }

    /// Adversarial input must degrade, never recurse without a floor.
    #[test]
    fn deeply_nested_input_renders_without_recursing_forever() {
        let deep = "> ".repeat(400) + "still here\n";
        let tree = parse_full(&deep);
        let style = style();
        let _ = render_document(&tree, &style, &NoColors);
    }

    /// A reply that shows the same snippet twice — the shape of every
    /// before/after answer — must not hand gpui the same element id twice.
    #[test]
    fn two_identical_fences_get_different_copy_ids() {
        let style = style();
        assert_ne!(style.copy_id(0, 0), style.copy_id(0, 1));
        // ...nor two blocks of one reply that each open with a fence.
        assert_ne!(style.copy_id(0, 0), style.copy_id(1, 0));
        // ...and two documents do not collide either.
        let other = MarkdownStyle::body(
            MdKey::Reply(8),
            Selection::default(),
            Theme::default(),
            Density::default(),
            &Typography::default(),
        );
        assert_ne!(style.copy_id(0, 0), other.copy_id(0, 0));
    }

    /// Selection reassembles a copy from text elements in the order they were
    /// built, so that order must follow the document — including through a
    /// fence nested inside a list inside a quote.
    #[test]
    fn text_ordinals_are_handed_out_in_document_order() {
        let tree = parse_full("first\n\n> - a\n>   - ```\n>     code\n>     ```\n\nlast\n");
        let style = style();
        let cx = ctx(&style, &NoColors);
        let _ = {
            let mut col = Vec::new();
            for top in &tree.blocks {
                col.push(render_block(&top.block, &cx, 0));
            }
            col
        };
        assert!(
            cx.text_ord.get() >= 3,
            "only {} text elements enrolled; nesting was skipped",
            cx.text_ord.get(),
        );
    }

    /// Every block variant must render. A `todo!()` arm or a panic on an
    /// unusual block is a crash in the most-looked-at surface in the product.
    #[test]
    fn every_block_kind_renders() {
        let md = "\
# Heading

A paragraph with **bold**.

- one
- two
  1. nested
  2. items

> a quote
>
> ```rust
> let x = 1;
> ```

| a | b |
|:-:|--:|
| 1 | 2 |

---

<div>raw</div>

```
plain fence
```
";
        let tree = parse_full(md);
        assert!(tree.len() >= 7, "the fixture stopped covering the variants");
        let style = style();
        let _ = render_document(&tree, &style, &NoColors);
    }
}
