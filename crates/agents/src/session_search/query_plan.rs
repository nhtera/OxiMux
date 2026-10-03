//! User query → FTS5 match expressions. Pure; no SQLite.
//!
//! Every term is double-quoted: unquoted `foo-bar`, `C++` or `cli.mjs` are
//! FTS5 syntax errors, while a quoted string is simply tokenized the way the
//! index was. The planner yields a ladder — phrase, then all terms, then any
//! term — and the engine stops at the first rung with results.

use super::turn::is_token_char;

/// Longest query considered (chars).
pub const MAX_QUERY_CHARS: usize = 512;
/// Most terms considered.
pub const MAX_TERMS: usize = 48;

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is", "it", "of", "on", "or",
    "that", "the", "this", "to", "was", "what", "when", "where", "with",
];

/// The planned query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    pub terms: Vec<String>,
    /// The last term also matches as a prefix.
    pub prefix_last: bool,
    /// Match expressions to try in order (phrase → AND → OR, deduped).
    pub rungs: Vec<String>,
}

/// Plan `query`; `None` when it holds no searchable term.
pub fn plan(query: &str) -> Option<QueryPlan> {
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    let literal = is_literal(&query);
    let mut terms: Vec<String> = query
        .split(|c: char| !is_token_char(c))
        .filter(|t| !t.is_empty())
        .take(MAX_TERMS)
        .map(str::to_string)
        .collect();
    if !literal {
        let kept: Vec<String> =
            terms.iter().filter(|t| !STOP_WORDS.contains(&t.to_lowercase().as_str())).cloned().collect();
        if kept.len() >= 2 {
            terms = kept;
        }
    }
    if terms.is_empty() {
        return None;
    }
    // The last term is usually still being typed: let it match as a prefix
    // (unless the user quoted the query to ask for exact words).
    let prefix_last = !query.contains('"') && terms.last().is_some_and(|t| t.chars().count() >= 2);
    let quoted: Vec<String> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let q = quote(t);
            if i + 1 == terms.len() && prefix_last { format!("{q}*") } else { q }
        })
        .collect();
    let mut rungs = vec![quoted.join(" + "), quoted.join(" "), quoted.join(" OR ")];
    rungs.dedup();
    Some(QueryPlan { terms, prefix_last, rungs })
}

/// `"term"` with embedded quotes doubled.
pub fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// A query that names something exact — a quoted phrase, a path, a file
/// extension, an issue number, a camelCase identifier — keeps every word.
fn is_literal(q: &str) -> bool {
    q.contains('"')
        || q.contains('/')
        || q.contains('#')
        || q.split_whitespace().any(|w| {
            let has_ext = w.rsplit_once('.').is_some_and(|(a, b)| {
                !a.is_empty() && (1..=5).contains(&b.len()) && b.chars().all(|c| c.is_ascii_alphanumeric())
            });
            let camel = w.chars().zip(w.chars().skip(1)).any(|(a, b)| a.is_lowercase() && b.is_uppercase());
            has_ext || camel
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_quoted_and_the_last_is_a_prefix() {
        let p = plan("login flaky").unwrap();
        assert_eq!(p.rungs, ["\"login\" + \"flaky\"*", "\"login\" \"flaky\"*", "\"login\" OR \"flaky\"*"]);
    }

    #[test]
    fn syntax_characters_stay_inside_quotes() {
        assert_eq!(plan("foo-bar").unwrap().terms, ["foo-bar"]);
        assert_eq!(plan("C++").unwrap().terms, ["C++"]);
        assert_eq!(plan("a/b.ts").unwrap().rungs[0], "\"a/b.ts\"*");
        assert_eq!(plan("say \"hi there\"").unwrap().rungs[0], "\"say\" + \"hi\" + \"there\"");
        assert_eq!(quote("x\"y"), "\"x\"\"y\"");
    }

    #[test]
    fn stop_words_drop_only_when_two_terms_remain() {
        assert_eq!(plan("how to fix the build").unwrap().terms, ["fix", "build"]);
        assert_eq!(plan("the").unwrap().terms, ["the"]);
        // Literal queries keep every word.
        assert_eq!(plan("the main.rs").unwrap().terms, ["the", "main.rs"]);
    }

    #[test]
    fn single_term_has_one_rung_and_blank_has_none() {
        assert_eq!(plan("zebra").unwrap().rungs, ["\"zebra\"*"]);
        assert!(plan("").is_none());
        assert!(plan("  ?! ").is_none());
        assert_eq!(plan("🙂 x").unwrap().terms, ["x"]);
    }

    #[test]
    fn long_queries_are_bounded() {
        let q = "word ".repeat(400);
        let p = plan(&q).unwrap();
        assert!(p.terms.len() <= MAX_TERMS);
    }
}
