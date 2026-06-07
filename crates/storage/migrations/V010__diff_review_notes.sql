-- V010: per-line diff review notes. A reviewer annotates diff lines; notes
-- accumulate per file and can be sent to the active agent as a markdown
-- prompt. Persisted so a review survives tab close and app restart.
--
-- The natural key is the (repo, diff_ref, path, side, line) anchor, kept
-- UNIQUE so re-annotating a line edits the existing note rather than
-- duplicating it. `side` is the 'old'/'new' slug from NoteSide; `line` is the
-- 1-based line number on that side. `diff_ref` distinguishes notes on the
-- same file across different diffs of it (worktree vs a commit vs a range).

CREATE TABLE diff_review_notes (
    id          TEXT PRIMARY KEY,
    repo        TEXT NOT NULL,
    diff_ref    TEXT NOT NULL,
    path        TEXT NOT NULL,
    side        TEXT NOT NULL,
    line        INTEGER NOT NULL,
    body        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (repo, diff_ref, path, side, line)
);

CREATE INDEX idx_diff_review_notes_scope
    ON diff_review_notes (repo, diff_ref);
