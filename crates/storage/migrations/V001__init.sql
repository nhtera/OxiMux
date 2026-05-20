-- V001: initial OxiMux schema.
--
-- This file is embedded into the binary via include_str! at compile time
-- (see crates/storage/src/migrations.rs). The runner wraps the whole
-- script in a single transaction, so no embedded BEGIN/COMMIT directives.

-- Projects: root directories the user has opened.
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    root_path       TEXT NOT NULL UNIQUE,
    default_branch  TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    last_opened_at  TEXT
);

-- Workspaces: one git worktree per task inside a project.
-- UNIQUE(project_id, slug) prevents two workspaces sharing the same
-- branch name / worktree path within a project.
CREATE TABLE workspaces (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    slug            TEXT NOT NULL,
    branch          TEXT NOT NULL,
    worktree_path   TEXT NOT NULL,
    status          TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    archived_at     TEXT,
    UNIQUE(project_id, slug)
);

-- Agent sessions: one row per agent launch within a workspace.
-- exit_code + status_detail back AgentStatus payload variants
-- (Done { code }, NeedsApproval(reason), Failed(msg)).
CREATE TABLE agent_sessions (
    id              TEXT PRIMARY KEY,
    workspace_id    TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    adapter_id      TEXT NOT NULL,
    model           TEXT,
    effort          TEXT,
    status          TEXT NOT NULL,
    exit_code       INTEGER,
    status_detail   TEXT,
    started_at      TEXT,
    ended_at        TEXT
);

-- Pane sessions: UI panes (terminal/editor/git/agent) within a workspace.
-- agent_session_id ON DELETE SET NULL — deleting the agent row preserves
-- the pane row (UI may want to surface the dead-agent state).
CREATE TABLE pane_sessions (
    id                TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_session_id  TEXT REFERENCES agent_sessions(id) ON DELETE SET NULL,
    shell_command     TEXT NOT NULL,
    grid_position     TEXT NOT NULL,
    log_path          TEXT,
    created_at        TEXT NOT NULL
);

-- Settings: flat key/value store. Caller enforces <= 64 KiB value size.
CREATE TABLE settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

-- FK support indexes (folded into V001 — see phase-04 step 3 plan Q1).
CREATE INDEX idx_workspaces_project_id
    ON workspaces(project_id);

CREATE INDEX idx_agent_sessions_workspace_id
    ON agent_sessions(workspace_id);

CREATE INDEX idx_pane_sessions_workspace_id
    ON pane_sessions(workspace_id);
