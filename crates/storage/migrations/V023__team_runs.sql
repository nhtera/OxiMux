-- V023: team runs — a multi-role fan-out recorded host-side so it survives a
-- restart. The differentiator over a client-side script: a host that comes back
-- re-associates its live sessions to the roles still open, rather than losing
-- the run with the process that started it.
--
-- Two tables rather than one JSON column: `team_run_roles` is written one row
-- at a time (each role reports independently, from its own session), and a
-- read-modify-write of a blob would drop a concurrent report.

CREATE TABLE team_runs (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    -- The project root every role's session opened in (or based its worktree
    -- on). Kept on the run so a restart can re-derive context without reading
    -- each role's session.
    cwd         TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE team_run_roles (
    run_id      TEXT NOT NULL REFERENCES team_runs(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    -- NULL when the role's session could not be started; `status` says why.
    session_id  TEXT,
    -- 'running' | 'done' | 'failed'. Text rather than an integer so a database
    -- opened by hand reads as what it is.
    status      TEXT NOT NULL,
    summary     TEXT,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (run_id, name)
);

-- The status board's only query: every role of one run.
CREATE INDEX idx_team_run_roles_run ON team_run_roles(run_id);
-- Boot's re-association pass walks the roles still open across every run.
CREATE INDEX idx_team_run_roles_status ON team_run_roles(status);
