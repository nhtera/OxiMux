//! Per-message block counts, remembered between frames.
//!
//! Splitting a reply into one row per markdown block made building the row list
//! a question about *every* message rather than only the visible ones: the row
//! list is global, so the transcript cannot know where row 900 is without
//! knowing how many rows each message above it takes. Asking the parser that
//! question per message per frame is O(transcript bytes) on every streamed
//! token — [`IncrementalParser::set_text`] compares the whole string before
//! concluding nothing changed — which is precisely the cost block granularity
//! exists to remove. This is what stops it.
//!
//! ## The fingerprint
//!
//! Content length, not a hash. Streamed text is append-only, so a message whose
//! length is unchanged is a message whose text is unchanged; and hashing would
//! cost the same full pass over the bytes that this exists to avoid.
//!
//! Length alone is a sound key only because a message's text never changes in
//! place. It changes by the transcript *shrinking* — a rewind, an edit-resend,
//! a compaction — after which the indices at and above the cut are reused by
//! different messages. [`RowCache::retain_entries`] drops exactly those, in
//! lockstep with the parsers they mirror, which is what makes the reuse safe.
//!
//! ## On the budget
//!
//! The plan for this phase warns against the reference implementation's
//! unbounded transcript cache, which grows with every row ever scrolled past and
//! is freed only on chat switch. This is not that cache and does not have that
//! shape: it holds two integers per *message*, never per row and never per
//! scroll event, and it is purged on every frame that shortens the transcript.
//! A viewport-sized budget would be actively wrong here — a miss is not a
//! visual error, it is a full re-read of the message, so evicting the messages
//! that are off screen would restore the exact per-frame cost this removes.
//!
//! It is strictly smaller than the parser map it sits beside, so it introduces
//! no growth class that map does not already have. (That map keeps the full text
//! of every message, and prunes only lazily — worth a look when transcript
//! memory is next measured, but not something this cache makes worse.)

use std::collections::HashMap;

use super::MdKey;

/// What was true about a message the last time its blocks were counted.
#[derive(Clone, Copy)]
struct Counted {
    len: usize,
    blocks: usize,
}

#[derive(Default)]
pub(super) struct RowCache {
    counted: HashMap<MdKey, Counted>,
    /// How many lookups had to fall through to the parser. Test-only, and the
    /// only way to assert the cache is doing anything: a hit and a miss produce
    /// the same answer, so nothing about the rendered result can tell them
    /// apart.
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}

impl RowCache {
    /// The remembered block count for a message of exactly this length, if
    /// there is one.
    pub fn hit(&self, key: MdKey, len: usize) -> Option<usize> {
        let found = self.counted.get(&key).filter(|c| c.len == len).map(|c| c.blocks);
        #[cfg(test)]
        if found.is_none() {
            self.probes.set(self.probes.get() + 1);
        }
        found
    }

    pub fn store(&mut self, key: MdKey, len: usize, blocks: usize) {
        self.counted.insert(key, Counted { len, blocks });
    }

    /// Forget every message at or past `len` — the ones a shrinking transcript
    /// has just made available for reuse by different text.
    ///
    /// Eagerly, unlike the parser map's `> len * 2` heuristic. A retained parser
    /// is a stale *cost*; a retained count is a stale *answer*, and an answer
    /// that is wrong by one row puts every jump target below it on the wrong
    /// message.
    pub fn retain_entries(&mut self, len: usize) {
        self.counted.retain(|k, _| k.entry().is_none_or(|ix| ix < len));
    }

    /// How many lookups have missed. Test-only — see [`Self::probes`].
    #[cfg(test)]
    pub fn probes(&self) -> usize {
        self.probes.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rewind truncates the transcript and the next turn reuses the indices it
    /// freed. A count remembered against a *different* message of the same
    /// length would then place every row below it wrong — so the purge is what
    /// makes length a sound key at all.
    #[test]
    fn a_shrinking_transcript_forgets_the_indices_it_freed() {
        let mut cache = RowCache::default();
        for ix in 0..5 {
            cache.store(MdKey::Reply(ix), 100, 7);
            cache.store(MdKey::Thinking(ix), 100, 7);
        }
        // Not addressed by position, so not affected by a truncation.
        cache.store(MdKey::Plan(99), 100, 7);

        cache.retain_entries(3);

        for ix in 0..3 {
            assert_eq!(cache.hit(MdKey::Reply(ix), 100), Some(7));
            assert_eq!(cache.hit(MdKey::Thinking(ix), 100), Some(7));
        }
        for ix in 3..5 {
            assert_eq!(cache.hit(MdKey::Reply(ix), 100), None, "entry {ix} survived the rewind");
            assert_eq!(cache.hit(MdKey::Thinking(ix), 100), None);
        }
        assert_eq!(cache.hit(MdKey::Plan(99), 100), Some(7));
    }

    /// The fingerprint: a message that grew is re-read, a message that did not
    /// is not.
    #[test]
    fn a_message_that_grew_is_re_read() {
        let mut cache = RowCache::default();
        cache.store(MdKey::Reply(0), 100, 7);
        assert_eq!(cache.hit(MdKey::Reply(0), 100), Some(7));
        assert_eq!(cache.hit(MdKey::Reply(0), 101), None, "a token arrived and was ignored");
    }
}
