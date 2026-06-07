//! `DiffReviewNoteRepo` — persisted per-line diff review notes.
//!
//! Notes are anchored to a `(repo, diff_ref, path, side, line)` coordinate
//! (UNIQUE), so `upsert` edits in place when a line is re-annotated. A diff
//! tab loads its notes with `list_for_scope(repo, diff_ref)` and writes back
//! through `upsert` / `delete` / `clear_scope`.

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
    /// `body` + `updated_at` change.
    pub fn upsert(
        &self,
        repo: &str,
        diff_ref: &str,
        path: &str,
        side: NoteSide,
        line: u32,
        body: &str,
    ) -> Result<(), StorageError> {
        let id = new_id();
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO diff_review_notes \
                   (id, repo, diff_ref, path, side, line, body, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
                 ON CONFLICT(repo, diff_ref, path, side, line) DO UPDATE SET \
                   body = excluded.body, \
                   updated_at = excluded.updated_at",
                params![id, repo, diff_ref, path, side.as_str(), line, body, ts],
            )
            .map(|_| ())
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
                "SELECT id, repo, diff_ref, path, side, line, body, created_at, updated_at \
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
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    fn repo() -> DiffReviewNoteRepo {
        DiffReviewNoteRepo::new(open_memory().expect("open_memory"))
    }

    #[test]
    fn upsert_then_list_roundtrips() {
        let r = repo();
        r.upsert("/repo", "worktree:unstaged", "src/a.rs", NoteSide::New, 12, "fix this")
            .expect("upsert");
        let notes = r.list_for_scope("/repo", "worktree:unstaged").expect("list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].path, "src/a.rs");
        assert_eq!(notes[0].side, NoteSide::New);
        assert_eq!(notes[0].line, 12);
        assert_eq!(notes[0].body, "fix this");
    }

    #[test]
    fn upsert_same_anchor_edits_in_place() {
        let r = repo();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "first")
            .expect("first");
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "second")
            .expect("second");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 1, "same anchor must not duplicate");
        assert_eq!(notes[0].body, "second");
    }

    #[test]
    fn old_and_new_side_same_line_are_distinct() {
        let r = repo();
        r.upsert("/repo", "ref", "a.rs", NoteSide::Old, 5, "removed")
            .expect("old");
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 5, "added")
            .expect("new");
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn list_orders_by_path_then_line() {
        let r = repo();
        r.upsert("/repo", "ref", "b.rs", NoteSide::New, 1, "b1").unwrap();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 9, "a9").unwrap();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 2, "a2").unwrap();
        let notes = r.list_for_scope("/repo", "ref").expect("list");
        let order: Vec<(&str, u32)> =
            notes.iter().map(|n| (n.path.as_str(), n.line)).collect();
        assert_eq!(order, [("a.rs", 2), ("a.rs", 9), ("b.rs", 1)]);
    }

    #[test]
    fn scopes_are_isolated() {
        let r = repo();
        r.upsert("/repo", "ref-a", "a.rs", NoteSide::New, 1, "in a").unwrap();
        r.upsert("/repo", "ref-b", "a.rs", NoteSide::New, 1, "in b").unwrap();
        assert_eq!(r.list_for_scope("/repo", "ref-a").unwrap().len(), 1);
        assert_eq!(r.list_for_scope("/repo", "ref-b").unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_one_anchor() {
        let r = repo();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "x").unwrap();
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 2, "y").unwrap();
        r.delete("/repo", "ref", "a.rs", NoteSide::New, 1).expect("delete");
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
        r.upsert("/repo", "ref", "a.rs", NoteSide::New, 1, "x").unwrap();
        r.upsert("/repo", "ref", "b.rs", NoteSide::New, 1, "y").unwrap();
        r.upsert("/repo", "other", "a.rs", NoteSide::New, 1, "z").unwrap();
        r.clear_scope("/repo", "ref").expect("clear");
        assert!(r.list_for_scope("/repo", "ref").unwrap().is_empty());
        assert_eq!(r.list_for_scope("/repo", "other").unwrap().len(), 1);
    }
}
