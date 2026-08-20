//! Block-level markdown for streaming agent replies.
//!
//! Two things this crate exists to do, neither of which the renderer it feeds
//! could do for itself:
//!
//! 1. **Parse to a block tree with exact byte ranges.** The ranges are the
//!    point. They are what lets a caller reparse part of a document, and what
//!    lets the transcript treat one block as one row.
//! 2. **Reparse a streaming append in O(delta), not O(document).** A reply
//!    arrives a token at a time and is re-rendered on every batch; re-reading
//!    the whole message per token is quadratic in the length of the reply, and
//!    long replies are exactly where it hurts.
//!
//! **No rendering, no colors, no GPUI.** That boundary is not tidiness — it is
//! what makes this testable in isolation, and it is enforced by the crate having
//! exactly one dependency. Anything that draws belongs to the desktop app.
//!
//! ```
//! use oximux_markdown::{Block, IncrementalParser};
//!
//! let mut p = IncrementalParser::new();
//! p.set_text("# Title\n\nfirst para\n\nsecond ");
//! p.set_text("# Title\n\nfirst para\n\nsecond para");
//!
//! assert!(matches!(p.tree().blocks[0].block, Block::Heading { level: 1, .. }));
//! // The heading was kept, not re-read, to append to the last paragraph.
//! assert_eq!(p.stable_prefix_blocks(), 1);
//! assert!(p.last_parse_bytes() < p.text().len());
//! ```
//!
//! Note the shape of that guarantee: the reparse window is the last *two*
//! blocks, so a document with fewer than three has no stable prefix and is
//! simply reparsed whole. That is the cheap case anyway — the cost this crate
//! exists to remove only appears once a reply is long.

mod mend;
mod parser;

pub use parser::{IncrementalParser, parse_full};

use std::ops::Range;

/// One top-level block plus the source bytes it came from.
///
/// `range` is load-bearing in two directions: the incremental parser anchors
/// its reparse window on it, and a block-granularity renderer uses it as the
/// identity of a row. It must be exact — a range that merely looks right will
/// produce a parser that merely usually works.
#[derive(Clone, Debug, PartialEq)]
pub struct TopBlock {
    pub range: Range<usize>,
    pub block: Block,
}

/// A parsed document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockTree {
    pub blocks: Vec<TopBlock>,
}

impl BlockTree {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }
}

/// A block-level markdown construct.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph {
        runs: Vec<InlineRun>,
    },
    Heading {
        level: u8,
        runs: Vec<InlineRun>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    BlockQuote {
        children: Vec<Block>,
    },
    List {
        /// `Some(n)` for an ordered list starting at `n`; `None` for a bullet
        /// list. Mirrors what the source actually said rather than normalising,
        /// because a list that starts at 3 renders starting at 3.
        ordered_start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Table {
        header: Vec<Vec<InlineRun>>,
        rows: Vec<Vec<Vec<InlineRun>>>,
        align: Vec<TableAlign>,
    },
    Rule,
    /// A raw HTML block, carried verbatim.
    ///
    /// Not in the original design, and added because dropping it would lose
    /// content: sessions imported from other agent CLIs embed raw HTML with
    /// some regularity, and a parser that silently discarded it would show the
    /// user a message shorter than the one the agent sent. What to *do* with it
    /// is the renderer's call — this crate's job is not to lose it.
    Html {
        raw: String,
    },
}

/// A styled span of text inside a block.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineRun {
    pub text: String,
    pub style: InlineStyle,
}

impl InlineRun {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: InlineStyle::default() }
    }
}

/// Inline styling, as a set of flags rather than a nested tree.
///
/// Flat because every consumer wants "how do I draw this span", and a nested
/// emphasis tree would make each of them re-flatten it. Nesting is representable
/// — bold *and* italic is both flags set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
    /// The destination of the link this span sits inside, if any. Carried per
    /// run rather than as a wrapping node because every consumer asks "how do I
    /// draw this span", and a nested link node would make each of them
    /// re-flatten it.
    pub link: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAlign {
    #[default]
    None,
    Left,
    Center,
    Right,
}
