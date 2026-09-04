-- V026: give a worktree an agent-writable status line and a work phase.
--
-- Several agents running at once are only legible by reading each transcript.
-- These two columns let an agent say what it is doing and where it is, so a
-- listing answers "what is happening" without opening anything.
--
-- `comment` is a snapshot, not a log: last write wins, no history table. The
-- question it answers is "what is this worktree doing *now*", and a history
-- would need pruning, paging, and a retention rule to answer the same thing.
--
-- `phase` is a closed vocabulary (todo / in-progress / in-review / done)
-- stored as TEXT rather than an integer ordinal, so a value written by a newer
-- peer survives a round trip through an older one instead of being renumbered
-- into a different meaning. Readers that do not recognise a value show no
-- phase; only writers validate. Empty string = unset, which is why both
-- columns are NOT NULL DEFAULT '' rather than nullable: "no comment" and "no
-- phase" are one state each, and a NULL/'' pair would be two spellings of it.
--
-- Additive with defaults, matching V011/V012/V015/V016 on this same table —
-- every existing row reads as unset, which is what it is.

ALTER TABLE workspaces ADD COLUMN comment TEXT NOT NULL DEFAULT '';
ALTER TABLE workspaces ADD COLUMN phase TEXT NOT NULL DEFAULT '';
