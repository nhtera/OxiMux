//! Pure parsing of `@file` mentions in a composed draft. No I/O.
//!
//! Two views are needed by the composer:
//! - [`parse_mentions`] — every completed `@token` in the draft, for any
//!   downstream highlight / resolution pass.
//! - [`pending_mention`] — the single in-progress `@query` ending at the
//!   cursor, which drives the autocomplete dropdown (what to fuzzy-match and
//!   which span to replace when a file is chosen).
//!
//! A `@` only starts a mention at a word boundary (string start or right after
//! whitespace) so an email like `foo@bar.com` is never mistaken for a mention.

use std::ops::Range;

/// A completed `@token` in the source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSpan {
    /// Byte range of the whole `@token` (including the leading `@`).
    pub range: Range<usize>,
    /// The path text after `@`.
    pub path: String,
}

/// The in-progress `@query` ending at the cursor, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMention {
    /// Byte span of the `@query` to replace when a file is chosen.
    pub range: Range<usize>,
    /// Text after `@` up to the cursor (the fuzzy query; may be empty).
    pub query: String,
}

/// True when `prev` is a byte that can precede a mention's `@`.
fn is_boundary_byte(prev: u8) -> bool {
    prev.is_ascii_whitespace()
}

/// All completed `@token`s in `input`.
pub fn parse_mentions(input: &str) -> Vec<MentionSpan> {
    let bytes = input.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || is_boundary_byte(bytes[i - 1]);
        if bytes[i] == b'@' && boundary {
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > i + 1 {
                spans.push(MentionSpan {
                    range: i..j,
                    path: input[i + 1..j].to_string(),
                });
            }
            i = j;
        } else {
            i += 1;
        }
    }
    spans
}

/// The `@query` token the cursor is currently inside, or `None` when the cursor
/// is not in a mention. `cursor` is a byte offset into `input`.
pub fn pending_mention(input: &str, cursor: usize) -> Option<PendingMention> {
    let cursor = cursor.min(input.len());
    let prefix = &input[..cursor];
    let at = prefix.rfind('@')?;
    // No whitespace may sit between the '@' and the cursor.
    if prefix[at + 1..].bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    // The '@' itself must sit at a word boundary.
    if at > 0 {
        let prev = prefix.as_bytes()[at - 1];
        if !is_boundary_byte(prev) {
            return None;
        }
    }
    Some(PendingMention {
        range: at..cursor,
        query: prefix[at + 1..cursor].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_parser_detects_at_trigger() {
        let spans = parse_mentions("Fix @src/auth.rs and @README.md");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].path, "src/auth.rs");
        assert_eq!(&"Fix @src/auth.rs and @README.md"[spans[0].range.clone()], "@src/auth.rs");
        assert_eq!(spans[1].path, "README.md");
        assert_eq!(&"Fix @src/auth.rs and @README.md"[spans[1].range.clone()], "@README.md");
    }

    #[test]
    fn email_is_not_a_mention() {
        // '@' preceded by a non-whitespace char is not a mention.
        assert!(parse_mentions("ping foo@bar.com now").is_empty());
    }

    #[test]
    fn bare_at_is_not_a_completed_mention() {
        // A lone '@' with no following token yields no completed mention.
        assert!(parse_mentions("look here @ ok").is_empty());
    }

    #[test]
    fn pending_mention_tracks_cursor_query() {
        let input = "review @src/li";
        let pm = pending_mention(input, input.len()).expect("cursor inside mention");
        assert_eq!(pm.query, "src/li");
        assert_eq!(&input[pm.range.clone()], "@src/li");
    }

    #[test]
    fn pending_mention_empty_query_right_after_at() {
        let input = "review @";
        let pm = pending_mention(input, input.len()).expect("just typed '@'");
        assert_eq!(pm.query, "");
        assert_eq!(&input[pm.range.clone()], "@");
    }

    #[test]
    fn pending_mention_none_after_whitespace() {
        // Cursor after a space following the token — not composing a mention.
        let input = "review @src/auth.rs ";
        assert_eq!(pending_mention(input, input.len()), None);
    }

    #[test]
    fn pending_mention_none_for_email() {
        let input = "mail foo@bar";
        assert_eq!(pending_mention(input, input.len()), None);
    }
}
