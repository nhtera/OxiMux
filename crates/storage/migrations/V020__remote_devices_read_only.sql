-- V020: per-device read-only tier for remote control.
--
-- Pairing grants FULL access by default (the confirmed product decision), so a
-- paired phone can drive agents and write to the repository. `read_only` is the
-- opt-down: the desktop user marks a specific device view-only, and the host
-- refuses every state-changing RPC from it (prompts, steering, cancel, permission
-- decisions, and — once they ship — git writes and terminal keystrokes) while
-- still serving transcripts, status, and diffs.
--
-- Orthogonal to `scope`: a device can be session-scoped AND read-only. Kept as a
-- separate column rather than another `scope` kind so the two don't multiply.
--
-- DEFAULT 0 preserves the behavior of every device paired before this migration:
-- they stay read-write, so the upgrade changes nothing until a user opts one down.

ALTER TABLE remote_devices ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
