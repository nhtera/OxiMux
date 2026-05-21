-- V003: per-pane relay PTY id storage for survive-across-restart wiring.
--
-- Each row maps a (project, pane-ordinal) pair to a relay-side PTY id
-- and the relay session that minted it. On the next app launch, the
-- reconciliation factory:
--   1. Asks the relay for `ListPtys`.
--   2. For each persisted row whose `relay_session` matches the live
--      session AND whose `relay_pty_id` appears in the live list,
--      attach (the live shell continues).
--   3. Otherwise fall back to a fresh spawn + the visual replay we
--      already keep in `pane_buffers` (V002).
--
-- Why store `relay_session`: when the daemon restarts (crash + respawn
-- by RelaySupervisor), every PTY id minted by the old session is
-- meaningless to the new one. Comparing `relay_session` against the
-- HelloAck.session_id lets reconciliation bulk-delete stale rows in
-- a single round trip instead of one PtyNotFound per pane.
--
-- ON DELETE CASCADE keeps rows in sync with project deletion.
-- `ordinal` indexes the same DFS leaf order used by `pane_buffers`,
-- so the two tables stay aligned without a join.
CREATE TABLE pane_relay_ids (
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    relay_pty_id    TEXT NOT NULL,
    relay_session   TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (project_id, ordinal)
);
