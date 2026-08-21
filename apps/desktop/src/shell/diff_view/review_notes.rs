//! In-memory review-note store for one diff view, plus the markdown
//! formatter that turns accumulated notes into an agent prompt.
//!
//! Pure + GPUI-free: the store is a map keyed by a `(path, line, side)`
//! anchor (the diff scope — `repo` + `diff_ref` — is held by the `DiffView`,
//! not repeated per anchor). The `DiffView` mirrors persisted notes into this
//! store on open and writes changes back through `DiffReviewNoteRepo`; render
//! code reads it to decide which lines get a gutter marker.
//!
//! **A line number is a position, not an identity.** Between the moment a
//! note is written and the moment it is read back, the diff is recomputed —
//! the author edits the file, stages a hunk, the poller reloads. Insert one
//! line above a noted line and every number below it shifts, and a store
//! keyed only by number hands the note to whatever code now occupies that
//! slot. Nothing looks wrong: the marker is in the gutter, the note is in the
//! list, and the prompt sent to an agent quotes code the reviewer never saw
//! and asks it to act on a remark about something else entirely.
//!
//! So each note also carries the text of the line it was written against,
//! and [`ReviewNoteStore::reconcile`] re-runs the question on every load:
//! same text at that number (still anchored), that text found elsewhere in
//! the file (it moved — follow it), or that text gone (the note has outlived
//! its line and must say so rather than point somewhere false). The third
//! case is why detached notes are kept rather than dropped: the remark is
//! still the reviewer's, and losing it silently is its own kind of wrong.

use std::collections::{BTreeMap, HashMap, HashSet};

use oximux_core::{DiffReviewNote, NoteSide, anchor_text_is_checkable, anchor_text_matches};

/// Anchor identifying one note within a diff scope. Field order is
/// `(path, line, side)` so the derived `Ord` groups notes by file then walks
/// top-to-bottom — exactly the order the "Notes" list and the formatter want.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteAnchor {
    pub path: String,
    pub line: u32,
    pub side: NoteSide,
}

impl NoteAnchor {
    pub fn new(path: impl Into<String>, line: u32, side: NoteSide) -> Self {
        Self {
            path: path.into(),
            line,
            side,
        }
    }
}

/// One note's contents: what the reviewer wrote, and the line they wrote it
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub body: String,
    /// The diff line's text at the time of writing. Empty for rows persisted
    /// before the column existed — unverifiable, never treated as a mismatch.
    pub anchor_text: String,
}

impl Note {
    pub fn new(body: impl Into<String>, anchor_text: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            anchor_text: anchor_text.into(),
        }
    }
}

/// The line texts of one diff, keyed by the anchor each line would carry.
/// Built by the `DiffView` from a render plan and handed to [`
/// ReviewNoteStore::reconcile`] — the store never reaches into the diff model
/// itself.
pub type LineIndex = HashMap<NoteAnchor, String>;

/// What one reconcile pass concluded, for the caller to persist and report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Notes that followed their line elsewhere, as `(old anchor, new line)`.
    pub moves: Vec<(NoteAnchor, u32)>,
    /// How many notes lost their line in this pass.
    pub newly_detached: usize,
    /// How many detached notes found their line again.
    pub reattached: usize,
}

impl Reconciled {
    /// Whether anything at all changed — the caller's cue to write back and
    /// repaint. A pass that concluded "everything is where it was" must not
    /// cost a SQLite write or a frame.
    pub fn is_noop(&self) -> bool {
        self.moves.is_empty() && self.newly_detached == 0 && self.reattached == 0
    }
}

/// All review notes for one open diff. Ordered maps so iteration is
/// deterministic (path, then line, then side).
///
/// Two maps rather than a flag on one, because they answer different
/// questions and are read by different code: `attached` is what the gutter
/// paints markers for and what a click can reopen, `detached` is a note whose
/// line is gone — still the reviewer's, still sent to the agent, but with no
/// line left to point at.
#[derive(Debug, Default, Clone)]
pub struct ReviewNoteStore {
    attached: BTreeMap<NoteAnchor, Note>,
    detached: BTreeMap<NoteAnchor, Note>,
}

impl ReviewNoteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the store contents from a freshly-loaded DB scope. Notes with
    /// an empty body are dropped defensively (the compose UI never saves
    /// empty, but a corrupt row shouldn't surface a blank marker).
    ///
    /// Everything loads attached; [`Self::reconcile`] is what decides
    /// otherwise, and it needs a diff to decide against.
    pub fn load(&mut self, notes: Vec<DiffReviewNote>) {
        self.detached.clear();
        self.attached = notes
            .into_iter()
            .filter(|n| !n.body.trim().is_empty())
            .map(|n| {
                (
                    NoteAnchor::new(n.path, n.line, n.side),
                    Note::new(n.body, n.anchor_text),
                )
            })
            .collect();
    }

    pub fn is_empty(&self) -> bool {
        self.attached.is_empty() && self.detached.is_empty()
    }

    /// Every note in the scope, attached or not — the number the toolbar
    /// counts and the number Send will carry.
    pub fn len(&self) -> usize {
        self.attached.len() + self.detached.len()
    }

    /// How many notes have lost the line they were written against.
    pub fn detached_len(&self) -> usize {
        self.detached.len()
    }

    /// Note body at an anchor, if a note is attached there.
    pub fn get(&self, anchor: &NoteAnchor) -> Option<&str> {
        self.attached.get(anchor).map(|n| n.body.as_str())
    }

    /// Whether a specific line carries a note. Used by the prepare-time
    /// marker pass (once per diff rebuild, not per frame). O(log n) on the
    /// ordered map — the transient anchor is cheaper than scanning every key.
    ///
    /// Detached notes are deliberately invisible to this: their line number
    /// is the last place they were seen, not where they are, and painting a
    /// marker there would re-tell the lie the reconcile pass just caught.
    pub fn has_note(&self, path: &str, line: u32, side: NoteSide) -> bool {
        self.attached
            .contains_key(&NoteAnchor::new(path, line, side))
    }

    /// Insert or replace a note. An empty/whitespace body removes the note
    /// instead (treating "cleared the text" as "delete").
    pub fn set(&mut self, anchor: NoteAnchor, note: Note) {
        if note.body.trim().is_empty() {
            self.remove(&anchor);
        } else {
            // A write to a line is a write to an attached note: the author is
            // looking at the line right now.
            self.detached.remove(&anchor);
            self.attached.insert(anchor, note);
        }
    }

    pub fn remove(&mut self, anchor: &NoteAnchor) {
        self.attached.remove(anchor);
        self.detached.remove(anchor);
    }

    pub fn clear(&mut self) {
        self.attached.clear();
        self.detached.clear();
    }

    /// Remove every note whose line is gone, handing back their anchors so
    /// the caller can delete the persisted rows too.
    ///
    /// Exists because these are the only notes with no marker to click: with
    /// nothing to open, a reviewer who has read a stale remark and finished
    /// with it could otherwise clear the whole review or nothing at all.
    pub fn drain_detached(&mut self) -> Vec<NoteAnchor> {
        std::mem::take(&mut self.detached).into_keys().collect()
    }

    /// Attached notes in `(path, line, side)` order.
    pub fn iter(&self) -> impl Iterator<Item = (&NoteAnchor, &Note)> {
        self.attached.iter()
    }

    /// Detached notes in the order they were last seen.
    pub fn iter_detached(&self) -> impl Iterator<Item = (&NoteAnchor, &Note)> {
        self.detached.iter()
    }

    /// Every note in reading order, each flagged with whether its line is
    /// gone. Detached notes keep their last-known anchor, so they sort into
    /// the file where the reviewer left them.
    pub fn iter_all(&self) -> Vec<(&NoteAnchor, &Note, bool)> {
        let mut all: Vec<_> = self
            .attached
            .iter()
            .map(|(a, n)| (a, n, false))
            .chain(self.detached.iter().map(|(a, n)| (a, n, true)))
            .collect();
        all.sort_by(|a, b| a.0.cmp(b.0));
        all
    }

    /// Re-decide every note's anchor against the diff as it now reads.
    ///
    /// `index` maps each addressable diff line to its text. An empty index
    /// means the diff is not loaded — not that every line vanished — so the
    /// pass does nothing rather than detaching the entire review.
    ///
    /// Order matters within a file: notes still sitting on their own text
    /// claim their line first, so a note that never moved cannot be displaced
    /// by another note searching for a home. Movers then take the *nearest*
    /// unclaimed match to where they were, which is what makes a shifted
    /// block keep its internal order instead of collapsing onto its first
    /// duplicate line.
    pub fn reconcile(&mut self, index: &LineIndex) -> Reconciled {
        let mut out = Reconciled::default();
        if index.is_empty() {
            return out;
        }

        // Lines available to claim, grouped by the file+side a note can
        // search within.
        let mut by_file: HashMap<(&str, NoteSide), Vec<(u32, &str)>> = HashMap::new();
        for (anchor, text) in index {
            by_file
                .entry((anchor.path.as_str(), anchor.side))
                .or_default()
                .push((anchor.line, text.as_str()));
        }
        for lines in by_file.values_mut() {
            lines.sort_unstable_by_key(|(line, _)| *line);
        }

        let previously_detached: Vec<NoteAnchor> = self.detached.keys().cloned().collect();
        let mut pending: Vec<(NoteAnchor, Note, bool)> = Vec::with_capacity(self.len());
        for (anchor, note) in std::mem::take(&mut self.attached) {
            pending.push((anchor, note, false));
        }
        for (anchor, note) in std::mem::take(&mut self.detached) {
            pending.push((anchor, note, true));
        }
        pending.sort_by(|a, b| a.0.cmp(&b.0));

        // Lines already spoken for, so no two notes land on one line.
        let mut claimed: HashMap<(String, NoteSide), HashSet<u32>> = HashMap::new();
        let mut movers: Vec<(NoteAnchor, Note)> = Vec::new();

        // Pass one: notes that are still exactly where they were said to be,
        // plus the ones there is no way to check.
        for (anchor, note, was_detached) in pending {
            let unverifiable = !anchor_text_is_checkable(&note.anchor_text);
            let still_here = index
                .get(&anchor)
                .is_some_and(|live| anchor_text_matches(&note.anchor_text, live));
            if unverifiable || still_here {
                if was_detached {
                    // Its own line came back at the very number it left from.
                    out.reattached += 1;
                }
                claimed
                    .entry((anchor.path.clone(), anchor.side))
                    .or_default()
                    .insert(anchor.line);
                self.attached.insert(anchor, note);
            } else {
                movers.push((anchor, note));
            }
        }

        // Pass two: everything else goes looking for its own text.
        for (anchor, note) in movers {
            let claim_key = (anchor.path.clone(), anchor.side);
            let taken = claimed.entry(claim_key).or_default();
            let candidate = by_file
                .get(&(anchor.path.as_str(), anchor.side))
                .and_then(|lines| {
                    lines
                        .iter()
                        .filter(|(line, text)| {
                            !taken.contains(line) && anchor_text_matches(&note.anchor_text, text)
                        })
                        .min_by_key(|(line, _)| line.abs_diff(anchor.line))
                        .map(|(line, _)| *line)
                });
            match candidate {
                Some(line) => {
                    taken.insert(line);
                    let moved = NoteAnchor::new(anchor.path.clone(), line, anchor.side);
                    if line != anchor.line {
                        out.moves.push((anchor.clone(), line));
                    }
                    if previously_detached.contains(&anchor) {
                        out.reattached += 1;
                    }
                    self.attached.insert(moved, note);
                }
                None => {
                    if !previously_detached.contains(&anchor) {
                        out.newly_detached += 1;
                    }
                    self.detached.insert(anchor, note);
                }
            }
        }

        out
    }
}

/// The label above the Send / Copy / Clear chips.
///
/// Names the detached notes when there are any, because they are the ones
/// with no marker in the gutter: without a word here, a review that reads
/// "Notes (5)" while showing three markers looks like a bug in the markers
/// rather than what it is — two remarks whose code has moved on. "Gone" is
/// about the line, not the note; the note is still in the count and still in
/// what Send carries.
pub fn notes_label(total: usize, detached: usize) -> String {
    if detached == 0 {
        return format!("Notes ({total})");
    }
    let lines = if detached == 1 { "line" } else { "lines" };
    format!("Notes ({total} · {detached} {lines} gone)")
}

/// Format accumulated notes into a markdown prompt for the active agent.
///
/// `context_for` yields the code at an anchor (the diff line text, already
/// truncated by the caller per the size guard) — injected so this stays pure
/// and never reaches into the diff model. Notes are grouped by file and
/// listed top-to-bottom. Returns an empty string when there are no notes.
///
/// A detached note is labelled as such and quotes the line *as the reviewer
/// saw it*, from the note's own record. That is the honest thing to send: the
/// remark still stands, the agent is told the code has moved on since, and
/// nothing in the prompt claims a line number that no longer means anything.
pub fn format_notes_markdown(
    store: &ReviewNoteStore,
    context_for: impl Fn(&NoteAnchor) -> Option<String>,
) -> String {
    if store.is_empty() {
        return String::new();
    }

    let mut out = String::from("Please address the following code review notes:\n");
    let mut current_file: Option<&str> = None;

    for (anchor, note, detached) in store.iter_all() {
        if current_file != Some(anchor.path.as_str()) {
            out.push_str("\n## ");
            out.push_str(&anchor.path);
            out.push('\n');
            current_file = Some(anchor.path.as_str());
        }

        let side = match anchor.side {
            NoteSide::Old => "old",
            NoteSide::New => "new",
        };
        if detached {
            out.push_str(&format!(
                "\n- **L{} ({side})** — this line is no longer in the diff; \
                 the code below is how it read when the note was written\n",
                anchor.line
            ));
        } else {
            out.push_str(&format!("\n- **L{} ({side})**\n", anchor.line));
        }

        let code = if detached {
            Some(note.anchor_text.clone())
        } else {
            context_for(anchor)
        };
        if let Some(code) = code {
            let code = code.trim_end_matches('\n');
            if !code.is_empty() {
                out.push_str("  ```\n");
                for line in code.lines() {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("  ```\n");
            }
        }

        for line in note.body.trim_end().lines() {
            out.push_str("  > ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str, line: u32, side: NoteSide, body: &str) -> DiffReviewNote {
        anchored(path, line, side, body, "")
    }

    fn anchored(
        path: &str,
        line: u32,
        side: NoteSide,
        body: &str,
        anchor_text: &str,
    ) -> DiffReviewNote {
        DiffReviewNote {
            id: format!("{path}:{line}"),
            repo: "/repo".into(),
            diff_ref: "worktree:unstaged".into(),
            path: path.into(),
            side,
            line,
            body: body.into(),
            anchor_text: anchor_text.into(),
            created_at: "2026-06-08T00:00:00+00:00".into(),
            updated_at: "2026-06-08T00:00:00+00:00".into(),
        }
    }

    fn index(entries: &[(&str, u32, NoteSide, &str)]) -> LineIndex {
        entries
            .iter()
            .map(|(path, line, side, text)| {
                (NoteAnchor::new(*path, *line, *side), (*text).to_string())
            })
            .collect()
    }

    #[test]
    fn load_indexes_and_drops_empty_bodies() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            note("a.rs", 1, NoteSide::New, "real"),
            note("a.rs", 2, NoteSide::New, "   "),
        ]);
        assert_eq!(store.len(), 1);
        assert!(store.has_note("a.rs", 1, NoteSide::New));
        assert!(!store.has_note("a.rs", 2, NoteSide::New));
    }

    #[test]
    fn set_empty_body_removes() {
        let mut store = ReviewNoteStore::new();
        let anchor = NoteAnchor::new("a.rs", 5, NoteSide::New);
        store.set(anchor.clone(), Note::new("keep", "code"));
        assert_eq!(store.len(), 1);
        store.set(anchor.clone(), Note::new("  ", "code"));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn has_note_distinguishes_side() {
        let mut store = ReviewNoteStore::new();
        store.set(
            NoteAnchor::new("a.rs", 5, NoteSide::Old),
            Note::new("old", "gone();"),
        );
        assert!(store.has_note("a.rs", 5, NoteSide::Old));
        assert!(!store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn a_note_still_on_its_own_text_does_not_move() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored(
            "a.rs",
            5,
            NoteSide::New,
            "check this",
            "let x = compute();",
        )]);
        let out = store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "let x = compute();")]));
        assert!(out.is_noop(), "an unchanged diff must not cost a write");
        assert!(store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn a_note_follows_its_line_when_the_diff_shifts() {
        // Two lines inserted above: the note's text now lives at 7.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored(
            "a.rs",
            5,
            NoteSide::New,
            "check this",
            "let x = compute();",
        )]);
        let out = store.reconcile(&index(&[
            ("a.rs", 5, NoteSide::New, "let inserted = 1;"),
            ("a.rs", 6, NoteSide::New, "let inserted = 2;"),
            ("a.rs", 7, NoteSide::New, "let x = compute();"),
        ]));
        assert_eq!(out.moves, vec![(NoteAnchor::new("a.rs", 5, NoteSide::New), 7)]);
        assert!(
            store.has_note("a.rs", 7, NoteSide::New),
            "the marker belongs on the line the code is on now"
        );
        assert!(!store.has_note("a.rs", 5, NoteSide::New));
        assert_eq!(store.detached_len(), 0);
    }

    #[test]
    fn a_note_whose_line_is_gone_detaches_instead_of_pointing_at_a_stranger() {
        // The failure this whole mechanism exists for: line 5 still exists,
        // but it is somebody else's code now.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored(
            "a.rs",
            5,
            NoteSide::New,
            "check this",
            "let x = compute();",
        )]);
        let out = store.reconcile(&index(&[(
            "a.rs",
            5,
            NoteSide::New,
            "totally_unrelated_call();",
        )]));
        assert_eq!(out.newly_detached, 1);
        assert!(
            !store.has_note("a.rs", 5, NoteSide::New),
            "no marker may sit on a line the note is not about"
        );
        assert_eq!(store.len(), 1, "the reviewer's remark is not thrown away");
        assert_eq!(store.detached_len(), 1);
    }

    #[test]
    fn a_detached_note_reattaches_when_its_line_comes_back() {
        // Undo, revert, restore from stash — the code returns and so should
        // the note.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored(
            "a.rs",
            5,
            NoteSide::New,
            "check this",
            "let x = compute();",
        )]);
        store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "something else();")]));
        assert_eq!(store.detached_len(), 1);

        let out = store.reconcile(&index(&[("a.rs", 9, NoteSide::New, "let x = compute();")]));
        assert_eq!(out.reattached, 1);
        assert_eq!(store.detached_len(), 0);
        assert!(store.has_note("a.rs", 9, NoteSide::New));
    }

    #[test]
    fn an_empty_index_is_a_diff_that_has_not_loaded_yet() {
        // Reconciling against nothing would detach the entire review on every
        // reopen, one frame before the diff arrives.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 5, NoteSide::New, "n", "let x = 1;")]);
        let out = store.reconcile(&LineIndex::new());
        assert!(out.is_noop());
        assert!(store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn a_note_with_no_recorded_text_is_left_where_it_is() {
        // Written by a build before the column existed. Unverifiable is not
        // the same as wrong, and moving it would be a guess.
        let mut store = ReviewNoteStore::new();
        store.load(vec![note("a.rs", 5, NoteSide::New, "legacy")]);
        let out = store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "whatever_is_here();")]));
        assert!(out.is_noop());
        assert!(store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn a_note_on_a_blank_line_is_never_dragged_to_another_blank_line() {
        // Every blank line matches every other, so a "match" carries no
        // information — staying put is the only defensible answer.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 3, NoteSide::New, "why blank?", "   ")]);
        let out = store.reconcile(&index(&[
            ("a.rs", 3, NoteSide::New, "now_has_code();"),
            ("a.rs", 8, NoteSide::New, ""),
        ]));
        assert!(out.is_noop());
        assert!(store.has_note("a.rs", 3, NoteSide::New));
    }

    #[test]
    fn a_note_that_did_not_move_keeps_its_line_from_one_that_did() {
        // Both notes are on identical text. The one still sitting on its own
        // line claims it first, so the other cannot displace it.
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            anchored("a.rs", 2, NoteSide::New, "first", "same();"),
            anchored("a.rs", 9, NoteSide::New, "second", "same();"),
        ]);
        let out = store.reconcile(&index(&[
            ("a.rs", 2, NoteSide::New, "same();"),
            ("a.rs", 4, NoteSide::New, "same();"),
        ]));
        assert!(store.has_note("a.rs", 2, NoteSide::New));
        assert_eq!(store.get(&NoteAnchor::new("a.rs", 2, NoteSide::New)), Some("first"));
        assert_eq!(out.moves, vec![(NoteAnchor::new("a.rs", 9, NoteSide::New), 4)]);
        assert_eq!(store.get(&NoteAnchor::new("a.rs", 4, NoteSide::New)), Some("second"));
    }

    #[test]
    fn a_mover_takes_the_nearest_match_not_the_first() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 40, NoteSide::New, "n", "dup();")]);
        let out = store.reconcile(&index(&[
            ("a.rs", 2, NoteSide::New, "dup();"),
            ("a.rs", 38, NoteSide::New, "dup();"),
        ]));
        assert_eq!(out.moves, vec![(NoteAnchor::new("a.rs", 40, NoteSide::New), 38)]);
    }

    #[test]
    fn a_note_does_not_follow_its_text_into_another_file() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 5, NoteSide::New, "n", "shared();")]);
        let out = store.reconcile(&index(&[("b.rs", 5, NoteSide::New, "shared();")]));
        assert_eq!(out.newly_detached, 1);
        assert!(!store.has_note("b.rs", 5, NoteSide::New));
    }

    #[test]
    fn a_note_does_not_cross_sides() {
        // The old side's text and the new side's text can coincide; a note on
        // a removed line is not a note on the line that replaced it.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 5, NoteSide::Old, "n", "moved();")]);
        let out = store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "moved();")]));
        assert_eq!(out.newly_detached, 1);
    }

    #[test]
    fn reindentation_does_not_detach_a_note() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 5, NoteSide::New, "n", "let x = 1;")]);
        let out = store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "        let x = 1;")]));
        assert!(out.is_noop());
        assert!(store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn empty_store_formats_to_empty_string() {
        let store = ReviewNoteStore::new();
        assert_eq!(format_notes_markdown(&store, |_| None), "");
    }

    #[test]
    fn formatter_groups_by_file_in_order() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            note("b.rs", 1, NoteSide::New, "b-note"),
            note("a.rs", 9, NoteSide::New, "a-note-9"),
            note("a.rs", 2, NoteSide::New, "a-note-2"),
        ]);
        let md = format_notes_markdown(&store, |_| None);
        // Files appear a.rs before b.rs; within a.rs line 2 before 9.
        let a_pos = md.find("## a.rs").unwrap();
        let b_pos = md.find("## b.rs").unwrap();
        assert!(a_pos < b_pos);
        let n2 = md.find("a-note-2").unwrap();
        let n9 = md.find("a-note-9").unwrap();
        assert!(n2 < n9);
        assert!(md.starts_with("Please address the following code review notes:"));
    }

    #[test]
    fn formatter_includes_code_context_fence() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![note("a.rs", 42, NoteSide::New, "handle None")]);
        let md = format_notes_markdown(&store, |a| {
            assert_eq!(a.line, 42);
            Some("let x = compute();".into())
        });
        assert!(md.contains("**L42 (new)**"));
        assert!(md.contains("```"));
        assert!(md.contains("let x = compute();"));
        assert!(md.contains("> handle None"));
    }

    #[test]
    fn formatter_handles_missing_context() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![note("a.rs", 7, NoteSide::Old, "removed line note")]);
        let md = format_notes_markdown(&store, |_| None);
        assert!(md.contains("**L7 (old)**"));
        assert!(md.contains("> removed line note"));
        // No fence when there's no context.
        assert!(!md.contains("```"));
    }

    #[test]
    fn formatter_renders_multiline_note_body() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![note("a.rs", 1, NoteSide::New, "line one\nline two")]);
        let md = format_notes_markdown(&store, |_| None);
        assert!(md.contains("  > line one\n"));
        assert!(md.contains("  > line two\n"));
    }

    #[test]
    fn a_detached_note_reattaches_in_place_when_its_own_line_returns() {
        // The undo case: the text comes back at the very number it left from,
        // so the note never becomes a "mover" and pass one has to notice.
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored("a.rs", 5, NoteSide::New, "n", "let x = 1;")]);
        store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "let y = 2;")]));
        assert_eq!(store.detached_len(), 1);

        let out = store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "let x = 1;")]));
        assert_eq!(out.reattached, 1);
        assert!(!out.is_noop(), "a note coming back is a change worth reporting");
        assert!(store.has_note("a.rs", 5, NoteSide::New));
    }

    #[test]
    fn draining_detached_notes_leaves_the_attached_ones_alone() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            anchored("a.rs", 1, NoteSide::New, "stays", "keeps();"),
            anchored("a.rs", 2, NoteSide::New, "goes", "vanishes();"),
        ]);
        store.reconcile(&index(&[
            ("a.rs", 1, NoteSide::New, "keeps();"),
            ("a.rs", 2, NoteSide::New, "replaced();"),
        ]));
        let dropped = store.drain_detached();
        assert_eq!(dropped, vec![NoteAnchor::new("a.rs", 2, NoteSide::New)]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.detached_len(), 0);
        assert!(store.has_note("a.rs", 1, NoteSide::New));
    }

    #[test]
    fn the_label_names_notes_that_lost_their_line() {
        assert_eq!(notes_label(3, 0), "Notes (3)");
        assert_eq!(notes_label(3, 1), "Notes (3 · 1 line gone)");
        assert_eq!(notes_label(3, 2), "Notes (3 · 2 lines gone)");
    }

    #[test]
    fn the_count_in_the_label_includes_the_detached_ones() {
        // They are still sent, so hiding them from the count would understate
        // what the button is about to do.
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            anchored("a.rs", 1, NoteSide::New, "stays", "keeps();"),
            anchored("a.rs", 2, NoteSide::New, "goes", "vanishes();"),
        ]);
        store.reconcile(&index(&[
            ("a.rs", 1, NoteSide::New, "keeps();"),
            ("a.rs", 2, NoteSide::New, "replaced();"),
        ]));
        assert_eq!(
            notes_label(store.len(), store.detached_len()),
            "Notes (2 · 1 line gone)"
        );
    }

    #[test]
    fn formatter_says_so_when_a_notes_line_is_gone() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![anchored(
            "a.rs",
            5,
            NoteSide::New,
            "this leaks",
            "let f = File::open(p);",
        )]);
        store.reconcile(&index(&[("a.rs", 5, NoteSide::New, "let g = other();")]));

        // The live index would hand back the *replacement* line here; the
        // formatter must ignore it and quote what the reviewer actually saw.
        let md = format_notes_markdown(&store, |_| Some("let g = other();".into()));
        assert!(md.contains("no longer in the diff"));
        assert!(md.contains("let f = File::open(p);"));
        assert!(
            !md.contains("let g = other();"),
            "quoting the line that replaced it is exactly the wrong code to send"
        );
        assert!(md.contains("> this leaks"));
    }

    #[test]
    fn formatter_keeps_detached_notes_in_reading_order() {
        let mut store = ReviewNoteStore::new();
        store.load(vec![
            anchored("a.rs", 2, NoteSide::New, "early", "stays();"),
            anchored("a.rs", 8, NoteSide::New, "late", "vanishes();"),
        ]);
        store.reconcile(&index(&[
            ("a.rs", 2, NoteSide::New, "stays();"),
            ("a.rs", 8, NoteSide::New, "replaced();"),
        ]));
        let md = format_notes_markdown(&store, |_| None);
        assert!(md.matches("## a.rs").count() == 1, "one heading per file");
        assert!(md.find("early").unwrap() < md.find("late").unwrap());
    }
}
