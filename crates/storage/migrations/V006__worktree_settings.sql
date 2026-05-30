-- V006: per-worktree SCM settings (one row per workspace).
--
-- Keyed by `workspace_id` to keep `workspaces` cleanly identity-only.
-- All payload columns are nullable; absence means "fall back to global
-- default" (the inline-composer reads `commit_draft IS NULL` as "no
-- draft", the base-ref picker reads `base_ref IS NULL` as "use repo
-- default", and the view-mode toggle reads `view_mode_override IS NULL`
-- as "use the global `scm_view_mode` setting").
--
-- ON DELETE CASCADE: archiving / deleting a workspace also discards its
-- SCM scratch state. There is no second source of truth for these
-- fields; nothing else needs them after the workspace is gone.

CREATE TABLE worktree_settings (
    workspace_id        TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    base_ref            TEXT,
    commit_draft        TEXT,
    view_mode_override  TEXT,
    updated_at          TEXT NOT NULL
);
