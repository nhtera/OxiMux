-- V002: per-pane scrollback buffer storage for terminal-tab restore.
--
-- Each row holds one captured ANSI-byte buffer (output of the
-- grid_serializer) for a single terminal leaf inside a project's tabs
-- snapshot. `ordinal` indexes the DFS leaf order within the snapshot;
-- the restore path walks the tabs in the same order and pairs each leaf
-- with the same ordinal so bytes line up without storing extra IDs.
--
-- ON DELETE CASCADE keeps buffers in sync when a project is deleted.
-- updated_at is informational only — for prune policies, telemetry.
CREATE TABLE pane_buffers (
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    bytes           BLOB NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (project_id, ordinal)
);
