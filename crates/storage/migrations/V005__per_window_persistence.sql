-- V005: per-window persistence keying for pane_buffers and pane_relay_ids.
--
-- Two app windows sharing the same project must not clobber each other's
-- saved scrollback or relay PTY ids. This migration adds `window_id TEXT
-- NOT NULL DEFAULT 'main'` to the PRIMARY KEY of both tables so each
-- window's state lives in its own slice. The default value 'main' matches
-- the legacy single-window persist_id, keeping pre-upgrade rows loadable
-- by the existing single-window code path without any data loss.
--
-- SQLite cannot alter a primary key in place, so each table is rebuilt
-- via the standard rename-rebuild dance: create <name>_v5, copy existing
-- rows with window_id='main', drop the old table, rename v5 into place.

CREATE TABLE pane_buffers_v5 (
    project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    window_id         TEXT NOT NULL DEFAULT 'main',
    ordinal           INTEGER NOT NULL,
    sub_pane_ordinal  INTEGER NOT NULL DEFAULT 0,
    bytes             BLOB NOT NULL,
    updated_at        TEXT NOT NULL,
    PRIMARY KEY (project_id, window_id, ordinal, sub_pane_ordinal)
);

INSERT INTO pane_buffers_v5
    (project_id, window_id, ordinal, sub_pane_ordinal, bytes, updated_at)
SELECT project_id, 'main', ordinal, sub_pane_ordinal, bytes, updated_at
FROM pane_buffers;

DROP TABLE pane_buffers;
ALTER TABLE pane_buffers_v5 RENAME TO pane_buffers;

CREATE TABLE pane_relay_ids_v5 (
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    window_id     TEXT NOT NULL DEFAULT 'main',
    ordinal       INTEGER NOT NULL,
    relay_pty_id  TEXT NOT NULL,
    relay_session TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (project_id, window_id, ordinal)
);

INSERT INTO pane_relay_ids_v5
    (project_id, window_id, ordinal, relay_pty_id, relay_session, created_at)
SELECT project_id, 'main', ordinal, relay_pty_id, relay_session, created_at
FROM pane_relay_ids;

DROP TABLE pane_relay_ids;
ALTER TABLE pane_relay_ids_v5 RENAME TO pane_relay_ids;
