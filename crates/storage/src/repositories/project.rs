//! `ProjectRepo` — typed CRUD over the `projects` table.

use oximux_core::Project;
use rusqlite::{OptionalExtension, params};

use super::{classify_unique, new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::ProjectRow;

#[derive(Clone)]
pub struct ProjectRepo {
    db: Db,
}

impl ProjectRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn insert(
        &self,
        name: &str,
        root_path: &str,
        default_branch: &str,
    ) -> Result<Project, StorageError> {
        let id = new_id();
        let created_at = now();
        // New projects append to the end of the manual order. `next_sort_order`
        // is `max + 1.0`; the very first row lands at `1.0` (never `0.0`, which
        // is the not-yet-backfilled sentinel).
        let sort_order = self.next_sort_order()?;
        self.db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO projects (id, name, root_path, default_branch, created_at, last_opened_at, sort_order) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
                    params![id, name, root_path, default_branch, created_at, sort_order],
                )
            })
            .map_err(|e| classify_unique("projects", "root_path", e))?;
        Ok(Project {
            id,
            name: name.to_string(),
            root_path: root_path.to_string(),
            default_branch: default_branch.to_string(),
            created_at,
            last_opened_at: None,
            sort_order,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<Project>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at, sort_order \
                 FROM projects WHERE id = ?1",
                [id],
                ProjectRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Lookup by absolute path. Used by the project picker's
    /// conflict-then-touch flow when a user re-opens a project older
    /// than the `list_recent(20)` horizon (so the in-memory scan misses).
    pub fn get_by_root_path(&self, root_path: &str) -> Result<Option<Project>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at, sort_order \
                 FROM projects WHERE root_path = ?1",
                [root_path],
                ProjectRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Insert a new project, or — on duplicate `root_path` — silently
    /// touch the existing row's `last_opened_at` and return it.
    ///
    /// Used by the project picker (step 5) so the "Open Folder…" flow
    /// re-promotes already-known paths to the top of the recent list
    /// without surfacing a Conflict to the user. Two SQL writes in the
    /// duplicate case: a failed INSERT (caught as Conflict) then an
    /// UPDATE — acceptable for an interactive-only path.
    pub fn insert_or_touch(
        &self,
        name: &str,
        root_path: &str,
        default_branch: &str,
    ) -> Result<Project, StorageError> {
        match self.insert(name, root_path, default_branch) {
            Ok(project) => Ok(project),
            Err(StorageError::Conflict {
                table, constraint, ..
            }) if table == "projects" && constraint == "root_path" => {
                // Re-fetch existing row; this is the canonical home for
                // the conflict→touch flow so consumers never see SQL.
                let existing = self
                    .get_by_root_path(root_path)?
                    .ok_or(StorageError::Conflict {
                        table: "projects".into(),
                        constraint: "root_path".into(),
                    })?;
                self.update_last_opened_at(&existing.id)?;
                Ok(existing)
            }
            Err(other) => Err(other),
        }
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<Project>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at, sort_order \
                 FROM projects \
                 ORDER BY last_opened_at IS NULL, last_opened_at DESC, created_at DESC \
                 LIMIT ?1",
            )?;
            let iter = stmt.query_map([limit as i64], ProjectRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Projects in manual display order for the left rail: by `sort_order`
    /// ascending, with `last_opened_at` desc then `created_at` desc as a
    /// stable tiebreak for rows sharing a rank (e.g. all-`0.0` before the
    /// first backfill). Unlike [`list_recent`], opening a project does not
    /// reshuffle this order — the rank is sticky until a drag rewrites it.
    pub fn list_ordered(&self, limit: usize) -> Result<Vec<Project>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at, sort_order \
                 FROM projects \
                 ORDER BY sort_order ASC, last_opened_at IS NULL, last_opened_at DESC, created_at DESC \
                 LIMIT ?1",
            )?;
            let iter = stmt.query_map([limit as i64], ProjectRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The append rank for a new project: `max(sort_order) + 1.0`, or `1.0`
    /// for an empty table. Never returns `0.0`, keeping that value reserved
    /// as the not-yet-backfilled sentinel.
    pub fn next_sort_order(&self) -> Result<f64, StorageError> {
        let max: f64 = self.db.with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(MAX(sort_order), 0.0) FROM projects",
                [],
                |row| row.get(0),
            )
        })?;
        Ok(max + 1.0)
    }

    /// Overwrite a project's manual rank. Used by drag-to-reorder, which
    /// computes the midpoint between the drop neighbours. No-op (warns) when
    /// the id does not exist.
    pub fn set_sort_order(&self, id: &str, value: f64) -> Result<(), StorageError> {
        let affected = self.db.with_conn(|c| {
            c.execute(
                "UPDATE projects SET sort_order = ?1 WHERE id = ?2",
                params![value, id],
            )
        })?;
        if affected == 0 {
            tracing::warn!(project_id = %id, "set_sort_order matched no project row");
        }
        Ok(())
    }

    /// Seed `sort_order` for rows still at the `0.0` sentinel, assigning
    /// `1.0, 2.0, …` in the current recency display order (mirroring
    /// [`list_recent`]'s ORDER BY) so the first launch after the migration
    /// shows no visible reshuffle. Idempotent: rows already ranked (>0) are
    /// left untouched, so a second call is a no-op.
    pub fn backfill_sort_order(&self) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM projects WHERE sort_order = 0.0 \
                 ORDER BY last_opened_at IS NULL, last_opened_at DESC, created_at DESC",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (i, id) in ids.iter().enumerate() {
                let rank = (i as f64) + 1.0;
                c.execute(
                    "UPDATE projects SET sort_order = ?1 WHERE id = ?2",
                    params![rank, id],
                )?;
            }
            Ok(())
        })
    }

    pub fn update_last_opened_at(&self, id: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE projects SET last_opened_at = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM projects WHERE id = ?1", [id])
                .map(|_| ())
        })?;
        Ok(())
    }

    /// Reorder the project `moved_id` to sit adjacent to the project currently
    /// at display index `target_index` (the drop target). Writes one midpoint
    /// rank; if the bounding neighbours have collapsed too close, renumbers the
    /// whole list to integers first and retries. No-op when the move resolves
    /// to the same slot or the id is unknown.
    pub fn reorder_to(&self, moved_id: &str, target_index: usize) -> Result<(), StorageError> {
        let list = self.list_ordered(LIST_ALL)?;
        let Some(src) = list.iter().position(|p| p.id == moved_id) else {
            tracing::warn!(project_id = %moved_id, "reorder_to: moved project not found");
            return Ok(());
        };
        if src == target_index || target_index >= list.len() {
            return Ok(());
        }
        let orders: Vec<f64> = list.iter().map(|p| p.sort_order).collect();
        if let Some(v) = super::reorder_slot_value(&orders, src, target_index) {
            return self.set_sort_order(moved_id, v);
        }
        // Collapsed gap: renumber 1.0..N in current display order, then retry
        // against the fresh integer ranks.
        self.normalize_ranks(&list)?;
        let fresh = self.list_ordered(LIST_ALL)?;
        let orders: Vec<f64> = fresh.iter().map(|p| p.sort_order).collect();
        let src = fresh.iter().position(|p| p.id == moved_id).unwrap_or(src);
        if let Some(v) = super::reorder_slot_value(&orders, src, target_index) {
            self.set_sort_order(moved_id, v)?;
        }
        Ok(())
    }

    /// Renumber every project's rank to `1.0, 2.0, …` in the given display
    /// order. Called only when a midpoint would collapse; cheap because the
    /// list is short.
    fn normalize_ranks(&self, list: &[Project]) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            // One transaction so a mid-loop crash can't leave a partial
            // renumber; the whole renumber lands or none of it does.
            let tx = c.unchecked_transaction()?;
            for (i, p) in list.iter().enumerate() {
                tx.execute(
                    "UPDATE projects SET sort_order = ?1 WHERE id = ?2",
                    params![(i as f64) + 1.0, p.id],
                )?;
            }
            tx.commit()
        })
    }
}

/// `list_ordered` limit standing in for "all rows" — the rail never holds
/// anywhere near this many projects.
const LIST_ALL: usize = 1_000_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[test]
    fn insert_assigns_ascending_append_ranks() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a");
        let b = repo.insert("b", "/b", "main").expect("b");
        let c = repo.insert("c", "/c", "main").expect("c");
        assert_eq!(a.sort_order, 1.0);
        assert_eq!(b.sort_order, 2.0);
        assert_eq!(c.sort_order, 3.0);
    }

    #[test]
    fn next_sort_order_is_max_plus_one() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        assert_eq!(repo.next_sort_order().expect("empty"), 1.0);
        repo.insert("a", "/a", "main").expect("a");
        assert_eq!(repo.next_sort_order().expect("after one"), 2.0);
    }

    #[test]
    fn set_sort_order_lands_midpoint_between_neighbours() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a"); // 1.0
        let b = repo.insert("b", "/b", "main").expect("b"); // 2.0
        let _c = repo.insert("c", "/c", "main").expect("c"); // 3.0
        // Move c between a and b.
        repo.set_sort_order(&_c.id, (a.sort_order + b.sort_order) / 2.0)
            .expect("set");
        let ordered = repo.list_ordered(10).expect("ordered");
        let ids: Vec<&str> = ordered.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, [a.id.as_str(), _c.id.as_str(), b.id.as_str()]);
    }

    #[test]
    fn set_sort_order_missing_id_is_noop() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        repo.set_sort_order("nope", 5.0).expect("noop ok");
    }

    #[test]
    fn list_ordered_sorts_by_sort_order_not_recency() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a"); // 1.0
        let b = repo.insert("b", "/b", "main").expect("b"); // 2.0
        // Open b later — must NOT float it above a in list_ordered.
        repo.update_last_opened_at(&b.id).expect("touch");
        let ordered = repo.list_ordered(10).expect("ordered");
        assert_eq!(ordered[0].id, a.id);
        assert_eq!(ordered[1].id, b.id);
    }

    fn ordered_ids(repo: &ProjectRepo) -> Vec<String> {
        repo.list_ordered(100)
            .expect("ordered")
            .into_iter()
            .map(|p| p.id)
            .collect()
    }

    #[test]
    fn reorder_to_moves_item_down_after_target() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a"); // idx 0
        let _b = repo.insert("b", "/b", "main").expect("b"); // idx 1
        let c = repo.insert("c", "/c", "main").expect("c"); // idx 2
        let _d = repo.insert("d", "/d", "main").expect("d"); // idx 3
        // Drag a (idx 0) onto c (idx 2) → a lands after c.
        repo.reorder_to(&a.id, 2).expect("reorder");
        let ids = ordered_ids(&repo);
        assert_eq!(ids, [_b.id, c.id, a.id, _d.id]);
    }

    #[test]
    fn reorder_to_moves_item_up_before_target() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a"); // idx 0
        let b = repo.insert("b", "/b", "main").expect("b"); // idx 1
        let _c = repo.insert("c", "/c", "main").expect("c"); // idx 2
        let d = repo.insert("d", "/d", "main").expect("d"); // idx 3
        // Drag d (idx 3) onto b (idx 1) → d lands before b.
        repo.reorder_to(&d.id, 1).expect("reorder");
        let ids = ordered_ids(&repo);
        assert_eq!(ids, [a.id, d.id, b.id, _c.id]);
    }

    #[test]
    fn reorder_to_first_never_writes_zero_sentinel() {
        // Dragging to the front must not produce sort_order == 0.0, which the
        // launch backfill would mistake for an un-ranked row and re-seed,
        // silently losing the reorder across restart.
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let _a = repo.insert("a", "/a", "main").expect("a"); // 1.0
        let _b = repo.insert("b", "/b", "main").expect("b"); // 2.0
        let c = repo.insert("c", "/c", "main").expect("c"); // 3.0
        repo.reorder_to(&c.id, 0).expect("reorder");
        let after = repo.get_by_id(&c.id).expect("g").expect("s");
        assert!(
            after.sort_order > 0.0,
            "drag-to-first must never write the 0.0 sentinel (got {})",
            after.sort_order
        );
        assert_eq!(ordered_ids(&repo)[0], c.id);
    }

    #[test]
    fn reorder_to_in_place_is_noop() {
        let repo = ProjectRepo::new(open_memory().expect("db"));
        let a = repo.insert("a", "/a", "main").expect("a");
        let before = a.sort_order;
        repo.reorder_to(&a.id, 0).expect("noop");
        assert_eq!(
            repo.get_by_id(&a.id).expect("g").expect("s").sort_order,
            before
        );
    }

    #[test]
    fn reorder_to_survives_collapsed_gap_via_normalize() {
        let db = open_memory().expect("db");
        let repo = ProjectRepo::new(db.clone());
        let a = repo.insert("a", "/a", "main").expect("a");
        let b = repo.insert("b", "/b", "main").expect("b");
        let c = repo.insert("c", "/c", "main").expect("c");
        // Force b and c essentially equal so a midpoint between them collapses.
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE projects SET sort_order = 5.0 WHERE id = ?1",
                params![b.id],
            )?;
            conn.execute(
                "UPDATE projects SET sort_order = 5.0000000001 WHERE id = ?1",
                params![c.id],
            )
            .map(|_| ())
        })
        .expect("collapse");
        // a has rank 1.0 (idx 0); b,c ~5.0. Drag a between b and c (onto b at
        // idx 1, dragging down lands after b). Normalize kicks in and the move
        // still lands cleanly.
        repo.reorder_to(&a.id, 1).expect("reorder");
        let ids = ordered_ids(&repo);
        // a ends up immediately after b.
        assert_eq!(ids[0], b.id);
        assert_eq!(ids[1], a.id);
        assert_eq!(ids[2], c.id);
    }

    #[test]
    fn backfill_seeds_only_zero_rows_from_recency() {
        let db = open_memory().expect("db");
        let repo = ProjectRepo::new(db.clone());
        // Simulate a pre-migration row by forcing sort_order back to 0.0.
        let a = repo.insert("a", "/a", "main").expect("a");
        let b = repo.insert("b", "/b", "main").expect("b");
        db.with_conn(|c| {
            c.execute("UPDATE projects SET sort_order = 0.0", [])
                .map(|_| ())
        })
        .expect("reset");
        // b is more recently opened → should backfill to rank 1.0.
        repo.update_last_opened_at(&b.id).expect("touch b");
        repo.backfill_sort_order().expect("backfill");
        let after_b = repo.get_by_id(&b.id).expect("b").expect("some");
        let after_a = repo.get_by_id(&a.id).expect("a").expect("some");
        assert_eq!(after_b.sort_order, 1.0);
        assert_eq!(after_a.sort_order, 2.0);
        // Second run is a no-op (no 0.0 rows left).
        repo.backfill_sort_order().expect("idempotent");
        assert_eq!(
            repo.get_by_id(&b.id).expect("b").expect("some").sort_order,
            1.0
        );
    }
}
