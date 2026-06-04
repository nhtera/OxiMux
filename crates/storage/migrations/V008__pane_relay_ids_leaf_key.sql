-- V008: per-leaf relay PTY id keying for split / multi-tab terminals.
--
-- Pre-V008 stored one relay id per terminal tab, keyed
-- (project_id, window_id, ordinal) — only the active leaf's active view.
-- Split leaves (Cmd+D) and background per-pane tabs had no relay id, so
-- the tree restore path could never re-attach their surviving daemon
-- PTYs; it always fell back to a fresh in-process spawn, and those
-- shells died on quit. This migration widens the key with `sub_pane` and
-- `tab` so every live leaf-tab persists its own relay id and re-attaches
-- independently on the next launch.
--
-- `sub_pane` is the leaf's DFS position (same order `pane_buffers` uses,
-- so the two tables stay aligned without a join). `tab` is the leaf's
-- per-pane tab index. Legacy rows copy forward with sub_pane=0, tab=0 —
-- identical to the single-leaf/single-tab behavior they already had.
--
-- SQLite cannot alter a primary key in place, so the table is rebuilt
-- via the standard rename-rebuild dance: create _v8, copy rows, drop, rename.

CREATE TABLE pane_relay_ids_v8 (
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    window_id     TEXT NOT NULL DEFAULT 'main',
    ordinal       INTEGER NOT NULL,
    sub_pane      INTEGER NOT NULL DEFAULT 0,
    tab           INTEGER NOT NULL DEFAULT 0,
    relay_pty_id  TEXT NOT NULL,
    relay_session TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    PRIMARY KEY (project_id, window_id, ordinal, sub_pane, tab)
);

INSERT INTO pane_relay_ids_v8
    (project_id, window_id, ordinal, sub_pane, tab, relay_pty_id, relay_session, created_at)
SELECT project_id, window_id, ordinal, 0, 0, relay_pty_id, relay_session, created_at
FROM pane_relay_ids;

DROP TABLE pane_relay_ids;
ALTER TABLE pane_relay_ids_v8 RENAME TO pane_relay_ids;
