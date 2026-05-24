-- V004: per-sub-pane scrollback dimension for pane_buffers.
--
-- F3.4 introduces dormant PTYs with per-sub-pane scrollback. The pre-v4
-- table held one row per terminal tab (only the active sub-pane's
-- scrollback was captured). This migration adds the `sub_pane_ordinal`
-- column and extends the primary key so each leaf inside a multi-
-- sub-pane tab keeps its own buffer.
--
-- Legacy rows resolve to `sub_pane_ordinal = 0`, matching how pre-v4
-- snapshots captured only the active sub-pane. SQLite cannot rebuild a
-- primary key in place, so this migration rebuilds the table and copies
-- existing rows with the default sub-pane ordinal.

CREATE TABLE pane_buffers_v4 (
    project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    ordinal           INTEGER NOT NULL,
    sub_pane_ordinal  INTEGER NOT NULL DEFAULT 0,
    bytes             BLOB NOT NULL,
    updated_at        TEXT NOT NULL,
    PRIMARY KEY (project_id, ordinal, sub_pane_ordinal)
);

INSERT INTO pane_buffers_v4 (project_id, ordinal, sub_pane_ordinal, bytes, updated_at)
SELECT project_id, ordinal, 0, bytes, updated_at FROM pane_buffers;

DROP TABLE pane_buffers;
ALTER TABLE pane_buffers_v4 RENAME TO pane_buffers;
