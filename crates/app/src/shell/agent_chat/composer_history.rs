//! Shell-style prompt history for the chat composer: a list of previously-sent
//! prompts that ↑/↓ walk through, like a terminal's command history (the CLI's
//! "History i/N"). Pure state — no GPUI — so the walk logic is unit-testable
//! without a laid-out view. The composer owns one of these and drives the input
//! text from it; see [`super::composer`].
//!
//! Model: `entries` is oldest→newest. `idx == None` means "showing the live
//! draft" (not navigating); `idx == Some(i)` means the input currently shows
//! `entries[i]`. When navigation starts, the live draft is stashed and restored
//! when the user walks back past the newest entry.

/// The recall state machine. `record` on send, `older`/`newer` on ↑/↓, `detach`
/// when the user edits a recalled entry.
#[derive(Default)]
pub(super) struct PromptHistory {
    /// Sent prompts, oldest first. Adjacent duplicates are collapsed on push.
    entries: Vec<String>,
    /// Which entry the input currently shows, or `None` when showing the live
    /// (un-recalled) draft.
    idx: Option<usize>,
    /// The live draft saved when navigation began, restored on walking past the
    /// newest entry. `None` while not navigating.
    stashed: Option<String>,
}

impl PromptHistory {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Replace the history with `entries` (oldest→newest), e.g. seeded from a
    /// restored transcript. Resets any in-progress navigation.
    pub(super) fn seed(&mut self, entries: Vec<String>) {
        self.entries = entries;
        self.idx = None;
        self.stashed = None;
    }

    /// Record a just-sent prompt as the newest entry and reset navigation.
    /// Blank prompts and an exact repeat of the newest entry are ignored (so
    /// hammering the same message doesn't bloat the history, matching a shell).
    pub(super) fn record(&mut self, text: &str) {
        self.idx = None;
        self.stashed = None;
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.entries.last().map(String::as_str) == Some(text) {
            return;
        }
        self.entries.push(text.to_string());
    }

    /// Whether the input currently shows a recalled entry (vs. the live draft).
    pub(super) fn is_navigating(&self) -> bool {
        self.idx.is_some()
    }

    /// The entry the input should currently show, or `None` when not navigating.
    /// Used to tell our own programmatic reload apart from a genuine user edit.
    pub(super) fn current(&self) -> Option<&str> {
        self.idx.and_then(|i| self.entries.get(i)).map(String::as_str)
    }

    /// One-based position + total for a "History i/N" indicator, or `None` when
    /// not navigating.
    pub(super) fn position(&self) -> Option<(usize, usize)> {
        self.idx.map(|i| (i + 1, self.entries.len()))
    }

    /// Walk to an older entry (↑). `live` is the current draft, stashed on the
    /// first step so it can be restored later. Returns the text to load into the
    /// input, or `None` when there is no history at all (so the caller lets the
    /// caret move normally instead of consuming the key). Staying on the oldest
    /// entry returns that entry (a consumed no-op), never `None`.
    pub(super) fn older(&mut self, live: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.idx {
            None => {
                self.stashed = Some(live.to_string());
                let last = self.entries.len() - 1;
                self.idx = Some(last);
                Some(self.entries[last].clone())
            }
            Some(0) => Some(self.entries[0].clone()),
            Some(i) => {
                self.idx = Some(i - 1);
                Some(self.entries[i - 1].clone())
            }
        }
    }

    /// Walk to a newer entry (↓). Returns the text to load, or `None` when not
    /// navigating (so the caller lets the caret move down normally). Walking past
    /// the newest entry leaves navigation and restores the stashed live draft.
    pub(super) fn newer(&mut self) -> Option<String> {
        match self.idx {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                self.idx = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                // Past the newest: back to the live draft.
                self.idx = None;
                Some(self.stashed.take().unwrap_or_default())
            }
        }
    }

    /// Stop navigating without changing the input — call when the user edits a
    /// recalled entry, so the edited text becomes the new live draft and ↑ starts
    /// again from the newest entry.
    pub(super) fn detach(&mut self) {
        self.idx = None;
        self.stashed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> PromptHistory {
        let mut h = PromptHistory::new();
        h.seed(vec!["first".into(), "second".into(), "third".into()]);
        h
    }

    #[test]
    fn older_enters_from_newest_and_stops_at_oldest() {
        let mut h = seeded();
        assert_eq!(h.older("draft").as_deref(), Some("third"));
        assert_eq!(h.position(), Some((3, 3)));
        assert_eq!(h.older("draft").as_deref(), Some("second"));
        assert_eq!(h.older("draft").as_deref(), Some("first"));
        // Already oldest: stays on it (a consumed no-op), never falls through.
        assert_eq!(h.older("draft").as_deref(), Some("first"));
        assert_eq!(h.position(), Some((1, 3)));
    }

    #[test]
    fn newer_walks_forward_then_restores_the_stashed_draft() {
        let mut h = seeded();
        h.older("my draft"); // -> third
        h.older("my draft"); // -> second
        assert_eq!(h.newer().as_deref(), Some("third"));
        // Past the newest: leaves navigation, restores the draft that was live
        // when navigation began.
        assert_eq!(h.newer().as_deref(), Some("my draft"));
        assert!(!h.is_navigating());
        // Nothing newer when not navigating: fall through (None).
        assert_eq!(h.newer(), None);
    }

    #[test]
    fn empty_history_older_falls_through() {
        let mut h = PromptHistory::new();
        assert_eq!(h.older("draft"), None);
        assert!(!h.is_navigating());
    }

    #[test]
    fn record_dedups_adjacent_and_resets_navigation() {
        let mut h = seeded();
        h.older("draft"); // navigating
        h.record("fourth");
        assert!(!h.is_navigating(), "recording ends navigation");
        assert_eq!(h.position(), None);
        // Adjacent duplicate + blank are ignored.
        h.record("fourth");
        h.record("   ");
        // Newest is "fourth"; walking up once lands on it.
        assert_eq!(h.older("x").as_deref(), Some("fourth"));
    }

    #[test]
    fn detach_ends_navigation_without_restoring_draft() {
        let mut h = seeded();
        h.older("draft"); // -> third
        h.detach();
        assert!(!h.is_navigating());
        // ↑ starts fresh from the newest again.
        assert_eq!(h.older("edited").as_deref(), Some("third"));
    }

    #[test]
    fn current_tracks_the_shown_entry() {
        let mut h = seeded();
        assert_eq!(h.current(), None);
        h.older("draft");
        assert_eq!(h.current(), Some("third"));
        h.older("draft");
        assert_eq!(h.current(), Some("second"));
    }
}
