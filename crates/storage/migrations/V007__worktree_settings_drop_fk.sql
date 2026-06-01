-- V007: drop the FOREIGN KEY on worktree_settings.workspace_id.
--
-- V006 modeled this column as `REFERENCES workspaces(id) ON DELETE
-- CASCADE` on the assumption that the SCM panel would scope its
-- scratch state per workspace UUID. In practice the panel mounts at
-- project level (one row per worktree path, with no `workspaces` row
-- backing it on a freshly-opened project), so every upsert triggers
-- FOREIGN KEY constraint failed and the persistence path is dead.
--
-- The column is now a free-form TEXT key holding the worktree's
-- absolute path. ON DELETE CASCADE is dropped with the FK: orphan
-- rows are accepted (a few hundred bytes at worst per archived
-- worktree).
--
-- SQLite has no ALTER TABLE DROP CONSTRAINT — the table is rebuilt
-- via the standard rename-swap dance. The runner already wraps each
-- migration in its own transaction, so this is atomic.

CREATE TABLE worktree_settings_new (
    workspace_id        TEXT PRIMARY KEY,
    base_ref            TEXT,
    commit_draft        TEXT,
    view_mode_override  TEXT,
    updated_at          TEXT NOT NULL
);

INSERT INTO worktree_settings_new (workspace_id, base_ref, commit_draft, view_mode_override, updated_at)
SELECT workspace_id, base_ref, commit_draft, view_mode_override, updated_at
FROM worktree_settings;

DROP TABLE worktree_settings;

ALTER TABLE worktree_settings_new RENAME TO worktree_settings;
