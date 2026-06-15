-- Manual display rank for workspace rows within their project group. Same
-- sparse-REAL scheme as projects, but the rank space is per-project (each
-- group renumbers independently). DEFAULT 0 marks a not-yet-ranked row; a
-- one-shot Rust backfill seeds existing rows from creation order so nothing
-- visibly moves on the first launch after this migration.
ALTER TABLE workspaces ADD COLUMN sort_order REAL NOT NULL DEFAULT 0;
