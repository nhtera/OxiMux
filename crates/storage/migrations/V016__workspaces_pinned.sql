-- Per-workspace pin flag. A pinned workspace floats to the top of its
-- project group in every sort mode (Smart, Recent, Manual), so users who
-- stay in an automatic sort still have a way to keep important worktrees
-- in reach. Additive column, DEFAULT 0 (= unpinned) so no backfill is
-- needed and existing rows are untouched.
ALTER TABLE workspaces ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
