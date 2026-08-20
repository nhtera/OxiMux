//! The gate the incremental parser rests on.
//!
//! Every document below is streamed one character at a time, and after **every
//! single character** the incrementally-built tree is compared against a full
//! parse of the same prefix. They must be identical, ranges included.
//!
//! This is deliberately stricter than production, which appends whole tokens.
//! The boundary rule's failure mode is not a crash — it is an occasionally-wrong
//! tree under a specific interleaving, which no amount of looking at the app
//! will find. Splitting at every character is how you hit the interleavings
//! nobody thought of.
//!
//! **If a case here fails, do not weaken it.** The fix is to widen the reparse
//! window or add the construct to the always-full set. A green parity test is
//! the only evidence that "reparse the last two blocks" is sound at all.

use oximux_markdown::{IncrementalParser, parse_full};

/// Stream `doc` one character at a time, asserting agreement at every prefix.
fn assert_parity(name: &str, doc: &str) {
    let mut incremental = IncrementalParser::new();
    let mut prefix = String::new();

    for ch in doc.chars() {
        prefix.push(ch);
        incremental.set_text(&prefix);

        let expected = parse_full(&prefix);
        assert_eq!(
            incremental.tree(),
            &expected,
            "\n{name}: incremental and full parses disagree at {} bytes.\nprefix: {prefix:?}\n",
            prefix.len(),
        );
    }
}

/// The same document, fed as explicit `append` calls in irregular chunks —
/// closer to how a backend actually delivers deltas, and a different path
/// through `set_text`'s append detection.
fn assert_parity_chunked(name: &str, doc: &str, chunk: usize) {
    let mut incremental = IncrementalParser::new();
    let mut prefix = String::new();
    let chars: Vec<char> = doc.chars().collect();

    for group in chars.chunks(chunk) {
        let delta: String = group.iter().collect();
        prefix.push_str(&delta);
        incremental.append(&delta);

        assert_eq!(
            incremental.tree(),
            &parse_full(&prefix),
            "\n{name}: chunked append disagrees at {} bytes\n",
            prefix.len(),
        );
    }
}

fn check(name: &str, doc: &str) {
    assert_parity(name, doc);
    for chunk in [1, 3, 7, 32] {
        assert_parity_chunked(name, doc, chunk);
    }
}

// ---------------------------------------------------------------------------
// The constructs the boundary rule is most likely to get wrong
// ---------------------------------------------------------------------------

#[test]
fn nested_lists() {
    check(
        "nested lists",
        "Intro paragraph.\n\n- one\n  - one a\n  - one b\n- two\n  1. ordered\n  2. more\n\nAfter.\n",
    );
}

/// **The case the whole two-block rule exists for.**
///
/// The blank line is load-bearing. With it, `3` is a paragraph of its own — so
/// the tree is `[List, Paragraph]` — and the `.` that arrives next fuses that
/// paragraph back into the list, changing a block that was *already settled*.
/// A one-block reparse window cannot see a merge into the predecessor and
/// yields `[List(2), List(1)]` where a full parse says `[List(3)]`: a list
/// silently split in half.
///
/// Verified to bite: changing the window from two blocks to one fails this
/// test. Without the blank line it does not — `3` would be a lazy continuation
/// that never becomes its own block, and the case would pass while testing
/// nothing.
#[test]
fn a_paragraph_becoming_a_list_item() {
    check("paragraph joins list", "Lead in.\n\n1. first\n2. second\n\n3. third\n");
    check("bullet rejoins list", "Lead.\n\n- a\n- b\n\n- c\n\ntail\n");
}

/// A paragraph turns into a heading when an underline arrives beneath it.
///
/// Worth covering, but note this is a *one*-block change — the promoted
/// paragraph is the last block — so it would survive a narrower window. It is
/// here for the restyle, not as evidence for the boundary rule.
#[test]
fn a_paragraph_becoming_a_setext_heading() {
    check("setext promotion", "Before.\n\nTitle\n=====\n\nAfter.\n");
}

#[test]
fn fenced_code_with_blank_lines() {
    check(
        "fence with blanks",
        "Text.\n\n```python\ndef f():\n\n    return 1\n\n```\n\nMore text.\n",
    );
}

/// A fence whose *contents* are markdown. The parser must not treat the inner
/// `# heading` or `- list` as structure.
#[test]
fn a_fence_containing_markdown() {
    check(
        "fence of markdown",
        "Example:\n\n````markdown\n# Not a heading\n\n- not a list\n\n```\nnested fence\n```\n````\n\nDone.\n",
    );
}

#[test]
fn tables() {
    check(
        "tables",
        "Before.\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\n\nAfter.\n",
    );
}

#[test]
fn blockquotes() {
    check("blockquotes", "Intro.\n\n> quoted\n> more quoted\n>\n> - a list\n\nOut.\n");
}

/// Link-reference definitions have non-local effect, so the parser must drop to
/// full reparses. Parity has to hold across that transition, including at the
/// prefix where the definition is only half-written.
#[test]
fn link_reference_definitions() {
    check(
        "link refs",
        "See [the docs][d] and [more].\n\nBody paragraph.\n\n[d]: https://example.com\n[more]: https://example.org\n",
    );
}

#[test]
fn headings_rules_and_mixed_structure() {
    check(
        "mixed",
        "# H1\n\ntext\n\n## H2\n\n---\n\n### H3\n\n> quote\n\n- a\n- b\n\nfinal\n",
    );
}

/// Hard line breaks written as two trailing spaces. Easy to lose to a trimming
/// bug, and they change the rendered shape of a reply.
#[test]
fn two_space_hard_breaks() {
    check("hard breaks", "Line one.  \nLine two.  \nLine three.\n\nNext block.\n");
}

// ---------------------------------------------------------------------------
// Shapes that come from imported sessions
// ---------------------------------------------------------------------------

/// Captured verbatim from `crates/agents/src/thread/testdata/codex-collab-turn.jsonl`.
///
/// The point of using a real capture: this combines `**Label:**` runs with
/// two-space hard breaks and inline code in a way none of the hand-written
/// cases above happened to, and it is what one backend actually emits rather
/// than what we imagined it might.
#[test]
fn imported_codex_reply() {
    check(
        "codex capture",
        "The exact contents are `hello` followed by a newline.\n\n\
         **Status:** DONE  \n**Summary:** Read `scratch-verify.txt`; it contains `hello\\n`.  \n\
         **Concerns/Blockers:** None.",
    );
}

/// Conventions the import paths are known to carry, **modelled rather than
/// captured** — the fixtures in this repo are tool-call transcripts and contain
/// almost no prose. Named as modelled so nobody later mistakes them for
/// evidence of what a backend emits. Replace with real captures when a session
/// with rich prose is fixtured.
#[test]
fn imported_shapes_modelled() {
    check("no trailing newline", "A reply that simply stops mid-sentence with no trailing newline");
    check("embedded raw html", "Before.\n\n<details>\n<summary>more</summary>\n\ninner text\n\n</details>\n\nAfter.\n");
    check("crlf line endings", "One.\r\n\r\nTwo.\r\n\r\n- item\r\n- item\r\n");
    check("leading blank lines", "\n\n\nStarts after blanks.\n\nSecond.\n");
    check("tabs for indent", "Intro.\n\n-\titem one\n-\titem two\n\nEnd.\n");
}

/// Adversarial: constructs whose block boundaries are ambiguous until more text
/// arrives. Not required by the plan — added because these are where a
/// two-block window is most likely to be one block short.
#[test]
fn boundary_stress() {
    check("list then fence", "- a\n- b\n\n```\ncode\n```\n\n- c\n");
    check("quote holding a fence", "> ```\n> code\n> ```\n\nafter\n");
    check("consecutive fences", "```\none\n```\n```\ntwo\n```\n");
    check("list interrupted by quote", "- a\n\n> q\n\n- b\n");
    check("html then list", "<p>x</p>\n\n- a\n- b\n");
    check("indented code", "para\n\n    indented code\n    more code\n\npara\n");
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

/// The performance claim, asserted rather than assumed.
///
/// Streaming into a long document must reparse a window bounded by the last two
/// blocks — not the document. Without this test the incremental path could
/// silently degrade to a full reparse (a `full_only` trigger, a boundary bug)
/// and every test above would still pass, because they only check correctness.
#[test]
fn streaming_cost_is_bounded_by_the_last_two_blocks() {
    let mut doc = String::new();
    for i in 0..200 {
        doc.push_str(&format!("Paragraph number {i} with enough text to be worth measuring.\n\n"));
    }
    let base_len = doc.len();

    let mut p = IncrementalParser::new();
    p.set_text(&doc);
    assert!(p.tree().len() >= 200);

    // Stream a new paragraph in, a word at a time.
    let mut worst = 0;
    for word in ["A", "final", "paragraph", "arriving", "one", "word", "at", "a", "time."] {
        doc.push_str(word);
        doc.push(' ');
        p.set_text(&doc);
        worst = worst.max(p.last_parse_bytes());
    }

    assert!(
        worst < 400,
        "streaming reparsed {worst} bytes; the window should be the last two paragraphs, \
         not the {base_len}-byte document",
    );
    assert!(
        p.stable_prefix_blocks() >= 199,
        "only the tail should be rebuilt, got {} stable blocks",
        p.stable_prefix_blocks(),
    );
    // And it is still correct.
    assert_eq!(p.tree(), &parse_full(&doc));
}

/// A document with a link-reference definition is *supposed* to cost a full
/// reparse. Asserted so the escape hatch stays visible rather than becoming an
/// unexplained performance cliff.
#[test]
fn a_link_definition_deliberately_costs_full_reparses() {
    let mut doc = String::from("[d]: https://example.com\n\n");
    for i in 0..50 {
        doc.push_str(&format!("Paragraph {i}.\n\n"));
    }
    let mut p = IncrementalParser::new();
    p.set_text(&doc);
    assert!(p.is_full_only());

    doc.push_str("tail");
    p.set_text(&doc);
    assert_eq!(p.last_parse_bytes(), doc.len(), "non-local effect means no safe window");
    assert_eq!(p.tree(), &parse_full(&doc));
}

// ---------------------------------------------------------------------------
// Mend stays display-only
// ---------------------------------------------------------------------------

/// The canonical tree must never see a synthetic closer. If mending leaked into
/// it, an append would build on repaired text and the document would diverge
/// from what the agent actually sent.
#[test]
fn mending_never_mutates_the_canonical_tree() {
    let mut p = IncrementalParser::new();
    p.set_text("Intro.\n\nSecond block.\n\nThis is **hanging");

    let canonical = p.tree().clone();
    let displayed = p.display_tree().expect("a hanging marker should mend");

    assert_ne!(&canonical, &displayed, "the display tree differs, or mending did nothing");
    assert_eq!(p.tree(), &canonical, "the canonical tree is untouched");
    assert_eq!(p.tree(), &parse_full(p.text()), "and still agrees with a full parse");
}

#[test]
fn nothing_to_mend_costs_nothing() {
    let mut p = IncrementalParser::new();
    p.set_text("Intro.\n\nA complete **bold** sentence.");
    assert_eq!(p.display_tree(), None, "whole text needs no display tree");
}

/// A fence's contents are literal, so "repairing" an unbalanced marker inside
/// one would corrupt the code being shown to the user.
#[test]
fn code_blocks_are_never_mended() {
    let mut p = IncrementalParser::new();
    p.set_text("Intro.\n\n```rust\nlet x = \"**not bold\";\n");
    assert_eq!(p.display_tree(), None);
}
