//! In-memory review-note store for one diff view, plus the markdown
//! formatter that turns accumulated notes into an agent prompt.
//!
//! Pure + GPUI-free: the store is a map keyed by a `(path, line, side)`
//! anchor (the diff scope — `repo` + `diff_ref` — is held by the `DiffView`,
//! not repeated per anchor). The `DiffView` mirrors persisted notes into this
//! store on open and writes changes back through `DiffReviewNoteRepo`; render
//! code reads it to decide which lines get a gutter marker.

use std::collections::BTreeMap;

use oximux_core::{DiffReviewNote, NoteSide};

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

/// All review notes for one open diff, indexed by anchor. Ordered map so
/// iteration is deterministic (path, then line, then side).
#[derive(Debug, Default, Clone)]
pub struct ReviewNoteStore {
    notes: BTreeMap<NoteAnchor, String>,
}

impl ReviewNoteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the store contents from a freshly-loaded DB scope. Notes with
    /// an empty body are dropped defensively (the compose UI never saves
    /// empty, but a corrupt row shouldn't surface a blank marker).
    pub fn load(&mut self, notes: Vec<DiffReviewNote>) {
        self.notes = notes
            .into_iter()
            .filter(|n| !n.body.trim().is_empty())
            .map(|n| (NoteAnchor::new(n.path, n.line, n.side), n.body))
            .collect();
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    /// Note body at an anchor, if present.
    pub fn get(&self, anchor: &NoteAnchor) -> Option<&str> {
        self.notes.get(anchor).map(String::as_str)
    }

    /// Whether a specific line carries a note. Used by the prepare-time
    /// marker pass (once per diff rebuild, not per frame).
    pub fn has_note(&self, path: &str, line: u32, side: NoteSide) -> bool {
        // Borrow-friendly probe: scan only the file's contiguous key range.
        // The map is small (a review is tens of notes), so this stays cheap
        // and avoids allocating an owned key per check.
        self.notes
            .keys()
            .any(|a| a.line == line && a.side == side && a.path == path)
    }

    /// Insert or replace a note. An empty/whitespace body removes the note
    /// instead (treating "cleared the text" as "delete").
    pub fn set(&mut self, anchor: NoteAnchor, body: String) {
        if body.trim().is_empty() {
            self.notes.remove(&anchor);
        } else {
            self.notes.insert(anchor, body);
        }
    }

    pub fn remove(&mut self, anchor: &NoteAnchor) {
        self.notes.remove(anchor);
    }

    pub fn clear(&mut self) {
        self.notes.clear();
    }

    /// All notes in `(path, line, side)` order.
    pub fn iter(&self) -> impl Iterator<Item = (&NoteAnchor, &str)> {
        self.notes.iter().map(|(a, b)| (a, b.as_str()))
    }
}

/// Format accumulated notes into a markdown prompt for the active agent.
///
/// `context_for` yields the code at an anchor (the diff line text, already
/// truncated by the caller per the size guard) — injected so this stays pure
/// and never reaches into the diff model. Notes are grouped by file and
/// listed top-to-bottom. Returns an empty string when there are no notes.
pub fn format_notes_markdown(
    store: &ReviewNoteStore,
    context_for: impl Fn(&NoteAnchor) -> Option<String>,
) -> String {
    if store.is_empty() {
        return String::new();
    }

    let mut out = String::from("Please address the following code review notes:\n");
    let mut current_file: Option<&str> = None;

    for (anchor, body) in store.iter() {
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
        out.push_str(&format!("\n- **L{} ({side})**\n", anchor.line));

        if let Some(code) = context_for(anchor) {
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

        for line in body.trim_end().lines() {
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
        DiffReviewNote {
            id: format!("{path}:{line}"),
            repo: "/repo".into(),
            diff_ref: "worktree:unstaged".into(),
            path: path.into(),
            side,
            line,
            body: body.into(),
            created_at: "2026-06-08T00:00:00+00:00".into(),
            updated_at: "2026-06-08T00:00:00+00:00".into(),
        }
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
        store.set(anchor.clone(), "keep".into());
        assert_eq!(store.len(), 1);
        store.set(anchor.clone(), "  ".into());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn has_note_distinguishes_side() {
        let mut store = ReviewNoteStore::new();
        store.set(NoteAnchor::new("a.rs", 5, NoteSide::Old), "old".into());
        assert!(store.has_note("a.rs", 5, NoteSide::Old));
        assert!(!store.has_note("a.rs", 5, NoteSide::New));
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
}
