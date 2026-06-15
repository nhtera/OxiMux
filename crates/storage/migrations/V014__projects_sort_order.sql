-- Manual display rank for the project list in the left rail. A sparse REAL
-- so reorders write a single midpoint value instead of renumbering the whole
-- list. DEFAULT 0 marks a not-yet-ranked row; a one-shot Rust backfill seeds
-- existing rows from their current recency order so nothing visibly moves on
-- the first launch after this migration.
ALTER TABLE projects ADD COLUMN sort_order REAL NOT NULL DEFAULT 0;
