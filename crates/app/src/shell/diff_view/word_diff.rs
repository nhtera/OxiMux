//! Word-level diff for paired Removed/Added rows inside a hunk.
//!
//! Whole-line red / whole-line green is loud and lies when the edit is one
//! word. `pair_runs` walks a hunk's row list and groups consecutive Removed
//! and Added rows into pair-up windows. `diff_words` runs `similar` over
//! each (old, new) pair and returns a per-token span list with the op tag.
//!
//! The renderer in `render.rs` consumes these spans and paints each token
//! with its theme color: `Same` tokens use the muted row foreground;
//! `Insert` / `Delete` tokens use the bright `status_ok` / `status_error`.
//!
//! ### Pair-up policy
//!
//! Only when a Removed run and the *immediately-following* Added run carry
//! the same number of rows do we pair them index-by-index. Anything else —
//! 3 removed lines followed by 5 added lines, or a Removed run separated
//! from the next Added run by a Context line — falls back to the previous
//! whole-line tint behavior. The 1:1 constraint keeps the heuristic from
//! lying about which line "became" which under heavy edits.

use oximux_core::DiffLineKind;
use similar::{ChangeTag, TextDiff};

/// One contiguous slice of a line's content, tagged with the op that
/// applied to it relative to its pair. Renderer maps the op to a color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub text: String,
    pub op: WordOp,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WordOp {
    /// Identical token on both sides — paint with regular row color.
    Same,
    /// Token appears only on the new side — paint bright green (added).
    Insert,
    /// Token appears only on the old side — paint bright red (removed).
    Delete,
}

/// One Removed↔Added pairing inside a hunk. Indices are positions into
/// the hunk's `rows` vec, not into the underlying `DiffLine` vec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    pub old_row: usize,
    pub new_row: usize,
}

/// Scan a hunk's row kinds and emit 1:1 pairings for adjacent Removed→Added
/// runs of matching length. Anything else (run length mismatch, Context
/// row between the runs, no Added follow-up) yields no pairing for that
/// Removed run — the caller falls back to whole-line tint.
pub fn pair_runs(kinds: &[DiffLineKind]) -> Vec<Pairing> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < kinds.len() {
        if !matches!(kinds[i], DiffLineKind::Removed) {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < kinds.len() && matches!(kinds[i], DiffLineKind::Removed) {
            i += 1;
        }
        let removed_end = i;
        let added_start = i;
        while i < kinds.len() && matches!(kinds[i], DiffLineKind::Added) {
            i += 1;
        }
        let added_end = i;
        let removed_len = removed_end - removed_start;
        let added_len = added_end - added_start;
        if removed_len > 0 && removed_len == added_len {
            for k in 0..removed_len {
                out.push(Pairing {
                    old_row: removed_start + k,
                    new_row: added_start + k,
                });
            }
        }
    }
    out
}

/// Run `similar` over a (old, new) line pair at the word level and emit
/// per-token spans for both sides. `(old_spans, new_spans)`.
pub fn diff_words(old: &str, new: &str) -> (Vec<TokenSpan>, Vec<TokenSpan>) {
    let diff = TextDiff::from_words(old, new);
    let mut old_spans: Vec<TokenSpan> = Vec::new();
    let mut new_spans: Vec<TokenSpan> = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change.value().to_string();
        match change.tag() {
            ChangeTag::Equal => {
                push_or_merge(&mut old_spans, &text, WordOp::Same);
                push_or_merge(&mut new_spans, &text, WordOp::Same);
            }
            ChangeTag::Delete => {
                push_or_merge(&mut old_spans, &text, WordOp::Delete);
            }
            ChangeTag::Insert => {
                push_or_merge(&mut new_spans, &text, WordOp::Insert);
            }
        }
    }
    (old_spans, new_spans)
}

/// Coalesce adjacent same-op spans so the renderer emits one styled chunk
/// per visual run instead of one per word token. Saves child elements
/// and keeps the output close to the original whitespace structure.
fn push_or_merge(out: &mut Vec<TokenSpan>, text: &str, op: WordOp) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.op == op
    {
        last.text.push_str(text);
        return;
    }
    out.push(TokenSpan {
        text: text.to_string(),
        op,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_seq(seq: &[char]) -> Vec<DiffLineKind> {
        seq.iter()
            .map(|c| match c {
                '+' => DiffLineKind::Added,
                '-' => DiffLineKind::Removed,
                ' ' => DiffLineKind::Context,
                'n' => DiffLineKind::NoNewlineHint,
                _ => panic!("unexpected kind char {c}"),
            })
            .collect()
    }

    #[test]
    fn pair_runs_matching_length() {
        let k = kind_seq(&['-', '-', '+', '+']);
        let p = pair_runs(&k);
        assert_eq!(
            p,
            vec![
                Pairing {
                    old_row: 0,
                    new_row: 2
                },
                Pairing {
                    old_row: 1,
                    new_row: 3
                },
            ]
        );
    }

    #[test]
    fn pair_runs_mismatched_skips() {
        // 1 removed vs 2 added — no pairing emitted; renderer falls back
        // to whole-line tint for both sides.
        let k = kind_seq(&['-', '+', '+']);
        assert!(pair_runs(&k).is_empty());
    }

    #[test]
    fn pair_runs_context_between_breaks_pair() {
        // Context row between Removed and Added — runs aren't adjacent,
        // so no pairing. Avoids lying about "this removed line became
        // that added line three rows down".
        let k = kind_seq(&['-', ' ', '+']);
        assert!(pair_runs(&k).is_empty());
    }

    #[test]
    fn pair_runs_only_added_no_pair() {
        let k = kind_seq(&['+', '+', '+']);
        assert!(pair_runs(&k).is_empty());
    }

    #[test]
    fn pair_runs_two_separate_pair_blocks() {
        let k = kind_seq(&['-', '+', ' ', '-', '+']);
        let p = pair_runs(&k);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], Pairing { old_row: 0, new_row: 1 });
        assert_eq!(p[1], Pairing { old_row: 3, new_row: 4 });
    }

    #[test]
    fn diff_words_isolates_changed_token() {
        let (old, new) = diff_words("let x = 1;", "let y = 1;");
        // Old side should contain `x` as Delete; new side should contain
        // `y` as Insert. Both sides share the rest as Same.
        assert!(old.iter().any(|s| s.text.contains('x') && matches!(s.op, WordOp::Delete)));
        assert!(new.iter().any(|s| s.text.contains('y') && matches!(s.op, WordOp::Insert)));
        // No Insert tokens on the old side, no Delete tokens on the new.
        assert!(old.iter().all(|s| !matches!(s.op, WordOp::Insert)));
        assert!(new.iter().all(|s| !matches!(s.op, WordOp::Delete)));
    }

    #[test]
    fn diff_words_identical_strings_all_same() {
        let (old, new) = diff_words("hello world", "hello world");
        assert!(old.iter().all(|s| matches!(s.op, WordOp::Same)));
        assert!(new.iter().all(|s| matches!(s.op, WordOp::Same)));
    }

    #[test]
    fn diff_words_completely_different_no_same() {
        let (old, new) = diff_words("aaa", "bbb");
        assert!(old.iter().all(|s| !matches!(s.op, WordOp::Same)));
        assert!(new.iter().all(|s| !matches!(s.op, WordOp::Same)));
    }
}
