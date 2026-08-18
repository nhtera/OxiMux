//! Full and incremental parsing.
//!
//! The incremental half is the reason this file is not three functions, so its
//! rule is stated once, here, rather than discovered from the code:
//!
//! **Reparse from the start of the second-to-last top-level block, snapped back
//! to a line start.** Not the last block — the last two.
//!
//! Why two: a block's boundary with its *predecessor* can still move when text
//! is appended. A trailing paragraph `3` becomes `3.` and fuses into the loose
//! list above it; a paragraph gains a `---` underneath and turns into a setext
//! heading. Reparsing only the final block cannot see either, so it produces a
//! tree that disagrees with a full parse — and it does so silently, on specific
//! streaming interleavings, which is the kind of bug that surfaces weeks later
//! as "list items sometimes split in half". Merges cannot cascade further back
//! than one block, because by then the separation is already settled.
//!
//! Why snapped to a line start: indented code and fenced-block indentation are
//! both line-relative, and starting a parse mid-line changes what the leading
//! whitespace means.
//!
//! `tests/parity.rs` is what holds this honest — it streams a corpus one byte at
//! a time and compares against a full parse at every step. If a case there
//! cannot be made to agree, the fix is to widen the window or add the construct
//! to the always-full set. It is never to weaken the test.

use std::ops::Range;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};

use crate::{Block, BlockTree, InlineRun, InlineStyle, TableAlign, TopBlock};

/// GFM extensions this crate understands.
///
/// Tables and strikethrough, and deliberately not task lists. Enabling task
/// lists would make `pulldown-cmark` emit a `TaskListMarker` event that has no
/// home in [`Block`], so representing it would mean either inventing a field or
/// silently dropping the checkbox. Left off, `- [x] done` parses as a list item
/// whose text begins `[x] done` — literal, lossless, and exactly what the agent
/// wrote. A renderer that wants real checkboxes can ask for the extension when
/// there is somewhere to put the answer.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

/// Parse a whole document.
pub fn parse_full(text: &str) -> BlockTree {
    let events: Vec<(Event<'_>, Range<usize>)> =
        Parser::new_ext(text, options()).into_offset_iter().collect();
    let mut cursor = Cursor { events: &events, at: 0 };
    let mut blocks = Vec::new();
    while let Some((block, range)) = cursor.next_block() {
        blocks.push(TopBlock { range, block });
    }
    BlockTree { blocks }
}

/// A parser that keeps its source and reparses only what an append can reach.
///
/// Feed it with [`Self::set_text`] when you hold the whole string (the common
/// case for a streaming fold, which owns the accumulated text) or
/// [`Self::append`] when you hold only the delta. `set_text` recognises an
/// append and takes the cheap path; anything else — an edit, a rewind, a
/// different message — resets.
#[derive(Debug, Default)]
pub struct IncrementalParser {
    text: String,
    tree: BlockTree,
    /// This source contains a link-reference definition, so every parse from
    /// here on must be a full one. See [`Self::scan_for_link_definition`].
    full_only: bool,
    last_parse_bytes: usize,
    stable_prefix_blocks: usize,
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tree(&self) -> &BlockTree {
        &self.tree
    }

    /// Bytes the most recent parse actually read.
    ///
    /// Exposed for tests rather than for callers: it is the only way to assert
    /// that streaming stays O(delta), and a cost claim nothing checks is a cost
    /// claim that quietly stops being true.
    pub fn last_parse_bytes(&self) -> usize {
        self.last_parse_bytes
    }

    /// Blocks the most recent parse kept without re-reading.
    pub fn stable_prefix_blocks(&self) -> usize {
        self.stable_prefix_blocks
    }

    /// Whether this source has been forced onto full reparses.
    pub fn is_full_only(&self) -> bool {
        self.full_only
    }

    /// Replace the source, taking the incremental path when the new text merely
    /// extends the old one.
    pub fn set_text(&mut self, text: &str) {
        if text.len() > self.text.len() && text.as_bytes().starts_with(self.text.as_bytes()) {
            let delta_start = self.text.len();
            self.append(&text[delta_start..]);
        } else if text != self.text {
            self.reset(text);
        }
        // Identical text: keep the tree AND the cost counters, so a caller that
        // re-sets the same string does not look like it did work.
    }

    /// Reparse from scratch.
    pub fn reset(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.full_only = self.scan_for_link_definition(0);
        self.tree = parse_full(&self.text);
        self.last_parse_bytes = self.text.len();
        self.stable_prefix_blocks = 0;
    }

    /// Extend the source and reparse only the window the append can reach.
    pub fn append(&mut self, delta: &str) {
        if delta.is_empty() {
            self.last_parse_bytes = 0;
            self.stable_prefix_blocks = self.tree.blocks.len();
            return;
        }

        // Where a link-reference definition could newly appear. From the last
        // newline of the PREVIOUS text, not from the delta: a definition line
        // can be begun by one append and completed by the next, and scanning
        // only the delta would see `url` with no `[label]:` in front of it and
        // conclude there was nothing there.
        let rescan_from = self.text.rfind('\n').map(|i| i + 1).unwrap_or(0);

        self.text.push_str(delta);

        if !self.full_only && self.scan_for_link_definition(rescan_from) {
            self.full_only = true;
        }

        if self.full_only {
            self.tree = parse_full(&self.text);
            self.last_parse_bytes = self.text.len();
            self.stable_prefix_blocks = 0;
            return;
        }

        // Fewer than two blocks means there is no stable prefix to keep.
        let Some(boundary_ix) = self.tree.blocks.len().checked_sub(2) else {
            self.tree = parse_full(&self.text);
            self.last_parse_bytes = self.text.len();
            self.stable_prefix_blocks = 0;
            return;
        };

        let start = snap_to_line_start(&self.text, self.tree.blocks[boundary_ix].range.start);
        let tail = parse_full(&self.text[start..]);

        self.tree.blocks.truncate(boundary_ix);
        self.tree.blocks.extend(tail.blocks.into_iter().map(|b| TopBlock {
            range: (b.range.start + start)..(b.range.end + start),
            block: b.block,
        }));

        self.last_parse_bytes = self.text.len() - start;
        self.stable_prefix_blocks = boundary_ix;
    }

    /// The tree to draw *right now*, with the streaming block's hanging inline
    /// markers closed.
    ///
    /// `None` when nothing needs repair, which is the common case — the caller
    /// then draws [`Self::tree`] and no work has been done. See [`crate::mend`]
    /// for why this is display-only.
    pub fn display_tree(&self) -> Option<BlockTree> {
        let last = self.tree.blocks.last()?;
        // A fenced block's contents are literal by definition; "repairing" an
        // unbalanced `**` inside one would corrupt the code being shown.
        if matches!(last.block, Block::CodeBlock { .. } | Block::Html { .. }) {
            return None;
        }
        let src = self.text.get(last.range.clone())?;
        let mended = crate::mend::close_hanging(src)?;

        let reparsed = parse_full(&mended);
        if reparsed.blocks.is_empty() {
            return None;
        }
        let mut out = self.tree.clone();
        let range = last.range.clone();
        out.blocks.pop();
        // The mended source can only ever produce blocks standing in for the one
        // that was removed, so they all inherit its range — a display tree is
        // never the anchor for a later incremental parse, and giving them
        // ranges into a string that does not exist would be worse.
        out.blocks.extend(
            reparsed.blocks.into_iter().map(|b| TopBlock { range: range.clone(), block: b.block }),
        );
        Some(out)
    }

    /// Is there a link-reference definition at or after `from`?
    ///
    /// Lexical rather than event-driven because `pulldown-cmark` consumes
    /// definitions silently — they produce no event to observe. A line of up to
    /// three spaces of indent, then `[label]:`, is the whole shape.
    ///
    /// Any hit is permanent ([`Self::full_only`]) and that is the point: a
    /// definition has **non-local** effect. `[a]: /x` at the bottom of a
    /// document changes how `[a]` renders at the top, so no window anchored near
    /// the end of the text can be trusted once one exists. Correctness over
    /// cost; documents that contain one are rare and usually short.
    fn scan_for_link_definition(&self, from: usize) -> bool {
        self.text[from..].lines().any(is_link_definition_line)
    }
}

fn is_link_definition_line(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false; // four spaces is indented code, not a definition
    }
    let rest = &line[indent..];
    let Some(rest) = rest.strip_prefix('[') else {
        return false;
    };
    // A label may not contain an unescaped `]`, so the first one ends it.
    let Some(close) = rest.find(']') else {
        return false;
    };
    rest[close + 1..].starts_with(':')
}

/// Walk back to the start of the line containing `at`.
fn snap_to_line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// A cursor over the flat event stream, which the block builders share.
struct Cursor<'a, 'b> {
    events: &'b [(Event<'a>, Range<usize>)],
    at: usize,
}

impl<'a> Cursor<'a, '_> {
    fn peek(&self) -> Option<&(Event<'a>, Range<usize>)> {
        self.events.get(self.at)
    }

    /// The next top-level block, or `None` at the end of the stream.
    fn next_block(&mut self) -> Option<(Block, Range<usize>)> {
        loop {
            let (event, range) = self.peek()?.clone();
            match event {
                Event::Start(tag) => {
                    self.at += 1;
                    let block = self.finish_container(tag);
                    return Some((block, range));
                }
                Event::Rule => {
                    self.at += 1;
                    return Some((Block::Rule, range));
                }
                // A stray End belongs to a container the caller is already
                // unwinding; hand control back rather than consuming it.
                Event::End(_) => return None,
                // Loose inline events at top level (blank-line artifacts) carry
                // no block; skip rather than inventing a paragraph around them.
                _ => {
                    self.at += 1;
                }
            }
        }
    }

    /// Build the block for a container whose `Start` has just been consumed,
    /// leaving the cursor past its `End`.
    fn finish_container(&mut self, tag: Tag<'a>) -> Block {
        match tag {
            Tag::Paragraph => {
                let runs = self.inline_runs();
                self.expect_end();
                Block::Paragraph { runs }
            }
            Tag::Heading { level, .. } => {
                let runs = self.inline_runs();
                self.expect_end();
                Block::Heading { level: heading_level(level), runs }
            }
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        // The info string is "lang extra args"; only the first
                        // word names a language, which is what a highlighter
                        // can act on.
                        info.split_whitespace().next().map(str::to_owned).filter(|s| !s.is_empty())
                    }
                    CodeBlockKind::Indented => None,
                };
                let mut code = String::new();
                while let Some((event, _)) = self.peek() {
                    match event {
                        Event::End(TagEnd::CodeBlock) => break,
                        Event::Text(t) | Event::Code(t) => {
                            code.push_str(t);
                            self.at += 1;
                        }
                        _ => {
                            self.at += 1;
                        }
                    }
                }
                self.expect_end();
                Block::CodeBlock { language, code }
            }
            Tag::BlockQuote(_) => {
                let mut children = Vec::new();
                while let Some((block, _)) = self.next_block() {
                    children.push(block);
                }
                self.expect_end();
                Block::BlockQuote { children }
            }
            Tag::List(ordered_start) => {
                let mut items = Vec::new();
                while let Some((event, _)) = self.peek() {
                    match event {
                        Event::Start(Tag::Item) => {
                            self.at += 1;
                            let mut blocks = Vec::new();
                            while let Some((block, _)) = self.next_block() {
                                blocks.push(block);
                            }
                            self.expect_end();
                            items.push(blocks);
                        }
                        Event::End(TagEnd::List(_)) => break,
                        _ => {
                            self.at += 1;
                        }
                    }
                }
                self.expect_end();
                Block::List { ordered_start, items }
            }
            Tag::Table(aligns) => self.finish_table(aligns),
            Tag::HtmlBlock => {
                let mut raw = String::new();
                while let Some((event, _)) = self.peek() {
                    match event {
                        Event::End(TagEnd::HtmlBlock) => break,
                        Event::Html(t) | Event::Text(t) => {
                            raw.push_str(t);
                            self.at += 1;
                        }
                        _ => {
                            self.at += 1;
                        }
                    }
                }
                self.expect_end();
                Block::Html { raw }
            }
            // Anything else that opens a container (footnote definitions, a
            // lone Item outside a list) is rendered as its inline content
            // rather than dropped — losing text is the worse failure.
            _ => {
                let runs = self.inline_runs();
                self.expect_end();
                Block::Paragraph { runs }
            }
        }
    }

    fn finish_table(&mut self, aligns: Vec<Alignment>) -> Block {
        let align = aligns.into_iter().map(table_align).collect();
        let mut header = Vec::new();
        let mut rows = Vec::new();

        while let Some((event, _)) = self.peek() {
            match event {
                Event::Start(Tag::TableHead) => {
                    self.at += 1;
                    header = self.table_cells();
                    self.expect_end();
                }
                Event::Start(Tag::TableRow) => {
                    self.at += 1;
                    rows.push(self.table_cells());
                    self.expect_end();
                }
                Event::End(TagEnd::Table) => break,
                _ => {
                    self.at += 1;
                }
            }
        }
        self.expect_end();
        Block::Table { header, rows, align }
    }

    fn table_cells(&mut self) -> Vec<Vec<InlineRun>> {
        let mut cells = Vec::new();
        while let Some((event, _)) = self.peek() {
            match event {
                Event::Start(Tag::TableCell) => {
                    self.at += 1;
                    cells.push(self.inline_runs());
                    self.expect_end();
                }
                Event::End(TagEnd::TableHead | TagEnd::TableRow) => break,
                _ => {
                    self.at += 1;
                }
            }
        }
        cells
    }

    /// Consume the `End` that closes the container being built.
    ///
    /// Tolerant of a missing one: a truncated source — which is every source
    /// this crate sees mid-stream — legitimately ends inside a container, and
    /// panicking on the common case would be an odd way to handle streaming.
    fn expect_end(&mut self) {
        if matches!(self.peek(), Some((Event::End(_), _))) {
            self.at += 1;
        }
    }

    /// Flatten inline events into styled runs until the container closes.
    fn inline_runs(&mut self) -> Vec<InlineRun> {
        let mut runs: Vec<InlineRun> = Vec::new();
        let mut style = InlineStyle::default();
        // Emphasis nests, so each closer has to restore what was there before
        // rather than clearing the flag — `**a *b* c**` must leave `c` bold.
        let mut stack: Vec<InlineStyle> = Vec::new();

        while let Some((event, _)) = self.peek().cloned() {
            match event {
                Event::End(_) => break,
                Event::Start(tag) => {
                    self.at += 1;
                    stack.push(style.clone());
                    match tag {
                        Tag::Strong => style.bold = true,
                        Tag::Emphasis => style.italic = true,
                        Tag::Strikethrough => style.strikethrough = true,
                        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                            style.link = Some(dest_url.to_string());
                        }
                        // A container opening inside inline context (a nested
                        // paragraph in a tight list item) is transparent here;
                        // its content flows into the same run sequence.
                        _ => {}
                    }
                }
                Event::Text(t) => {
                    self.at += 1;
                    push_run(&mut runs, &t, &style);
                }
                Event::Code(t) => {
                    self.at += 1;
                    let mut code_style = style.clone();
                    code_style.code = true;
                    push_run(&mut runs, &t, &code_style);
                }
                // Raw inline HTML is text. A chat transcript is not a browser,
                // and showing the tag is honest where interpreting it would be
                // both surprising and an injection surface.
                Event::InlineHtml(t) | Event::Html(t) => {
                    self.at += 1;
                    push_run(&mut runs, &t, &style);
                }
                // A soft break is a wrap opportunity, not a line break — the
                // renderer re-wraps to its own measure, so collapsing it to a
                // space is what keeps a hard-wrapped source from rendering
                // ragged.
                Event::SoftBreak => {
                    self.at += 1;
                    push_run(&mut runs, " ", &style);
                }
                Event::HardBreak => {
                    self.at += 1;
                    push_run(&mut runs, "\n", &style);
                }
                Event::FootnoteReference(label) => {
                    self.at += 1;
                    push_run(&mut runs, &format!("[^{label}]"), &style);
                }
                Event::Rule | Event::TaskListMarker(_) => {
                    self.at += 1;
                }
                Event::InlineMath(t) | Event::DisplayMath(t) => {
                    self.at += 1;
                    push_run(&mut runs, &t, &style);
                }
            }

            // Restore the style a closing tag popped. Done after the match so a
            // `break` on `End` leaves the cursor on it for the caller.
            if let Some((Event::End(end), _)) = self.peek()
                && closes_inline(end)
                && let Some(prev) = stack.pop()
            {
                style = prev;
                self.at += 1;
            }
        }
        runs
    }
}

/// Does this `End` close an inline container rather than a block one?
fn closes_inline(end: &TagEnd) -> bool {
    matches!(
        end,
        TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link | TagEnd::Image
    )
}

/// Append text, merging into the previous run when the style is unchanged.
///
/// Merging matters more than it looks: `pulldown-cmark` splits text at entity
/// and escape boundaries, so an unmerged parse of one sentence can be a dozen
/// runs, each of which a renderer would lay out separately.
fn push_run(runs: &mut Vec<InlineRun>, text: &str, style: &InlineStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.style == *style
    {
        last.text.push_str(text);
        return;
    }
    runs.push(InlineRun { text: text.to_string(), style: style.clone() });
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn table_align(a: Alignment) -> TableAlign {
    match a {
        Alignment::None => TableAlign::None,
        Alignment::Left => TableAlign::Left,
        Alignment::Center => TableAlign::Center,
        Alignment::Right => TableAlign::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_each_block_exactly() {
        let src = "# Title\n\nA paragraph.\n\n```rs\nfn x() {}\n```\n";
        let tree = parse_full(src);
        assert_eq!(tree.len(), 3);
        // The ranges are what every incremental decision anchors on, so assert
        // the text they name rather than the numbers.
        // The blank line separating blocks belongs to neither of them.
        assert_eq!(&src[tree.blocks[0].range.clone()], "# Title\n");
        assert!(src[tree.blocks[1].range.clone()].starts_with("A paragraph."));
        assert!(src[tree.blocks[2].range.clone()].starts_with("```rs"));
    }

    #[test]
    fn inline_styles_nest() {
        let tree = parse_full("**bold *both* bold**");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("expected a paragraph, got {:?}", tree.blocks[0].block);
        };
        assert_eq!(runs.len(), 3);
        assert!(runs[0].style.bold && !runs[0].style.italic);
        assert!(runs[1].style.bold && runs[1].style.italic, "nesting keeps the outer style");
        assert!(runs[2].style.bold && !runs[2].style.italic, "and restores it on close");
    }

    #[test]
    fn a_soft_break_is_a_space_not_a_newline() {
        let tree = parse_full("one\ntwo");
        let Block::Paragraph { runs } = &tree.blocks[0].block else {
            panic!("expected a paragraph");
        };
        assert_eq!(runs.len(), 1, "one merged run");
        assert_eq!(runs[0].text, "one two");
    }

    #[test]
    fn code_fence_keeps_its_language_and_body() {
        let tree = parse_full("```rust ignore\nlet x = 1;\n```");
        let Block::CodeBlock { language, code } = &tree.blocks[0].block else {
            panic!("expected a code block");
        };
        assert_eq!(language.as_deref(), Some("rust"), "only the first word names a language");
        assert_eq!(code, "let x = 1;\n");
    }

    #[test]
    fn raw_html_is_kept_verbatim() {
        let tree = parse_full("<div class=\"x\">\nhi\n</div>\n");
        let Block::Html { raw } = &tree.blocks[0].block else {
            panic!("expected an html block, got {:?}", tree.blocks[0].block);
        };
        assert!(raw.contains("<div"), "content must survive: {raw}");
    }

    #[test]
    fn link_definition_lines_are_recognised() {
        assert!(is_link_definition_line("[a]: http://x"));
        assert!(is_link_definition_line("   [a b]: /y"), "up to three spaces of indent");
        assert!(!is_link_definition_line("    [a]: /y"), "four spaces is indented code");
        assert!(!is_link_definition_line("[a] not a definition"));
        assert!(!is_link_definition_line("text [a]: /y"), "must start the line");
    }

    #[test]
    fn a_definition_forces_full_parses_from_then_on() {
        let mut p = IncrementalParser::new();
        p.set_text("para one\n\npara two\n\n");
        assert!(!p.is_full_only());
        p.append("[ref]: https://example.com\n");
        assert!(p.is_full_only(), "a definition has non-local effect");
        assert_eq!(p.last_parse_bytes(), p.text().len(), "so the window is the whole document");
    }

    /// The delta completes a line the previous append began. Scanning only the
    /// delta would see `: /url` and find nothing.
    #[test]
    fn a_definition_split_across_appends_is_still_caught() {
        let mut p = IncrementalParser::new();
        p.set_text("body\n\nmore\n\n[re");
        assert!(!p.is_full_only(), "not a definition yet");
        p.append("f]: /url\n");
        assert!(p.is_full_only(), "the rescan must start at the previous line, not the delta");
    }

    #[test]
    fn an_unchanged_set_text_does_no_work() {
        let mut p = IncrementalParser::new();
        p.set_text("a\n\nb\n\nc");
        let before = p.tree().clone();
        p.set_text("a\n\nb\n\nc");
        assert_eq!(p.tree(), &before);
    }

    #[test]
    fn a_shorter_text_resets_rather_than_appending() {
        let mut p = IncrementalParser::new();
        p.set_text("one\n\ntwo\n\nthree");
        p.set_text("one");
        assert_eq!(p.tree().len(), 1);
        assert_eq!(p.text(), "one");
    }
}
