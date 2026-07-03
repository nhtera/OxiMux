//! Pure, GPUI-free logic for the composer's slash-command palette: detect when
//! the caret sits inside a `/command` token, and rank the advertised command
//! names against the in-progress query. Kept transport- and view-agnostic so it
//! is fully unit-testable; the composer view owns the text field + the floating
//! list and calls these to decide what to show and what to insert.

use std::ops::Range;

/// An in-progress `/command` token under the caret. `range` is the byte span of
/// the whole token (the leading `/` through the caret) that a selection
/// replaces; `query` is the text after the `/` (empty right after typing `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashTrigger {
    pub query: String,
    pub range: Range<usize>,
}

/// Detect a slash-command trigger in `text` with the caret at byte offset
/// `cursor`. Returns `Some` when the caret sits in a `/token` that opens at the
/// start of the text or right after whitespace and contains no whitespace or
/// `/` — so a path like `/etc/hosts` is not a command. Stateless: recompute on
/// every keystroke; whitespace after the token yields `None` (palette closes),
/// and backspacing back into the token yields `Some` again (reopens).
pub fn detect_slash_trigger(text: &str, cursor: usize) -> Option<SlashTrigger> {
    // The caret is a byte offset from the text field; clamp it into range and
    // onto a char boundary so slicing never splits a multibyte char.
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let head = &text[..cursor];
    // The rightmost `/` before the caret that opens a token (preceded by the
    // start of the text or by whitespace).
    let slash = head.char_indices().rev().find_map(|(i, c)| {
        if c != '/' {
            return None;
        }
        let opens = i == 0 || head[..i].chars().next_back().is_some_and(char::is_whitespace);
        opens.then_some(i)
    })?;
    let token = &head[slash + 1..];
    if token.chars().any(|c| c.is_whitespace() || c == '/') {
        return None;
    }
    Some(SlashTrigger { query: token.to_string(), range: slash..cursor })
}

/// Rank `commands` against `query`, returning indices best-first. Empty query →
/// all indices in original order. Non-matches are dropped; ties break by
/// original index so the sort is stable and predictable.
pub fn rank_commands(query: &str, commands: &[String]) -> Vec<usize> {
    if query.is_empty() {
        return (0..commands.len()).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(u32, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, name)| score_command(&q, name).map(|s| (s, i)))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Best (lowest) tier score of `query` against `name` and its `:`/`-` segments,
/// so `rescue` finds `codex:rescue` and `research` finds `deep-research`.
/// `None` when nothing matches. `query` is already lowercased.
fn score_command(query: &str, name: &str) -> Option<u32> {
    let name_lc = name.to_lowercase();
    let mut best = score_text(query, &name_lc);
    for seg in name_lc.split([':', '-']).filter(|s| *s != name_lc) {
        if let Some(s) = score_text(query, seg) {
            // A segment match ranks just below a whole-name match of the same tier.
            let s = s + 5;
            best = Some(best.map_or(s, |b| b.min(s)));
        }
    }
    best
}

/// Tiered match of `query` in `haystack` (both lowercase). Lower = better:
/// exact (0) < prefix (10) < substring (30+pos) < subsequence (100).
fn score_text(query: &str, haystack: &str) -> Option<u32> {
    if haystack == query {
        Some(0)
    } else if haystack.starts_with(query) {
        Some(10)
    } else if let Some(pos) = haystack.find(query) {
        Some(30 + pos as u32)
    } else if is_subsequence(query, haystack) {
        Some(100)
    } else {
        None
    }
}

/// Whether `needle`'s chars appear in `haystack` in order (not necessarily
/// contiguous) — a loose fuzzy fallback.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|nc| hay.by_ref().any(|hc| hc == nc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trig(text: &str, cursor: usize) -> Option<SlashTrigger> {
        detect_slash_trigger(text, cursor)
    }

    #[test]
    fn leading_slash_at_start_triggers() {
        assert_eq!(
            trig("/co", 3),
            Some(SlashTrigger { query: "co".into(), range: 0..3 })
        );
    }

    #[test]
    fn bare_slash_triggers_with_empty_query() {
        assert_eq!(trig("/", 1), Some(SlashTrigger { query: "".into(), range: 0..1 }));
    }

    #[test]
    fn slash_after_whitespace_mid_line_triggers() {
        // "hello /wor" caret at end → token "wor" spanning the '/'.
        assert_eq!(
            trig("hello /wor", 10),
            Some(SlashTrigger { query: "wor".into(), range: 6..10 })
        );
    }

    #[test]
    fn caret_inside_token_reflects_typed_prefix() {
        // "/context" with caret after "co" → query "co", range 0..3.
        assert_eq!(
            trig("/context", 3),
            Some(SlashTrigger { query: "co".into(), range: 0..3 })
        );
    }

    #[test]
    fn path_is_not_a_command() {
        assert_eq!(trig("/etc/hosts", 10), None);
        assert_eq!(trig("a/b", 3), None); // mid-word slash
    }

    #[test]
    fn whitespace_after_token_closes() {
        assert_eq!(trig("/compact ", 9), None); // trailing space ends the token
        assert_eq!(trig("hi there", 8), None); // no slash at all
    }

    #[test]
    fn multibyte_draft_stays_boundary_safe() {
        // A non-ASCII prefix then a slash command; caret at end.
        let text = "café /co";
        assert_eq!(
            trig(text, text.len()),
            Some(SlashTrigger { query: "co".into(), range: 6..text.len() })
        );
        // A cursor landing inside the multibyte char returns None, never panics.
        assert_eq!(trig("é", 1), None);
    }

    #[test]
    fn cursor_past_end_is_clamped() {
        assert_eq!(trig("/co", 999), Some(SlashTrigger { query: "co".into(), range: 0..3 }));
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let cmds = names(&["compact", "clear", "context"]);
        assert_eq!(rank_commands("", &cmds), vec![0, 1, 2]);
    }

    #[test]
    fn prefix_matches_rank_before_substring_and_drop_nonmatches() {
        let cmds = names(&["compact", "context", "cook", "account", "fix", "codex:rescue", "config"]);
        let ranked = rank_commands("co", &cmds);
        let got: Vec<&str> = ranked.iter().map(|&i| cmds[i].as_str()).collect();
        // prefix matches first (stable by index), then substring "account",
        // "fix" (no 'co') dropped entirely.
        assert_eq!(got, vec!["compact", "context", "cook", "codex:rescue", "config", "account"]);
    }

    #[test]
    fn exact_match_wins() {
        let cmds = names(&["context", "cook", "co-op"]);
        let ranked = rank_commands("cook", &cmds);
        assert_eq!(cmds[ranked[0]], "cook");
    }

    #[test]
    fn namespaced_segment_matches() {
        let cmds = names(&["codex:rescue", "research", "deep-research"]);
        // "rescue" strongly matches the segment of "codex:rescue".
        assert_eq!(cmds[rank_commands("rescue", &cmds)[0]], "codex:rescue");
        // "research" exact-matches the skill, ranking above "deep-research".
        let r = rank_commands("research", &cmds);
        let got: Vec<&str> = r.iter().map(|&i| cmds[i].as_str()).collect();
        assert_eq!(got, vec!["research", "deep-research"]);
    }
}
