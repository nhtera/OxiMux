-- V033: what became of each send, and how often a failed turn was retried.
--
-- Both survive a restart, which is the point: after a crash the host must know
-- which sends the agent may already have (never resend those on its own), and
-- a retry counter that reset on every launch would let a rate-limited turn
-- retry forever. Owned by the host core; read by the desktop and, later, by a
-- headless host that has no transcript blob to keep them in.
--
-- Immutable once merged.

-- One row per send, keyed by a hash of its idempotency key: a desktop send's
-- gate id (kind 'gate') or a remote caller's key (kind 'v2'). Hashes only;
-- the text of a send is never stored here.
CREATE TABLE send_receipts (
    session_id    TEXT NOT NULL,
    key_hash      TEXT NOT NULL,
    -- 'pending'  reserved, written or about to be, no proof yet;
    -- 'accepted' the agent's CLI took it (it is in the agent's session): never
    --            resend it verbatim. A CLI acknowledges every line it reads,
    --            before the provider is asked, so this alone is not acceptance
    --            by the provider;
    -- 'rejected' the provider refused the turn before producing anything (an
    --            overload or rate-limit status and no reply or tool call after
    --            the prompt): safe to resend verbatim. Overrides 'accepted',
    --            since that acknowledgement comes first;
    -- 'unknown'  written, then the host went away before any proof: the agent
    --            may or may not have it, so nothing resends it on its own.
    state         TEXT NOT NULL,
    -- The host instance that wrote the row. On start a host turns every
    -- 'pending' row another instance left into 'unknown'.
    host_instance TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (session_id, key_hash)
);

-- Pruning by age (rows older than a day).
CREATE INDEX idx_send_receipts_created ON send_receipts(created_at);

-- The automatic retries of one failed turn. The attempt count lives here, so
-- neither a restart nor re-arming resets it.
CREATE TABLE turn_retries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    -- The failed turn: the receipt key hash of the send that started it. Not a
    -- per-process turn ordinal, which a restart would renumber.
    turn_key        TEXT NOT NULL,
    -- 'window' | 'overload': what the provider said (the retry policy's class).
    class           TEXT NOT NULL,
    -- The short reason shown to the user ("Usage limit reached").
    reason          TEXT NOT NULL,
    -- Automatic attempts already fired for this turn.
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- When the turn first failed (again when a cancelled retry is re-armed).
    first_failed_at TEXT NOT NULL,
    -- When the armed retry fires (unix ms, like every wake time the retry
    -- policy computes); NULL while none is armed.
    next_wake_at_ms INTEGER,
    -- NULL while live; then 'succeeded' | 'exhausted' | 'cancelled' |
    -- 'superseded' (the user sent something new). A live row a week old was
    -- abandoned (its chat closed) and is pruned with the resolved ones.
    resolved        TEXT,
    resolved_at     TEXT,
    host_instance   TEXT NOT NULL,
    UNIQUE (session_id, turn_key)
);

-- At most one live retry per session.
CREATE UNIQUE INDEX idx_turn_retries_live ON turn_retries(session_id) WHERE resolved IS NULL;
