//! A bounded, content-keyed cache of highlight results.
//!
//! Highlighting the same fence on every repaint is the cost this exists to
//! remove; retaining every fence ever scrolled past is the cost a cache
//! introduces if nobody bounds it. So: keyed by content, bounded by both entry
//! count and retained bytes, evicted least-recently-used.
//!
//! **Keyed by content, not by position.** A streaming reply renumbers its blocks
//! constantly and rewrites the last one on every token; a positional key would
//! miss on every repaint and grow without limit. A content key hits whenever the
//! text is unchanged, which after the first token of a settled block is always.
//!
//! **Colors are not in here.** The cache holds [`HighlightKind`] spans, so an
//! appearance change recolors every cached entry with no parsing at all. Caching
//! resolved colors would have made a theme switch a full re-tokenization of
//! everything on screen — the defect this crate was written to remove.
//!
//! The key folds in [`GRAMMAR_GENERATION`], so entries computed under an older
//! grammar set or scope mapping cannot be served after an upgrade. That is
//! deliberately structural rather than a "remember to clear the cache" note.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use crate::{GRAMMAR_GENERATION, HighlightedDocument, LanguageId};

/// Default entry ceiling.
///
/// A conversation shows a few dozen fences at most; a few hundred entries covers
/// scrollback without pretending to be a document store.
pub const DEFAULT_MAX_ENTRIES: usize = 256;

/// Default retained-span ceiling, as a span count rather than a byte estimate.
///
/// Spans are what is actually retained — a fixed-size struct each — so counting
/// them measures the real cost, where guessing at source bytes would measure the
/// input we already dropped.
pub const DEFAULT_MAX_SPANS: usize = 200_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key(u64);

struct Entry {
    doc: Arc<HighlightedDocument>,
    spans: usize,
    used: u64,
}

/// An LRU cache of highlight results.
///
/// Not thread-safe by itself and deliberately not internally locked: the callers
/// are UI code on one thread, and a mutex here would be a lock acquired on every
/// repaint to protect against contention that does not exist.
pub struct HighlightCache {
    entries: HashMap<Key, Entry>,
    max_entries: usize,
    max_spans: usize,
    spans: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl Default for HighlightCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_SPANS)
    }
}

impl HighlightCache {
    pub fn new(max_entries: usize, max_spans: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_spans,
            spans: 0,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// The highlighting for `source`, computing it only on a miss.
    ///
    /// Returns an `Arc` so a caller can hold the result across a frame without
    /// keeping the cache borrowed, and so eviction never invalidates something
    /// already handed out.
    pub fn get(&mut self, lang: &LanguageId, source: &str) -> Arc<HighlightedDocument> {
        let key = Self::key(lang, source);
        self.clock += 1;

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = self.clock;
            self.hits += 1;
            return Arc::clone(&entry.doc);
        }

        self.misses += 1;
        let doc = Arc::new(crate::highlight(lang, source));
        let spans = doc.span_count();
        self.spans += spans;
        self.entries.insert(key, Entry { doc: Arc::clone(&doc), spans, used: self.clock });
        self.evict_to_bounds();
        doc
    }

    /// Hits and misses since construction, for tests and for anyone wondering
    /// whether the key is doing its job. A cache whose hit rate is near zero is
    /// worse than no cache, and this is how that becomes visible.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retained spans across every entry.
    pub fn retained_spans(&self) -> usize {
        self.spans
    }

    /// Drop everything. For an explicit invalidation — a grammar reload — where
    /// waiting for LRU pressure would serve stale results in the meantime.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.spans = 0;
    }

    fn key(lang: &LanguageId, source: &str) -> Key {
        let mut h = DefaultHasher::new();
        // Generation first so a bump changes every key, not just those whose
        // source happens to hash into a different bucket.
        GRAMMAR_GENERATION.hash(&mut h);
        lang.hash(&mut h);
        // Length alongside the content: two different sources would have to
        // collide on both, and it costs nothing.
        source.len().hash(&mut h);
        source.hash(&mut h);
        Key(h.finish())
    }

    fn evict_to_bounds(&mut self) {
        while self.entries.len() > self.max_entries || self.spans > self.max_spans {
            let Some(victim) =
                self.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.spans = self.spans.saturating_sub(entry.spans);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect;

    fn rust() -> LanguageId {
        detect(None, Some("rust"), "").expect("rust grammar")
    }

    /// The point of the cache: identical source is highlighted once.
    #[test]
    fn identical_source_hits() {
        let mut cache = HighlightCache::default();
        let lang = rust();
        for _ in 0..10 {
            cache.get(&lang, "let x = 1;\n");
        }
        assert_eq!(cache.stats(), (9, 1), "one parse, nine hits");
        assert_eq!(cache.len(), 1);
    }

    /// A streaming block rewrites its own text every token, so each version is
    /// genuinely a different source and must miss. Asserted so nobody "fixes"
    /// the key into something positional that would serve a stale render.
    #[test]
    fn a_growing_source_misses_each_time() {
        let mut cache = HighlightCache::default();
        let lang = rust();
        for n in 1..=5 {
            cache.get(&lang, &"x".repeat(n));
        }
        assert_eq!(cache.stats(), (0, 5));
    }

    #[test]
    fn different_languages_do_not_share_an_entry() {
        let mut cache = HighlightCache::default();
        let rust = rust();
        let python = detect(None, Some("python"), "").expect("python grammar");
        // Source the two grammars genuinely disagree about. `x = 1` would not
        // do: both call `=` an operator and `1` a number, so identical results
        // would prove nothing about the key.
        let src = "def f(): pass\n";
        let a = cache.get(&rust, src);
        let b = cache.get(&python, src);
        assert_eq!(cache.stats(), (0, 2), "same text, different grammars, two entries");
        assert_eq!(cache.len(), 2);
        assert_ne!(a, b, "the language is part of the key, and of the answer");
    }

    #[test]
    fn the_entry_ceiling_is_enforced() {
        let mut cache = HighlightCache::new(3, usize::MAX);
        let lang = rust();
        for n in 0..10 {
            cache.get(&lang, &format!("let v{n} = {n};\n"));
        }
        assert_eq!(cache.len(), 3, "bounded by entry count");
    }

    #[test]
    fn the_span_ceiling_is_enforced_and_accounting_stays_balanced() {
        let mut cache = HighlightCache::new(usize::MAX, 20);
        let lang = rust();
        for n in 0..30 {
            cache.get(&lang, &format!("let v{n} = \"s{n}\"; // c{n}\n"));
        }
        assert!(cache.retained_spans() <= 20, "got {}", cache.retained_spans());
        // Accounting must match reality, or the bound drifts until it stops
        // bounding anything.
        let actual: usize = cache.entries.values().map(|e| e.spans).sum();
        assert_eq!(cache.retained_spans(), actual);
    }

    /// Least-recently-*used*, not least-recently-inserted: a re-read must
    /// protect an entry, or a fence being repainted every frame is the first
    /// thing evicted.
    #[test]
    fn recency_is_by_use_not_by_insertion() {
        let mut cache = HighlightCache::new(2, usize::MAX);
        let lang = rust();
        cache.get(&lang, "let a = 1;\n");
        cache.get(&lang, "let b = 2;\n");
        cache.get(&lang, "let a = 1;\n"); // touch the oldest
        cache.get(&lang, "let c = 3;\n"); // evicts b, not a

        let before = cache.stats();
        cache.get(&lang, "let a = 1;\n");
        assert_eq!(cache.stats().0, before.0 + 1, "`a` survived because it was used");
    }

    /// A handed-out result must stay valid after its entry is evicted — the
    /// caller is holding it across a frame.
    #[test]
    fn an_evicted_result_is_still_usable() {
        let mut cache = HighlightCache::new(1, usize::MAX);
        let lang = rust();
        let held = cache.get(&lang, "let held = 1;\n");
        for n in 0..5 {
            cache.get(&lang, &format!("let other{n} = {n};\n"));
        }
        assert_eq!(cache.len(), 1);
        assert!(!held.is_empty(), "the Arc outlives its cache entry");
    }

    #[test]
    fn clearing_resets_the_accounting_too() {
        let mut cache = HighlightCache::default();
        let lang = rust();
        cache.get(&lang, "let x = 1;\n");
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.retained_spans(), 0, "a stale byte count would leak the bound");
    }
}
