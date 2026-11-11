-- V034: what a person has already seen of a session, and how loudly it may
-- speak.
--
-- Attention is derived from a session's health, but whether it still *needs
-- you* is a fact about the person, not the agent: a failure you have looked at
-- is no longer an alert, a session you want to come back to is unread however
-- healthy it is, and a chatty child can be muted. None of that is derivable
-- from the agent, so it is persisted here and survives a restart — otherwise a
-- relaunch would replay every old failure into the badge.
--
-- Immutable once merged: `collapsed` is written by the subagent tree (a later
-- phase) and ships now, because adding a column later costs another migration.

CREATE TABLE agent_session_prefs (
    -- Whose prefs: 'chat:<id>' for a chat session, 'cockpit:<agent_sessions.id>'
    -- for a terminal agent, 'team:<run>' / 'plan:<run>' for a run's root row.
    -- A chat that rekeys (Codex names its own session after launch) carries its
    -- prefs over in the same transaction as its lineage.
    session_key     TEXT PRIMARY KEY,
    -- When the person last had this session in front of them, RFC-3339. An
    -- alert whose edge is older than this has been seen; a newer one has not.
    -- Dismiss writes it too, so there is no second "dismissed_at" to reconcile.
    last_viewed_at  TEXT,
    -- "Mark as unread": the person wants it to keep standing out even though
    -- they have viewed it. Cleared by the next send to the session.
    unread_override INTEGER NOT NULL DEFAULT 0,
    -- 'default' | 'all' | 'needs_you' | 'muted'. The notification mode this
    -- session's alerts obey; the Settings rows that drive it come later.
    notify_mode     TEXT NOT NULL DEFAULT 'default',
    -- The session's subtree is folded shut in the rail.
    collapsed       INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL
);
