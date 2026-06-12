-- V013: drop the FOREIGN KEY on agent_sessions.workspace_id.
--
-- V001 modeled this column as `REFERENCES workspaces(id) ON DELETE
-- CASCADE` on the assumption that every agent launch happens inside a
-- DB-backed workspace row. In practice the most common launch target is
-- the project's repo-root "primary" workspace, which is synthesized at
-- render time (id = 'primary:<project_id>') and has no `workspaces`
-- row — so every insert for it would trip the FK and the persistence
-- path stayed dead (same failure V007 fixed for worktree_settings).
--
-- The column is now a free-form TEXT key: either a real workspaces.id
-- UUID or a synthesized 'primary:<project_id>'. ON DELETE CASCADE is
-- dropped with the FK; orphan rows from deleted workspaces are accepted
-- (a few hundred bytes per archived workspace at worst).
--
-- SQLite has no ALTER TABLE DROP CONSTRAINT — the table is rebuilt via
-- the standard rename-swap dance. pane_sessions.agent_session_id
-- references this table by name and re-binds to the renamed table; the
-- runner wraps the migration in one transaction, so this is atomic.

CREATE TABLE agent_sessions_new (
    id              TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL,
    adapter_id      TEXT NOT NULL,
    model           TEXT,
    effort          TEXT,
    status          TEXT NOT NULL,
    exit_code       INTEGER,
    status_detail   TEXT,
    started_at      TEXT,
    ended_at        TEXT
);

INSERT INTO agent_sessions_new (id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at)
SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at
FROM agent_sessions;

DROP TABLE agent_sessions;

ALTER TABLE agent_sessions_new RENAME TO agent_sessions;

CREATE INDEX idx_agent_sessions_workspace_id
    ON agent_sessions(workspace_id);
