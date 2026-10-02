//! Snippets with the query terms highlighted, built from a row's text.
//!
//! Done here rather than with FTS5's `snippet()`: that re-runs the match per
//! row, which costs milliseconds per hit on a prefix query, while this is a
//! rowid fetch plus a scan of at most a few KB.

use super::turn::is_token_char;

/// Characters of context kept before the first hit.
const LEAD_CHARS: usize = 80;
/// Visible characters per snippet (excluding ellipses).
pub const SNIPPET_MAX_CHARS: usize = 240;

/// A run of snippet text; `hit` runs matched the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub hit: bool,
}

/// Snippet of `text` around the first occurrence of any of `terms`. A token
/// matches a term whole (case-insensitively), or by prefix for the last term
/// when `prefix_last`; failing any whole-token hit, a term found inside a
/// token (a camelCase piece, `done.`) is highlighted as a substring.
pub fn build(text: &str, terms: &[String], prefix_last: bool) -> Vec<Span> {
    let flat: String = flatten(text);
    let (lower, map) = lowercase_with_map(&flat);
    let ranges: Vec<(usize, usize)> = hit_ranges(&lower, terms, prefix_last)
        .into_iter()
        .filter_map(|(a, b)| to_flat_range(&flat, &map, a, b))
        .collect();
    let first = ranges.first().map_or(0, |r| r.0);
    let start = floor_boundary(&flat, back_chars(&flat, first, LEAD_CHARS));
    let end = forward_chars(&flat, start, SNIPPET_MAX_CHARS);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span { text: "…".into(), hit: false });
    }
    let mut at = start;
    for &(a, b) in ranges.iter().filter(|(a, b)| *a >= start && *b <= end) {
        if a > at {
            spans.push(Span { text: flat[at..a].to_string(), hit: false });
        }
        spans.push(Span { text: flat[a..b].to_string(), hit: true });
        at = b;
    }
    if end > at {
        spans.push(Span { text: flat[at..end].to_string(), hit: false });
    }
    if end < flat.len() {
        spans.push(Span { text: "…".into(), hit: false });
    }
    merge(spans)
}

/// `s` lowercased, plus for every byte of the result the byte offset in `s`
/// of the character it came from. Lowercasing changes byte lengths for some
/// characters (`İ` grows, the Kelvin sign shrinks), so offsets found in the
/// lowercase text never index the original directly.
fn lowercase_with_map(s: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (i, c) in s.char_indices() {
        for l in c.to_lowercase() {
            lower.push(l);
            map.extend(std::iter::repeat_n(i, l.len_utf8()));
        }
    }
    map.push(s.len());
    (lower, map)
}

/// A lowercase byte range mapped back onto whole characters of `flat`.
fn to_flat_range(flat: &str, map: &[usize], a: usize, b: usize) -> Option<(usize, usize)> {
    let start = *map.get(a)?;
    let mut end = *map.get(b)?;
    if end <= start {
        // The range ended inside one original character's expansion.
        end = flat[start..].chars().next().map_or(flat.len(), |c| start + c.len_utf8());
    }
    (flat.is_char_boundary(start) && flat.is_char_boundary(end) && start < end).then_some((start, end))
}

/// Byte ranges of hits in `lower`, ascending, non-overlapping.
fn hit_ranges(lower: &str, terms: &[String], prefix_last: bool) -> Vec<(usize, usize)> {
    let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut out = Vec::new();
    let mut pos = 0;
    for token in lower.split(|c: char| !is_token_char(c)) {
        let start = pos + lower[pos..].find(token).unwrap_or(0);
        pos = start + token.len();
        if token.is_empty() {
            continue;
        }
        let hit = terms.iter().enumerate().any(|(i, t)| {
            token == t || (prefix_last && i + 1 == terms.len() && token.starts_with(t.as_str()))
        });
        if hit {
            out.push((start, start + token.len()));
        }
    }
    if out.is_empty() {
        for t in terms.iter().filter(|t| !t.is_empty()) {
            out.extend(lower.match_indices(t.as_str()).map(|(i, m)| (i, i + m.len())));
        }
        out.sort_unstable();
        out.dedup_by(|b, a| b.0 < a.1);
    }
    out
}

/// Whitespace runs → one space, trimmed (snippets render on one line).
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn back_chars(s: &str, from: usize, n: usize) -> usize {
    s[..from].char_indices().rev().nth(n.saturating_sub(1)).map_or(0, |(i, _)| i)
}

fn forward_chars(s: &str, from: usize, n: usize) -> usize {
    s[from..].char_indices().nth(n).map_or(s.len(), |(i, _)| from + i)
}

/// Start the snippet at a word boundary when one is near.
fn floor_boundary(s: &str, at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    s[at..].find(' ').filter(|i| *i < 16).map_or(at, |i| at + i + 1)
}

fn merge(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for s in spans.into_iter().filter(|s| !s.text.is_empty()) {
        match out.last_mut() {
            Some(last) if last.hit == s.hit => last.text.push_str(&s.text),
            _ => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(spans: &[Span]) -> String {
        spans.iter().map(|s| if s.hit { format!("[{}]", s.text) } else { s.text.clone() }).collect()
    }

    fn terms(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn highlights_whole_tokens_and_the_last_term_as_prefix() {
        let s = build("Run cargo test then cargo testing", &terms(&["cargo", "test"]), true);
        assert_eq!(render(&s), "Run [cargo] [test] then [cargo] [testing]");
        let s = build("Run cargo testing", &terms(&["cargo", "test"]), false);
        assert_eq!(render(&s), "Run [cargo] testing");
    }

    #[test]
    fn falls_back_to_substrings_inside_identifiers() {
        let s = build("fixed resolveTerminalPath. done.", &terms(&["terminal"]), false);
        assert_eq!(render(&s), "fixed resolve[Terminal]Path. done.");
    }

    #[test]
    fn windows_long_text_around_the_first_hit() {
        let text = format!("{} needle {}", "lead ".repeat(100), "tail ".repeat(100));
        let spans = build(&text, &terms(&["needle"]), false);
        let s = render(&spans);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.contains("[needle]"));
        let visible: usize = spans.iter().map(|s| s.text.chars().count()).sum();
        assert!(visible <= SNIPPET_MAX_CHARS + 2, "{visible}");
    }

    #[test]
    fn flattens_whitespace_and_survives_multibyte_text() {
        assert_eq!(render(&build("a\n\n  b\tc", &terms(&["b"]), false)), "a [b] c");
        assert_eq!(render(&build("日本語 テスト", &terms(&["テスト"]), false)), "日本語 [テスト]");
        assert_eq!(render(&build("İstanbul x", &terms(&["x"]), false)), "İstanbul [x]");
        // Lowercasing that grows one char and shrinks another must not
        // shift offsets (this used to panic on a char boundary).
        assert_eq!(render(&build("İ İ x \u{212A}", &terms(&["x"]), false)), "İ İ [x] \u{212A}");
    }
}
