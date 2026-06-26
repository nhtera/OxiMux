//! Pure type-to-confirm match predicate.
//!
//! `is_match` is the single source of truth so both the render path and the
//! tests agree on what counts as a confirmed entry. Strict equality after
//! trimming surrounding whitespace — no case folding, no fuzzy matching;
//! the user typed it, the user owns it.

pub fn is_match(typed: &str, expected: &str) -> bool {
    !expected.is_empty() && typed.trim() == expected
}
