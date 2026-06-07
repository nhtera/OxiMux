-- V009: remember the last-used model + reasoning-effort per agent adapter
-- so the launch picker can preselect the user's previous choice on the next
-- spawn. One row per adapter slug (`claude-code`, `codex`, …). `model` and
-- `effort` are nullable — NULL means "adapter default, pass no flag", which
-- is the same safe behaviour the picker had before this feature.

CREATE TABLE agent_last_params (
    adapter_id  TEXT PRIMARY KEY,
    model       TEXT,
    effort      TEXT,
    updated_at  TEXT NOT NULL
);
