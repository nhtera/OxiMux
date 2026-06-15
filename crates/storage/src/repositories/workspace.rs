//! `WorkspaceRepo` — typed CRUD over the `workspaces` table. The
//! `(project_id, slug)` UNIQUE constraint is the foot-gun guard for the
//! step-6 worktree-add rollback flow.

use oximux_core::Workspace;
use rusqlite::{OptionalExtension, params};

use super::{classify_unique, new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::WorkspaceRow;

#[derive(Clone)]
pub struct WorkspaceRepo {
    db: Db,
}

impl WorkspaceRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Insert a new workspace. Returns [`StorageError::Conflict`] when the
    /// `(project_id, slug)` pair already exists — callers (step 6) should
    /// catch this before invoking `git worktree add` to avoid a half-baked
    /// state.
    pub fn insert(
        &self,
        project_id: &str,
        name: &str,
        slug: &str,
        branch: &str,
        worktree_path: &str,
    ) -> Result<Workspace, StorageError> {
        let id = new_id();
        let created_at = now();
        let status = "active";
        // New workspaces append to the end of their project's manual order.
        let sort_order = self.next_sort_order(project_id)?;
        self.db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO workspaces (id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at, sort_order) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
                    params![id, project_id, name, slug, branch, worktree_path, status, created_at, sort_order],
                )
            })
            .map_err(|e| classify_unique("workspaces", "project_id_slug", e))?;
        Ok(Workspace {
            id,
            project_id: project_id.to_string(),
            name: name.to_string(),
            slug: slug.to_string(),
            branch: branch.to_string(),
            worktree_path: worktree_path.to_string(),
            status: status.to_string(),
            created_at,
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order,
        })
    }

    /// The append rank for a new workspace within its project: per-project
    /// `max(sort_order) + 1.0`, or `1.0` for the project's first row. Each
    /// project has an independent rank space; `0.0` stays reserved as the
    /// not-yet-backfilled sentinel.
    pub fn next_sort_order(&self, project_id: &str) -> Result<f64, StorageError> {
        let max: f64 = self.db.with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(MAX(sort_order), 0.0) FROM workspaces WHERE project_id = ?1",
                [project_id],
                |row| row.get(0),
            )
        })?;
        Ok(max + 1.0)
    }

    /// Overwrite a workspace's manual rank (drag-to-reorder midpoint). No-op
    /// (warns) when the id does not exist.
    pub fn set_sort_order(&self, id: &str, value: f64) -> Result<(), StorageError> {
        let affected = self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET sort_order = ?1 WHERE id = ?2",
                params![value, id],
            )
        })?;
        if affected == 0 {
            tracing::warn!(workspace_id = %id, "set_sort_order matched no workspace row");
        }
        Ok(())
    }

    /// Seed `sort_order` for workspace rows still at the `0.0` sentinel,
    /// assigning `1.0, 2.0, …` per project in creation order so the first
    /// launch after the migration shows no visible reshuffle. Idempotent:
    /// already-ranked rows (>0) are skipped, so a second call is a no-op.
    pub fn backfill_sort_order(&self) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, project_id FROM workspaces WHERE sort_order = 0.0 \
                 ORDER BY project_id ASC, created_at ASC",
            )?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // Rank resets to 1.0 at each project boundary (rows are grouped by
            // project_id in the query).
            let mut current_project: Option<String> = None;
            let mut rank = 0.0_f64;
            for (id, project_id) in &rows {
                if current_project.as_deref() != Some(project_id.as_str()) {
                    current_project = Some(project_id.clone());
                    rank = 0.0;
                }
                rank += 1.0;
                c.execute(
                    "UPDATE workspaces SET sort_order = ?1 WHERE id = ?2",
                    params![rank, id],
                )?;
            }
            Ok(())
        })
    }

    /// Set (or clear) a workspace's identifier-hue swatch slug (e.g. `"blue"`).
    /// `None` clears the tint back to the default. Warns if no row matched.
    pub fn set_tint(&self, id: &str, tint: Option<&str>) -> Result<(), StorageError> {
        let affected = self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET tint = ?1 WHERE id = ?2",
                params![tint, id],
            )
        })?;
        if affected == 0 {
            tracing::warn!(workspace_id = %id, "set_tint matched no workspace row");
        }
        Ok(())
    }

    /// Set (or clear) the GitHub issue/PR reference a workspace links back to.
    /// Used by the Tasks page's "create workspace from this" action after the
    /// row is inserted; a no-op `None` clears it.
    pub fn set_linked_issue(
        &self,
        id: &str,
        linked_issue: Option<&str>,
    ) -> Result<(), StorageError> {
        let affected = self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET linked_issue = ?1 WHERE id = ?2",
                params![linked_issue, id],
            )
        })?;
        // No matching row means the contract ("the write happened") was not
        // met — surface it rather than reporting a silent success.
        if affected == 0 {
            tracing::warn!(workspace_id = %id, "set_linked_issue matched no workspace row");
        }
        Ok(())
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<Workspace>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at, linked_issue, tint, sort_order \
                 FROM workspaces WHERE id = ?1",
                [id],
                WorkspaceRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Resolve the active (non-archived) workspace owning a worktree path.
    /// Used by the agent-session persistence path to key session rows by
    /// workspace id given only the launch cwd. Newest first when a path
    /// somehow has two rows (should not happen; defensive).
    pub fn get_by_worktree_path(&self, path: &str) -> Result<Option<Workspace>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at, linked_issue, tint, sort_order \
                 FROM workspaces \
                 WHERE worktree_path = ?1 AND archived_at IS NULL \
                 ORDER BY created_at DESC LIMIT 1",
                [path],
                WorkspaceRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// List active (non-archived) workspaces for a project, newest first.
    pub fn list_for_project(&self, project_id: &str) -> Result<Vec<Workspace>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at, linked_issue, tint, sort_order \
                 FROM workspaces \
                 WHERE project_id = ?1 AND archived_at IS NULL \
                 ORDER BY created_at DESC",
            )?;
            let iter = stmt.query_map([project_id], WorkspaceRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub fn mark_archived(&self, id: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET archived_at = ?1, status = 'archived' WHERE id = ?2",
                params![ts, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn rename(&self, id: &str, new_name: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET name = ?1 WHERE id = ?2",
                params![new_name, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Delete a workspace row. FK cascade removes `pane_sessions` and
    /// `agent_sessions` for the workspace. Call this in the error branch
    /// of `git worktree add` to keep DB and disk state consistent.
    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM workspaces WHERE id = ?1", [id])
                .map(|_| ())
        })?;
        Ok(())
    }

    /// Active workspaces for a project in manual display order (`sort_order`
    /// ascending, `created_at` as a stable tiebreak). The reorder logic walks
    /// this list; it excludes the synthesized primary row (which only exists
    /// in-memory in the rail), so every entry is a real, reorderable row.
    pub fn list_ordered_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<Workspace>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at, linked_issue, tint, sort_order \
                 FROM workspaces \
                 WHERE project_id = ?1 AND archived_at IS NULL \
                 ORDER BY sort_order ASC, created_at ASC",
            )?;
            let iter = stmt.query_map([project_id], WorkspaceRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Reorder `moved_id` to sit where `target_id` currently is within the same
    /// project group, writing one midpoint rank. Renumbers the group to
    /// integers and retries if the bounding neighbours have collapsed. No-op
    /// when either id is missing from the group or the move is in place.
    pub fn reorder_to_target(
        &self,
        moved_id: &str,
        target_id: &str,
        project_id: &str,
    ) -> Result<(), StorageError> {
        let list = self.list_ordered_for_project(project_id)?;
        let (Some(src), Some(tgt)) = (
            list.iter().position(|w| w.id == moved_id),
            list.iter().position(|w| w.id == target_id),
        ) else {
            tracing::warn!(%moved_id, %target_id, "reorder_to_target: id not in group");
            return Ok(());
        };
        if src == tgt {
            return Ok(());
        }
        let orders: Vec<f64> = list.iter().map(|w| w.sort_order).collect();
        if let Some(v) = super::reorder_slot_value(&orders, src, tgt) {
            return self.set_sort_order(moved_id, v);
        }
        // Collapsed gap: renumber 1.0..N then retry against fresh ranks.
        self.normalize_ranks(&list)?;
        let fresh = self.list_ordered_for_project(project_id)?;
        let orders: Vec<f64> = fresh.iter().map(|w| w.sort_order).collect();
        let src = fresh.iter().position(|w| w.id == moved_id).unwrap_or(src);
        let tgt = fresh.iter().position(|w| w.id == target_id).unwrap_or(tgt);
        if let Some(v) = super::reorder_slot_value(&orders, src, tgt) {
            self.set_sort_order(moved_id, v)?;
        }
        Ok(())
    }

    /// Renumber a project's workspace ranks to `1.0, 2.0, …` in the given
    /// display order. Called only when a midpoint would collapse.
    fn normalize_ranks(&self, list: &[Workspace]) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            // One transaction so a mid-loop crash can't leave a partial
            // renumber; the whole renumber lands or none of it does.
            let tx = c.unchecked_transaction()?;
            for (i, w) in list.iter().enumerate() {
                tx.execute(
                    "UPDATE workspaces SET sort_order = ?1 WHERE id = ?2",
                    params![(i as f64) + 1.0, w.id],
                )?;
            }
            tx.commit()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;
    use crate::repositories::ProjectRepo;

    fn project(db: &crate::db::Db) -> String {
        ProjectRepo::new(db.clone())
            .insert("p", "/p", "main")
            .expect("project")
            .id
    }

    fn ws(repo: &WorkspaceRepo, project_id: &str, slug: &str) -> Workspace {
        repo.insert(project_id, slug, slug, "main", &format!("/p/{slug}"))
            .expect("ws")
    }

    #[test]
    fn insert_appends_per_project_ranks() {
        let db = open_memory().expect("db");
        let p = project(&db);
        let repo = WorkspaceRepo::new(db);
        assert_eq!(ws(&repo, &p, "a").sort_order, 1.0);
        assert_eq!(ws(&repo, &p, "b").sort_order, 2.0);
    }

    #[test]
    fn next_sort_order_is_per_project() {
        let db = open_memory().expect("db");
        let p1 = project(&db);
        let p2 = ProjectRepo::new(db.clone())
            .insert("p2", "/p2", "main")
            .expect("p2")
            .id;
        let repo = WorkspaceRepo::new(db);
        ws(&repo, &p1, "a"); // p1 → 1.0
        ws(&repo, &p1, "b"); // p1 → 2.0
        // p2 has its own rank space starting at 1.0.
        assert_eq!(repo.next_sort_order(&p2).expect("p2"), 1.0);
        assert_eq!(repo.next_sort_order(&p1).expect("p1"), 3.0);
    }

    #[test]
    fn set_sort_order_reorders_via_get_by_id() {
        let db = open_memory().expect("db");
        let p = project(&db);
        let repo = WorkspaceRepo::new(db);
        let a = ws(&repo, &p, "a");
        repo.set_sort_order(&a.id, 9.5).expect("set");
        assert_eq!(
            repo.get_by_id(&a.id).expect("get").expect("some").sort_order,
            9.5
        );
    }

    fn ordered_ids(repo: &WorkspaceRepo, project_id: &str) -> Vec<String> {
        repo.list_ordered_for_project(project_id)
            .expect("ordered")
            .into_iter()
            .map(|w| w.id)
            .collect()
    }

    #[test]
    fn reorder_to_target_moves_down() {
        let db = open_memory().expect("db");
        let p = project(&db);
        let repo = WorkspaceRepo::new(db);
        let a = ws(&repo, &p, "a"); // 1.0
        let b = ws(&repo, &p, "b"); // 2.0
        let c = ws(&repo, &p, "c"); // 3.0
        // Drag a onto c → a lands after c.
        repo.reorder_to_target(&a.id, &c.id, &p).expect("reorder");
        assert_eq!(ordered_ids(&repo, &p), [b.id, c.id, a.id]);
    }

    #[test]
    fn reorder_to_target_moves_up() {
        let db = open_memory().expect("db");
        let p = project(&db);
        let repo = WorkspaceRepo::new(db);
        let a = ws(&repo, &p, "a"); // 1.0
        let b = ws(&repo, &p, "b"); // 2.0
        let c = ws(&repo, &p, "c"); // 3.0
        // Drag c onto a → c lands before a.
        repo.reorder_to_target(&c.id, &a.id, &p).expect("reorder");
        assert_eq!(ordered_ids(&repo, &p), [c.id, a.id, b.id]);
    }

    #[test]
    fn reorder_to_target_first_never_writes_zero_sentinel() {
        // Same sentinel hazard as projects: dragging a row to the front of its
        // group must keep sort_order strictly positive.
        let db = open_memory().expect("db");
        let p = project(&db);
        let repo = WorkspaceRepo::new(db);
        let a = ws(&repo, &p, "a"); // 1.0
        let _b = ws(&repo, &p, "b"); // 2.0
        let c = ws(&repo, &p, "c"); // 3.0
        repo.reorder_to_target(&c.id, &a.id, &p).expect("reorder");
        let after = repo.get_by_id(&c.id).expect("g").expect("s");
        assert!(
            after.sort_order > 0.0,
            "drag-to-first must never write the 0.0 sentinel (got {})",
            after.sort_order
        );
        assert_eq!(ordered_ids(&repo, &p)[0], c.id);
    }

    #[test]
    fn reorder_to_target_is_per_project_isolated() {
        let db = open_memory().expect("db");
        let p1 = project(&db);
        let p2 = ProjectRepo::new(db.clone())
            .insert("p2", "/p2", "main")
            .expect("p2")
            .id;
        let repo = WorkspaceRepo::new(db);
        let a = ws(&repo, &p1, "a");
        let b = ws(&repo, &p1, "b");
        let x = ws(&repo, &p2, "x");
        // A target in another project is "not in group" → no-op.
        repo.reorder_to_target(&a.id, &x.id, &p1).expect("noop");
        assert_eq!(ordered_ids(&repo, &p1), [a.id, b.id]);
        assert_eq!(ordered_ids(&repo, &p2), [x.id]);
    }

    #[test]
    fn backfill_seeds_per_project_in_creation_order() {
        let db = open_memory().expect("db");
        let p1 = project(&db);
        let p2 = ProjectRepo::new(db.clone())
            .insert("p2", "/p2", "main")
            .expect("p2")
            .id;
        let repo = WorkspaceRepo::new(db.clone());
        let a = ws(&repo, &p1, "a");
        let b = ws(&repo, &p1, "b");
        let c = ws(&repo, &p2, "c");
        db.with_conn(|conn| {
            conn.execute("UPDATE workspaces SET sort_order = 0.0", [])
                .map(|_| ())
        })
        .expect("reset");
        repo.backfill_sort_order().expect("backfill");
        // p1: a then b → 1.0, 2.0; p2: c → 1.0 (independent rank space).
        assert_eq!(
            repo.get_by_id(&a.id).expect("a").expect("s").sort_order,
            1.0
        );
        assert_eq!(
            repo.get_by_id(&b.id).expect("b").expect("s").sort_order,
            2.0
        );
        assert_eq!(
            repo.get_by_id(&c.id).expect("c").expect("s").sort_order,
            1.0
        );
    }
}
