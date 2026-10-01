//! Query matching for the search palette. Pure — no GPUI.
//!
//! Tabs and worktrees are scored field by field: every query token must match
//! some field, each match is graded on a quality ladder, and the item's
//! [`Rank`] is driven by its *worst* token. Settings and actions use the
//! simpler word scorer in [`score_simple`].

use std::cmp::Ordering;
use std::ops::Range;

/// Most tokens a query is split into; extra words are ignored.
const MAX_TOKENS: usize = 16;
/// Words that carry no intent in a settings/action query ("open settings").
const FILLER: &[&str] = &["open", "go", "the", "to"];

/// A query split into case-folded, deduplicated tokens.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub raw: String,
    pub tokens: Vec<Vec<char>>,
}

impl Prepared {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

pub fn prepare(query: &str) -> Prepared {
    let mut tokens: Vec<Vec<char>> = Vec::new();
    for word in query.split_whitespace() {
        let folded: Vec<char> = word.chars().map(fold_char).collect();
        if !tokens.contains(&folded) {
            tokens.push(folded);
        }
        if tokens.len() == MAX_TOKENS {
            break;
        }
    }
    Prepared { raw: query.to_string(), tokens }
}

/// How well one token matched one field — higher is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Subsequence,
    /// Matched once punctuation is stripped from both sides.
    Compact,
    Substring,
    /// Starts on a word boundary but runs past the end of that word.
    BoundarySubstring,
    WordPrefix,
    FieldPrefix,
    WordExact,
    FieldExact,
}

/// What a field means to its item; decides tie-breaks between fields and
/// which matches count toward `primary_hits` / `container_only`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// A project name the item merely lives in — a match only here is weak.
    Container,
    Alias,
    Secondary,
    Primary,
}

#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub text: &'a str,
    pub role: Role,
}

/// Item ordering key; greater = better.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rank {
    pub worst: Quality,
    /// Tokens whose best match was in a primary field.
    pub primary_hits: usize,
    /// Share of the primary field covered by matches, in permille — the
    /// shorter of two equally-matched titles wins.
    pub coverage: u32,
    /// Tokens that matched only the container (project) field. Fewer = better.
    pub container_only: usize,
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        self.worst
            .cmp(&other.worst)
            .then(self.primary_hits.cmp(&other.primary_hits))
            .then(self.coverage.cmp(&other.coverage))
            .then(other.container_only.cmp(&self.container_only))
    }
}

impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A scored item: its rank plus byte ranges to highlight, one list per field
/// (aligned with the `fields` slice it was scored against).
#[derive(Clone, Debug)]
pub struct Scored {
    pub rank: Rank,
    pub ranges: Vec<Vec<Range<usize>>>,
}

/// Score an item described by `fields`. `None` when any token matches no field.
pub fn score_fields(prepared: &Prepared, fields: &[Field<'_>]) -> Option<Scored> {
    if prepared.is_empty() {
        return None;
    }
    let folded: Vec<Folded> = fields.iter().map(|f| Folded::new(f.text)).collect();
    let mut ranges: Vec<Vec<Range<usize>>> = vec![Vec::new(); fields.len()];
    let mut worst = Quality::FieldExact;
    let mut primary_hits = 0;
    let mut container_only = 0;
    for token in &prepared.tokens {
        let best = fields
            .iter()
            .zip(&folded)
            .enumerate()
            .filter_map(|(i, (field, f))| classify(token, f).map(|(q, r)| (q, field.role, i, r)))
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.2.cmp(&a.2)))?;
        let (quality, role, idx, hit) = best;
        worst = worst.min(quality);
        match role {
            Role::Primary => primary_hits += 1,
            Role::Container => container_only += 1,
            Role::Alias | Role::Secondary => {}
        }
        ranges[idx].extend(hit);
    }
    for r in &mut ranges {
        *r = merge_ranges(std::mem::take(r));
    }
    let coverage = fields
        .iter()
        .position(|f| f.role == Role::Primary)
        .map(|i| {
            let len = fields[i].text.len().max(1);
            let hit: usize = ranges[i].iter().map(|r| r.len()).sum();
            (hit.min(len) * 1000 / len) as u32
        })
        .unwrap_or(0);
    Some(Scored {
        rank: Rank { worst, primary_hits, coverage, container_only },
        ranges,
    })
}

/// Word scorer for settings panes and actions: exact word 3, word prefix 2,
/// substring 1 per token; filler words ignored; needs ≥ 2 query characters and
/// more than half of the remaining tokens covered.
pub fn score_simple(prepared: &Prepared, text: &str) -> Option<u32> {
    if prepared.raw.trim().chars().count() < 2 {
        return None;
    }
    let tokens: Vec<&Vec<char>> = prepared
        .tokens
        .iter()
        .filter(|t| !FILLER.iter().any(|f| t.iter().copied().eq(f.chars())))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let folded = Folded::new(text);
    let words = folded.words();
    let mut score = 0;
    let mut covered = 0;
    for t in &tokens {
        let s = if words.iter().any(|w| w == *t) {
            3
        } else if words.iter().any(|w| w.starts_with(t)) {
            2
        } else if find_all(&folded.chars, t).next().is_some() {
            1
        } else {
            0
        };
        if s > 0 {
            covered += 1;
            score += s;
        }
    }
    (covered * 2 > tokens.len()).then_some(score)
}

fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '/' | '.' | ':' | '·' | ',' | '(' | ')' | '[' | ']' | '@' | '#')
}

/// A field case-folded per char, with each char's byte offset in the original
/// text (plus a trailing end offset) and whether a word starts there.
struct Folded {
    chars: Vec<char>,
    offs: Vec<usize>,
    starts: Vec<bool>,
}

impl Folded {
    fn new(text: &str) -> Self {
        let mut chars = Vec::with_capacity(text.len());
        let mut offs = Vec::with_capacity(text.len() + 1);
        let mut starts = Vec::with_capacity(text.len());
        let mut prev: Option<char> = None;
        for (i, c) in text.char_indices() {
            let start = !is_sep(c)
                && match prev {
                    None => true,
                    Some(p) => is_sep(p) || (p.is_lowercase() && c.is_uppercase()),
                };
            chars.push(fold_char(c));
            offs.push(i);
            starts.push(start);
            prev = Some(c);
        }
        offs.push(text.len());
        Self { chars, offs, starts }
    }

    /// True when position `j` closes a word (end of text, separator, or the
    /// start of the next camelCase word).
    fn ends_word(&self, j: usize) -> bool {
        j == self.chars.len() || is_sep(self.chars[j]) || self.starts[j]
    }

    fn words(&self) -> Vec<Vec<char>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.chars.len() {
            if self.starts[i] {
                let mut j = i + 1;
                while !self.ends_word(j) {
                    j += 1;
                }
                out.push(self.chars[i..j].to_vec());
                i = j;
            } else {
                i += 1;
            }
        }
        out
    }

    fn span(&self, from: usize, to: usize) -> Range<usize> {
        self.offs[from]..self.offs[to]
    }
}

fn find_all<'a>(hay: &'a [char], needle: &'a [char]) -> impl Iterator<Item = usize> + 'a {
    let n = needle.len();
    (0..hay.len().saturating_sub(n.saturating_sub(1)))
        .filter(move |&p| n > 0 && p + n <= hay.len() && hay[p..p + n] == *needle)
}

/// Grade `token` against one field; `None` when it does not match at all.
fn classify(token: &[char], f: &Folded) -> Option<(Quality, Vec<Range<usize>>)> {
    let m = token.len();
    if m == 0 || f.chars.is_empty() {
        return None;
    }
    if f.chars == token {
        return Some((Quality::FieldExact, vec![f.span(0, m)]));
    }
    let best = find_all(&f.chars, token)
        .map(|p| {
            let q = if f.starts[p] && f.ends_word(p + m) {
                Quality::WordExact
            } else if p == 0 {
                Quality::FieldPrefix
            } else if f.starts[p] {
                let crosses = (p + 1..p + m).any(|j| f.ends_word(j));
                if crosses { Quality::BoundarySubstring } else { Quality::WordPrefix }
            } else {
                Quality::Substring
            };
            (q, p)
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    if m == 1 {
        return best
            .filter(|(q, _)| *q >= Quality::WordPrefix)
            .map(|(q, p)| (q, vec![f.span(p, p + 1)]));
    }
    if let Some((q, p)) = best {
        return Some((q, vec![f.span(p, p + m)]));
    }
    compact_match(token, f)
        .map(|r| (Quality::Compact, r))
        .or_else(|| subsequence(token, f).map(|r| (Quality::Subsequence, r)))
}

/// Match with separators removed from both sides ("oximuxmain", "featsearch").
fn compact_match(token: &[char], f: &Folded) -> Option<Vec<Range<usize>>> {
    let tok: Vec<char> = token.iter().copied().filter(|c| !is_sep(*c)).collect();
    if tok.len() < 2 {
        return None;
    }
    let (chars, idx): (Vec<char>, Vec<usize>) = f
        .chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_sep(**c))
        .map(|(i, c)| (*c, i))
        .unzip();
    let p = find_all(&chars, &tok).next()?;
    Some(merge_ranges(idx[p..p + tok.len()].iter().map(|&i| f.span(i, i + 1)).collect()))
}

fn subsequence(token: &[char], f: &Folded) -> Option<Vec<Range<usize>>> {
    let mut hits = Vec::with_capacity(token.len());
    let mut i = 0;
    for &c in token {
        while i < f.chars.len() && f.chars[i] != c {
            i += 1;
        }
        if i == f.chars.len() {
            return None;
        }
        hits.push(f.span(i, i + 1));
        i += 1;
    }
    Some(merge_ranges(hits))
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}
