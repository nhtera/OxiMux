//! Syntax highlighting as *neutral kinds*, not colors.
//!
//! One highlighter for every code surface OxiMux draws, emitting
//! [`HighlightKind`] spans that a theme resolves to colors at paint time.
//!
//! ## The discipline (the part that matters more than the code)
//!
//! > Do not add language-specific parsing to a renderer. Unknown languages,
//! > binaries, oversized sources, incompatible grammars and parse failures must
//! > remain **plain**. Highlighting changes foreground color **only** — never
//! > font, weight, style, wrapping, height, or scroll geometry.
//!
//! That last clause is what makes highlighting safe to compute lazily and
//! off-thread: if it cannot change layout, it can arrive late without reflowing
//! anything. Every degradation path in this crate returns plain rather than an
//! error, for the same reason.
//!
//! ## Why colors live outside
//!
//! The path this replaces copied literal sRGB out of a bundled theme into each
//! token. That made a color change a *re-tokenization* — and tied the palette to
//! a `.tmTheme` file rather than to OxiMux's own theme. Kinds are stable facts
//! about the source; colors are a preference. Separating them means an
//! appearance change recolors existing spans with no parsing at all.
//!
//! ## No UI toolkit
//!
//! Enforced in CI, not by convention. It is what lets this crate be tested in
//! isolation.
//!
//! ```
//! use oximux_syntax::{HighlightKind, detect, highlight};
//!
//! let lang = detect(None, Some("rust"), "").expect("rust is a known fence tag");
//! let doc = highlight(&lang, "let x = 1; // note\n");
//!
//! let kinds: Vec<_> = doc.line(0).iter().map(|s| s.kind).collect();
//! assert!(kinds.contains(&HighlightKind::Keyword));
//! assert!(kinds.contains(&HighlightKind::Comment));
//! ```

mod cache;
mod kinds;

pub use cache::HighlightCache;
pub use kinds::HighlightKind;

use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};

/// Sources above this are left plain.
///
/// Not a performance tuning knob so much as a refusal: a multi-megabyte
/// generated file or an accidental blob is not something a reader is reading,
/// and tokenizing it stalls whoever asked. The number is generous next to any
/// real code file and small next to anything pathological.
pub const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;

/// The bundled grammar collection (~250 languages), loaded once.
///
/// The `no_newlines` variant, because every line is fed without its terminator —
/// see [`highlight`].
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

/// Bumped whenever the grammar set or the scope→kind table changes.
///
/// Folded into every cache key so entries computed under an older mapping can
/// never be served after an upgrade. Cheaper and far more reliable than
/// remembering to clear a cache.
pub const GRAMMAR_GENERATION: u32 = 1;

/// A resolved grammar.
///
/// Opaque by design: it names a grammar in the bundled set, and callers have no
/// business knowing which. Cloning is cheap — it carries a name, not a parser.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LanguageId(String);

impl LanguageId {
    /// The grammar's display name (`"Rust"`, `"Python"`), for a language tag on
    /// a code fence.
    pub fn name(&self) -> &str {
        &self.0
    }

    fn syntax(&self) -> Option<&'static SyntaxReference> {
        SYNTAX_SET.find_syntax_by_name(&self.0)
    }
}

/// One highlighted span, as a byte range **within its line**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub kind: HighlightKind,
}

/// Per-line spans for one source.
///
/// Per line rather than whole-document offsets because every consumer draws line
/// by line — a diff row, a fence row — and would otherwise have to convert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HighlightedDocument {
    lines: Vec<Vec<HighlightSpan>>,
}

impl HighlightedDocument {
    /// Spans for line `ix`, or empty for a line that has none (and for any line
    /// beyond the end — a caller that got its line count from elsewhere must not
    /// be able to panic this).
    pub fn line(&self, ix: usize) -> &[HighlightSpan] {
        self.lines.get(ix).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn lines(&self) -> usize {
        self.lines.len()
    }

    /// Total spans, for cache accounting and tests.
    pub fn span_count(&self) -> usize {
        self.lines.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(Vec::is_empty)
    }
}

/// Resolve a grammar from whatever the caller happens to know.
///
/// Tried in order of how much the signal is worth: an explicit fence tag is the
/// author saying what they meant, a path extension is strong, and a shebang is
/// the last resort for a file whose name says nothing.
///
/// `None` means plain. It is never an error — an unknown language is the normal
/// case for a fence tagged `text`, `log`, or nothing at all.
pub fn detect(path: Option<&Path>, fence_lang: Option<&str>, source: &str) -> Option<LanguageId> {
    // A fence tag is a token: `rust`, `rs`, `py`, `sh`. `find_syntax_by_token`
    // covers names, extensions and the grammars' own aliases in one lookup, so
    // `js` and `javascript` both land without a hand-kept alias table.
    if let Some(tag) = fence_lang {
        let tag = tag.trim();
        if !tag.is_empty()
            && let Some(s) = SYNTAX_SET.find_syntax_by_token(tag)
        {
            return Some(LanguageId(s.name.clone()));
        }
    }

    if let Some(path) = path {
        // Exact filename first: `Makefile`, `Dockerfile`, `Cargo.lock` have no
        // extension to speak of, and matching the whole name is what catches
        // them.
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && let Some(s) = SYNTAX_SET.find_syntax_by_token(name)
        {
            return Some(LanguageId(s.name.clone()));
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && let Some(s) = SYNTAX_SET.find_syntax_by_extension(ext)
        {
            return Some(LanguageId(s.name.clone()));
        }
    }

    // Shebang, and only an unambiguous one — `find_syntax_by_first_line`
    // already declines to guess from ordinary prose.
    if let Some(first) = source.lines().next()
        && first.starts_with("#!")
        && let Some(s) = SYNTAX_SET.find_syntax_by_first_line(first)
    {
        return Some(LanguageId(s.name.clone()));
    }

    None
}

/// Highlight a whole source, carrying grammar state across lines.
///
/// State carry is what makes a multi-line string or block comment color
/// correctly past its first line, so this is the right entry point for anything
/// that has the complete text — a chat code fence, a file preview.
///
/// Returns an empty document (i.e. plain) rather than an error for a source that
/// is too large, contains NUL bytes, or trips a grammar. See the discipline note
/// at the top of this file.
pub fn highlight(lang: &LanguageId, source: &str) -> HighlightedDocument {
    if source.len() > MAX_HIGHLIGHT_BYTES || source.as_bytes().contains(&0) {
        return HighlightedDocument::default();
    }
    let Some(syntax) = lang.syntax() else {
        return HighlightedDocument::default();
    };

    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();

    for line in source.lines() {
        // A grammar that fails mid-document stops the highlighting from there
        // on rather than discarding what already worked: the lines above parsed
        // correctly, and a reader is better served by a correct prefix than by
        // a whole file going plain because line 900 confused a regex.
        let Ok(ops) = state.parse_line(line, &SYNTAX_SET) else {
            break;
        };
        lines.push(spans_for_line(line, &ops, &mut stack));
    }

    HighlightedDocument { lines }
}

/// Highlight one line standing alone, with no state carried in or out.
///
/// For a caller holding fragments rather than a document — a diff row, where
/// only some lines of the file are present at all. A multi-line construct
/// therefore colors only as far as the line itself reveals, which is the honest
/// answer when the surrounding lines genuinely are not there.
pub fn highlight_line(lang: &LanguageId, line: &str) -> Vec<HighlightSpan> {
    if line.len() > MAX_HIGHLIGHT_BYTES || line.as_bytes().contains(&0) {
        return Vec::new();
    }
    let Some(syntax) = lang.syntax() else {
        return Vec::new();
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let Ok(ops) = state.parse_line(line, &SYNTAX_SET) else {
        return Vec::new();
    };
    spans_for_line(line, &ops, &mut stack)
}

/// Turn one line's scope-stack operations into merged, sorted, non-overlapping
/// spans.
///
/// The operations arrive as `(byte offset, push/pop)`. Between two consecutive
/// offsets the scope stack is constant, so the kind is constant — walk the
/// offsets, emit the region behind each one, then apply the operation.
fn spans_for_line(
    line: &str,
    ops: &[(usize, ScopeStackOp)],
    stack: &mut ScopeStack,
) -> Vec<HighlightSpan> {
    let mut spans: Vec<HighlightSpan> = Vec::new();
    let mut last = 0usize;

    for (offset, op) in ops {
        let offset = (*offset).min(line.len());
        if offset > last {
            push_span(&mut spans, last..offset, current_kind(stack));
            last = offset;
        }
        // A grammar op that does not apply cleanly leaves the stack as it was;
        // the remaining spans are then merely less specific, never wrong.
        let _ = stack.apply(op);
    }
    if last < line.len() {
        push_span(&mut spans, last..line.len(), current_kind(stack));
    }
    spans
}

/// The kind implied by the current scope stack.
///
/// Walks innermost-outward and stops at the first match, which matters because
/// this runs once per scope operation per line: the innermost scope is almost
/// always the one that maps, so the common case builds exactly one string.
fn current_kind(stack: &ScopeStack) -> Option<HighlightKind> {
    stack.scopes.iter().rev().find_map(|s| kinds::kind_for_scope(&s.build_string()))
}

/// Append a region, merging into the previous span when the kind is unchanged.
///
/// Merging is what keeps the output free of adjacent same-kind runs: grammars
/// push and pop structural scopes constantly, and without this a line of plain
/// text can arrive as a dozen identical spans for a renderer to lay out
/// separately.
fn push_span(spans: &mut Vec<HighlightSpan>, range: Range<usize>, kind: Option<HighlightKind>) {
    let Some(kind) = kind else {
        return; // plain: represented by absence, never by a span
    };
    if range.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.kind == kind
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    spans.push(HighlightSpan { range, kind });
}

#[cfg(test)]
mod tests {
    use super::HighlightKind::*;
    use super::*;

    fn kinds_of(doc: &HighlightedDocument, line: usize) -> Vec<HighlightKind> {
        doc.line(line).iter().map(|s| s.kind).collect()
    }

    #[test]
    fn detects_from_a_fence_tag_including_aliases() {
        for tag in ["rust", "rs"] {
            assert_eq!(detect(None, Some(tag), "").map(|l| l.name().to_string()).as_deref(), Some("Rust"), "tag: {tag}");
        }
        assert!(detect(None, Some("python"), "").is_some());
        assert!(detect(None, Some("js"), "").is_some());
    }

    #[test]
    fn detects_from_a_path_extension_and_from_an_exact_filename() {
        assert!(detect(Some(Path::new("src/main.rs")), None, "").is_some());
        assert!(
            detect(Some(Path::new("Makefile")), None, "").is_some(),
            "an extension-less name must still resolve",
        );
    }

    /// Coverage for the languages the diff view highlights today.
    ///
    /// Identical by construction — both paths resolve against the same bundled
    /// collection — but asserted anyway, because "identical by construction" is
    /// exactly the kind of claim that stops being true when someone swaps a
    /// grammar set and only notices on the surface they were looking at.
    #[test]
    fn detection_covers_the_languages_the_diff_view_handles() {
        let by_extension = [
            "rs", "py", "go", "ts", "tsx", "js", "toml", "c", "h", "cpp", "java", "rb", "php",
            "sh", "yaml", "yml", "json", "html", "css", "scss", "sql", "swift", "kt", "md", "xml",
            "vue", "svelte", "zig", "lua", "scala", "dart", "ex",
        ];
        for ext in by_extension {
            assert!(
                detect(Some(Path::new(&format!("f.{ext}"))), None, "").is_some(),
                "no grammar for .{ext}",
            );
        }
        // The same set reached the other way, as a fence tag — this is the path
        // chat actually uses, and aliases (`rs` vs `rust`) resolve differently
        // from extensions.
        for tag in ["rust", "python", "go", "typescript", "javascript", "bash", "yaml", "json"] {
            assert!(detect(None, Some(tag), "").is_some(), "no grammar for fence tag `{tag}`");
        }
    }

    /// Gaps in the bundled collection, asserted so they are documented facts
    /// rather than surprises.
    ///
    /// These are **not** regressions: the diff view resolves against the same
    /// collection, so it does not highlight them today either. `.jsx`/`.mjs`
    /// simply are not registered extensions — a fence tagged `jsx` or a file
    /// opened as `.jsx` renders plain. The fix, if one is ever wanted, is an
    /// alias table mapping them onto the JavaScript grammar; that is a decision
    /// about product coverage, not a bug in this crate.
    ///
    /// `.h` resolving to Objective-C rather than C is the collection's own
    /// choice and is left alone: guessing between them needs the file contents,
    /// and guessing wrong is worse than a consistent answer.
    #[test]
    fn known_detection_gaps() {
        assert_eq!(detect(Some(Path::new("a.jsx")), None, ""), None);
        assert_eq!(detect(Some(Path::new("a.mjs")), None, ""), None);
        assert_eq!(
            detect(Some(Path::new("a.h")), None, "").map(|l| l.name().to_string()).as_deref(),
            Some("Objective-C"),
        );
    }

    #[test]
    fn detects_from_a_shebang_when_nothing_else_says() {
        assert!(detect(None, None, "#!/bin/sh\necho hi\n").is_some());
    }

    /// Every degradation path is plain, and none of them is an error.
    #[test]
    fn unknown_inputs_degrade_to_plain() {
        assert_eq!(detect(None, Some("no-such-language"), ""), None);
        assert_eq!(detect(None, None, "just some prose\n"), None);
        assert_eq!(detect(None, Some("   "), ""), None, "an empty fence tag is not a language");
    }

    #[test]
    fn binary_and_oversized_sources_are_plain() {
        let rust = detect(None, Some("rust"), "").unwrap();
        assert!(highlight(&rust, "let a = 1;\0\n").is_empty(), "NUL means binary");

        let huge = "a".repeat(MAX_HIGHLIGHT_BYTES + 1);
        assert!(highlight(&rust, &huge).is_empty(), "oversized is plain, not slow");
    }

    #[test]
    fn highlights_the_obvious_things() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let doc = highlight(&rust, "let x = 1; // note\n");
        let kinds = kinds_of(&doc, 0);
        assert!(kinds.contains(&Keyword), "`let` is a keyword: {kinds:?}");
        assert!(kinds.contains(&Number), "`1` is a number: {kinds:?}");
        assert!(kinds.contains(&Comment), "the trailing comment: {kinds:?}");
    }

    /// The property the renderer relies on: it walks spans in order and slices
    /// the line by them, so an unsorted or overlapping span set would paint
    /// garbage or panic.
    #[test]
    fn spans_are_sorted_non_overlapping_and_in_bounds() {
        let samples = [
            ("rust", "fn main() { let s = \"hi\\n\"; /* block */ }\n\nstruct S;\n"),
            ("python", "def f(x):\n    return f'{x!r}'  # note\n"),
            ("json", "{\"a\": [1, 2.5, null], \"b\": \"\\u00e9\"}\n"),
            ("html", "<div class=\"x\">text &amp; more</div>\n"),
            ("sh", "#!/bin/sh\nfor i in $(seq 1 3); do echo \"$i\"; done\n"),
            ("yaml", "key: value\nlist:\n  - a\n  - b\n"),
        ];
        for (tag, source) in samples {
            let Some(lang) = detect(None, Some(tag), source) else {
                panic!("{tag} should resolve");
            };
            let doc = highlight(&lang, source);
            for (ix, line) in source.lines().enumerate() {
                let spans = doc.line(ix);
                let mut prev_end = 0usize;
                for span in spans {
                    assert!(span.range.start >= prev_end, "{tag} line {ix}: overlap in {spans:?}");
                    assert!(span.range.start < span.range.end, "{tag} line {ix}: empty span");
                    assert!(span.range.end <= line.len(), "{tag} line {ix}: out of bounds");
                    assert!(line.is_char_boundary(span.range.start), "{tag} line {ix}: split a char");
                    assert!(line.is_char_boundary(span.range.end), "{tag} line {ix}: split a char");
                    prev_end = span.range.end;
                }
            }
        }
    }

    /// Adjacent same-kind regions must merge, or a plain line arrives as a
    /// dozen identical spans the renderer lays out one at a time.
    #[test]
    fn adjacent_same_kind_spans_are_merged() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let doc = highlight(&rust, "// one long comment with several words\n");
        let spans = doc.line(0);
        assert_eq!(spans.len(), 1, "the whole comment is one span, got {spans:?}");
        assert_eq!(spans[0].kind, Comment);
    }

    /// Carrying state is the difference between the two entry points, and the
    /// reason `highlight` exists alongside `highlight_line`.
    #[test]
    fn whole_document_highlighting_carries_state_across_lines() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let doc = highlight(&rust, "/* opens here\nstill inside\nand closes */\n");
        assert!(
            kinds_of(&doc, 1).contains(&Comment),
            "line 2 is inside a block comment: {:?}",
            doc.line(1),
        );
    }

    /// The standalone entry point deliberately does NOT carry state — a diff row
    /// genuinely does not have its neighbours, and pretending otherwise would
    /// color from context that is not there.
    #[test]
    fn line_highlighting_stands_alone() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let spans = highlight_line(&rust, "let y = 2;");
        assert!(spans.iter().any(|s| s.kind == Keyword));
        // A continuation line, with no way to know it is inside a comment.
        let orphan = highlight_line(&rust, "still inside");
        assert!(!orphan.iter().any(|s| s.kind == Comment));
    }

    #[test]
    fn multibyte_sources_keep_char_boundaries() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let src = "let s = \"héllo 日本語 🎉\"; // café\n";
        let doc = highlight(&rust, src);
        for span in doc.line(0) {
            let line = src.lines().next().unwrap();
            assert!(line.is_char_boundary(span.range.start));
            assert!(line.is_char_boundary(span.range.end));
        }
    }

    #[test]
    fn an_empty_source_is_an_empty_document() {
        let rust = detect(None, Some("rust"), "").unwrap();
        let doc = highlight(&rust, "");
        assert!(doc.is_empty());
        assert_eq!(doc.lines(), 0);
    }
}
