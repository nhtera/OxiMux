-- V030: two side tables for external worktree discovery.
--
-- `workspace_adoptions` records that a workspace row was ADOPTED — created
-- from a worktree that already existed on disk, made by `git worktree add`
-- in a terminal or by another tool — rather than provisioned by OxiMux. The
-- distinction matters after adoption, not only at it: every later script
-- action resolves `.oximux/scripts.toml` from the row's directory, and
-- `Delete` runs that directory's cleanup script with no consent step. A
-- directory somebody else set up may carry scripts the user has never read,
-- so an adopted row is `unvetted` until the user clears it explicitly. The
-- create path never inserts here; a row with no adoption row was minted by
-- OxiMux and is vetted by construction.
--
-- `project_prefs` holds the one per-project preference this needs — whether
-- to hide the "untracked worktrees" group for a repo where it is noise. A
-- side table rather than a `projects` column because `Project` is a wire
-- and snapshot type constructed in many places; a preference that only the
-- rail reads does not belong in every one of them. Absence means the default
-- (`hide_untracked = 0`).
--
-- Both cascade: an adoption is meaningless without its workspace, and a
-- preference without its project. Neither has a second source of truth.

CREATE TABLE workspace_adoptions (
    workspace_id  TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    adopted_at    TEXT NOT NULL,
    unvetted      INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE project_prefs (
    project_id      TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    hide_untracked  INTEGER NOT NULL DEFAULT 0
);
