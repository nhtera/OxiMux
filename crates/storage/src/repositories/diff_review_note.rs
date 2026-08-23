//! `DiffReviewNoteRepo` — persisted per-line diff review notes.
//!
//! Notes are anchored to a `(repo, diff_ref, path, side, line)` coordinate
//! (UNIQUE), so `upsert` edits in place when a line is re-annotated. A diff
//! tab loads its notes with `list_for_scope(repo, diff_ref)` and writes back
//! through `upsert` / `delete` / `clear_scope`.
//!
//! Each row also carries `anchor_text`, the line as it read when the note was
//! written. The line number alone cannot say whether a note is still on its
//! line — the text can, which is what lets the diff view re-anchor a drifted
//! note through [`DiffReviewNoteRepo::reanchor`] rather than leaving it
//! pointing at code its author never saw.

use oximux_core::{DiffReviewNote, NoteSide};
use rusqlite::{Row, params};

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::{new_id, now};

#[derive(Clone)]
pub struct DiffReviewNoteRepo {
    db: Db,
}

impl DiffReviewNoteRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Insert a note for the anchor, or replace the body of the existing note
    /// at that anchor. The original `id` / `created_at` survive an edit; only
    /// `body`, `anchor_text` and `updated_at` change.
    ///
    /// `anchor_text` is refreshed on an edit because the edit is the moment
    /// the author looked at that line again: whatever it reads now is what
    /// they meant this time.
    // Seven of these are one natural key plus its payload; splitting them into
    // a parameter struct would name the same fields twice.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        repo: &str,
        diff_ref: &str,
        path: &str,
        side: NoteSide,
        line: u32,
        body: &str,
        anchor_text: &str,
    ) -> Result<(), StorageError> {
        let id = new_id();
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO diff_review_notes \
                   (id, repo, diff_ref, path, side, line, body, anchor_text, \
                    created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9) \
                 ON CONFLICT(repo, diff_ref, path, side, line) DO UPDATE SET \
                   body = excluded.body, \
                   anchor_text = excluded.anchor_text, \
                   updated_at = excluded.updated_at",
                params![
                    id,
                    repo,
                    diff_ref,
                    path,
                    side.as_str(),
                    line,
                    body,
                    anchor_text,
                    ts
                ],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Move a batch of notes onto new line numbers, atomically.
    ///
    /// Each move is `(path, side, from_line, to_line)`. The caller has already
    /// established that the note's own text now lives at `to_line`; this only
    /// writes that conclusion down, so a diff reopened tomorrow starts from
    /// the settled answer instead of re-deriving it against a diff that has
    /// drifted further.
    ///
    /// Two passes, because the anchor is UNIQUE and a set of moves is a
    /// permutation: a note sliding onto another's old line collides if that
    /// one has not vacated yet, even when the final arrangement is
    /// conflict-free. The first pass parks every mover on a negative line
    /// number — a space no real note occupies, since lines are 1-based — and
    /// the second brings them down onto their targets. One transaction, so an
    /// interruption cannot leave notes parked where nothing will look for
    /// them.
    pub fn reanchor(
        &self,
        repo: &str,
        diff_ref: &str,
        moves: &[(String, NoteSide, u32, u32)],
    ) -> Result<(), StorageError> {
        if moves.is_empty() {
            return Ok(());
        }
        let ts = now();
        self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            for (idx, (path, side, from, _to)) in moves.iter().enumerate() {
                let parked = -(idx as i64) - 1;
                tx.execute(
                    "UPDATE diff_review_notes SET line = ?1 \
                     WHERE repo = ?2 AND diff_ref = ?3 AND path = ?4 \
                       AND side = ?5 AND line = ?6",
                    params![parked, repo, diff_ref, path, side.as_str(), from],
                )?;
            }
            for (idx, (path, side, _from, to)) in moves.iter().enumerate() {
                let parked = -(idx as i64) - 1;
                tx.execute(
                    "UPDATE diff_review_notes SET line = ?1, updated_at = ?2 \
                     WHERE repo = ?3 AND diff_ref = ?4 AND path = ?5 \
                       AND side = ?6 AND line = ?7",
                    params![to, ts, repo, diff_ref, path, side.as_str(), parked],
                )?;
            }
            tx.commit()
        })?;
        Ok(())
    }

    /// All notes for one diff scope, ordered by file then line so the
    /// "Notes (N)" list and the markdown formatter read top-to-bottom.
    pub fn list_for_scope(
        &self,
        repo: &str,
        diff_ref: &str,
    ) -> Result<Vec<DiffReviewNote>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, repo, diff_ref, path, side, line, body, anchor_text, \
                        created_at, updated_at \
                 FROM diff_review_notes \
                 WHERE repo = ?1 AND diff_ref = ?2 \
                 ORDER BY path ASC, line ASC",
            )?;
            let iter = stmt.query_map(params![repo, diff_ref], note_from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// Delete the note at one anchor. `Ok(())` even if no row matched —
    /// deleting an already-gone note is benign.
    pub fn delete(
        &self,
        repo: &str,
        diff_ref: &str,
        path: &str,
        side: NoteSide,
        line: u32,
    ) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM diff_review_notes \
                 WHERE repo = ?1 AND diff_ref = ?2 AND path = ?3 AND side = ?4 AND line = ?5",
                params![repo, diff_ref, path, side.as_str(), line],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Drop every note for a diff scope (the "Clear" affordance).
    pub fn clear_scope(&self, repo: &str, diff_ref: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM diff_review_notes WHERE repo = ?1 AND diff_ref = ?2",
                params![repo, diff_ref],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}

/// Map one row into a `DiffReviewNote`. A corrupt `side` slug degrades to
/// `New` rather than failing the whole query — a single bad row shouldn't
/// hide every other note in the scope.
fn note_from_row(row: &Row<'_>) -> rusqlite::Result<DiffReviewNote> {
    let side_slug: String = row.get(4)?;
    Ok(DiffReviewNote {
        id: row.get(0)?,
        repo: row.get(1)?,
        diff_ref: row.get(2)?,
        path: row.get(3)?,
        side: NoteSide::from_slug(&side_slug).unwrap_or_else(|| {
            tracing::warn!(slug = %side_slug, "unknown side slug in diff_review_notes; defaulting to New");
            NoteSide::New
        }),
        line: row.get(5)?,
        body: row.get(6)?,
        anchor_text: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    fn repo() -> DiffReviewNoteRepo {
        DiffReviewNoteRepo::new(open_memory().expect("open_memory"))
    }

    /// Most tests do not care what the anchor text is, only that it survives.
    fn put(r: &DiffReviewNoteRepo, path: &str, side: NoteSide, line: u32, body: &str) {
        r.upsert("/repo", "ref", path, side, line, body, "let x = 1;")
            .expect("upsert");
    }

    #[test]
    fn upsert_then_list_roundtrips() {
        let r = repo();
        r.upsert(
            "/repo",
            "worktree:unstaged",
            "src/a.rs",
            NoteSide::New,
            12,
            "fix this",
            "    let total = items.len();",
        )
        .expect("upsert");
        let notes = r.list_for_scope("/repo", "worktree:unstaged").expect("list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].path, "src/a.rs");
        assert_eq!(notes[0].side, NoteSide::New);
        assert_eq!(notes[0].line, 12);
        assert_eq!(notes[0].body, "fix this");
        assert_eq!(notes[0].anchor_text, "    let total = items.len();");
    }

    #[test]
    fn upsert_same_anchor_edits_in_place() {
        let r = repo();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "first", "old text")
            .expect("first");
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "second", "new text")
            .expect("second");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 1, "same anchor must not duplicate");
        assert_eq!(notes[0].body, "second");
        assert_eq!(
            notes[0].anchor_text, "new text",
            "re-annotating a line re-reads it; the stale text would outlive its own edit"
        );
    }

    #[test]
    fn a_row_from_before_the_column_existed_reads_as_unverifiable() {
        // The migration is additive with DEFAULT '', so a note written by an
        // older build must still load — with an empty anchor rather than a
        // wrong one.
        let r = repo();
        r.db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO diff_review_notes
                       (id, repo, diff_ref, path, side, line, body, created_at, updated_at)
                     VALUES ('legacy', '/repo', 'ref', 'a.rs', 'new', 4, 'old note', 't', 't')",
                    [],
                )
                .map(|_| ())
            })
            .expect("legacy insert");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].body, "old note");
        assert_eq!(notes[0].anchor_text, "");
    }

    #[test]
    fn old_and_new_side_same_line_are_distinct() {
        let r = repo();
        put(&r, "a.rs", NoteSide::Old, 5, "removed");
        put(&r, "a.rs", NoteSide::New, 5, "added");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn list_orders_by_path_then_line() {
        let r = repo();
        put(&r, "b.rs", NoteSide::New, 1, "b1");
        put(&r, "a.rs", NoteSide::New, 9, "a9");
        put(&r, "a.rs", NoteSide::New, 2, "a2");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        let order: Vec<(&str, u32)> = notes.iter().map(|n| (n.path.as_str(), n.line)).collect();
        assert_eq!(order, [("a.rs", 2), ("a.rs", 9), ("b.rs", 1)]);
    }

    #[test]
    fn scopes_are_isolated() {
        let r = repo();
        r.upsert("/repo", "ref-a", "a.rs", NoteSide::New, 1, "in a", "t")
            .unwrap();
        r.upsert("/repo", "ref-b", "a.rs", NoteSide::New, 1, "in b", "t")
            .unwrap();
        assert_eq!(r.list_for_scope("/repo", "ref-a").unwrap().len(), 1);
        assert_eq!(r.list_for_scope("/repo", "ref-b").unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_one_anchor() {
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 1, "x");
        put(&r, "a.rs", NoteSide::New, 2, "y");
        r.delete("/repo", "ref", "a.rs", NoteSide::New, 1)
            .expect("delete");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].line, 2);
    }

    #[test]
    fn delete_missing_anchor_is_ok() {
        let r = repo();
        r.delete("/repo", "ref", "nope.rs", NoteSide::Old, 1)
            .expect("delete missing is benign");
    }

    #[test]
    fn clear_scope_drops_all_in_scope_only() {
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 1, "x");
        put(&r, "b.rs", NoteSide::New, 1, "y");
        r.upsert("/repo", "other", "a.rs", NoteSide::New, 1, "z", "t")
            .unwrap();
        r.clear_scope("/repo", "ref").expect("clear");
        assert!(r.list_for_scope("/repo", "ref").unwrap().is_empty());
        assert_eq!(r.list_for_scope("/repo", "other").unwrap().len(), 1);
    }

    #[test]
    fn reanchor_moves_notes_to_their_new_lines() {
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 10, "ten");
        put(&r, "a.rs", NoteSide::New, 20, "twenty");
        r.reanchor(
            "/repo",
            "ref",
            &[
                ("a.rs".to_string(), NoteSide::New, 10, 13),
                ("a.rs".to_string(), NoteSide::New, 20, 23),
            ],
        )
        .expect("reanchor");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        let at: Vec<(u32, &str)> = notes.iter().map(|n| (n.line, n.body.as_str())).collect();
        assert_eq!(at, [(13, "ten"), (23, "twenty")]);
    }

    #[test]
    fn reanchor_survives_notes_swapping_lines() {
        // The case the two-pass write exists for: each note's target is the
        // other's current line, so a naive single UPDATE hits the UNIQUE
        // anchor halfway through and loses a note.
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 1, "was first");
        put(&r, "a.rs", NoteSide::New, 2, "was second");
        r.reanchor(
            "/repo",
            "ref",
            &[
                ("a.rs".to_string(), NoteSide::New, 1, 2),
                ("a.rs".to_string(), NoteSide::New, 2, 1),
            ],
        )
        .expect("reanchor");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 2, "no note may be dropped by a swap");
        let at: Vec<(u32, &str)> = notes.iter().map(|n| (n.line, n.body.as_str())).collect();
        assert_eq!(at, [(1, "was second"), (2, "was first")]);
    }

    #[test]
    fn reanchor_leaves_other_scopes_and_sides_alone() {
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 5, "target");
        put(&r, "a.rs", NoteSide::Old, 5, "other side");
        r.upsert("/repo", "other", "a.rs", NoteSide::New, 5, "other scope", "t")
            .unwrap();
        r.reanchor("/repo", "ref", &[("a.rs".to_string(), NoteSide::New, 5, 8)])
            .expect("reanchor");
        let moved = r.list_for_scope("/repo", "ref").expect("list");
        let lines: Vec<(NoteSide, u32)> = moved.iter().map(|n| (n.side, n.line)).collect();
        assert!(lines.contains(&(NoteSide::New, 8)));
        assert!(
            lines.contains(&(NoteSide::Old, 5)),
            "the old side never moved"
        );
        assert_eq!(r.list_for_scope("/repo", "other").unwrap()[0].line, 5);
    }

    #[test]
    fn reanchoring_nothing_is_not_a_write() {
        let r = repo();
        put(&r, "a.rs", NoteSide::New, 1, "x");
        r.reanchor("/repo", "ref", &[]).expect("empty reanchor");
        assert_eq!(r.list_for_scope("/repo", "ref").unwrap().len(), 1);
    }
}
