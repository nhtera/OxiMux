-- V032: who spawned whom, and the events a child owes its parent.
--
-- Agents may start child agents (the orchestration grant). The host has to
-- know the tree to cap it, to let a parent reach only its own subtree, and to
-- tell the parent when a child finishes or gets stuck. All of that must
-- survive a restart, so it lives here rather than in a dispatcher's memory.
--
-- Immutable once merged: every column the orchestration phases read is here
-- now (posture, notify_parent and identity_slot included), because adding one
-- later costs another migration and a backfill for rows already written.

CREATE TABLE agent_lineage (
    -- AUTOINCREMENT: a deleted reservation's id is never handed out again, so
    -- a launch that outlived its reservation cannot finalize someone else's.
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    -- NULL while `reserved` for a provider that names its own session after
    -- launch (Codex, Pi). Claude children are pre-assigned their id, so theirs
    -- is set at reservation. Rewritten only by a rekey.
    session_id           TEXT UNIQUE,
    -- Never NULL, so UNIQUE(parent_id, request_id) dedupes every row:
    -- '-' for a root the operator started, 'team:<run>' / 'plan:<run>' for the
    -- roles of a run, otherwise the parent's session id.
    parent_id            TEXT NOT NULL,
    root_id              TEXT NOT NULL,
    -- 0 for a root; a run's roles are depth 1.
    depth                INTEGER NOT NULL,
    -- The caller's idempotency key: a replay of the same spawn returns the row.
    request_id           TEXT NOT NULL,
    -- A digest of what was asked for under that key, so reusing the key for a
    -- different spawn is refused rather than silently answered with the old one.
    request_fingerprint  TEXT NOT NULL,
    -- 'user' | 'agent' | 'team'. Derived by the host from the caller's scope,
    -- never taken from the request.
    origin               TEXT NOT NULL,
    -- 'default' | 'review' | 'verify'. Fixed at spawn, reapplied on respawn.
    posture              TEXT NOT NULL DEFAULT 'default',
    -- 'reserved' | 'active' | 'parked' | 'ended'. Caps count reserved|active;
    -- a finished child parks (frees its slot); only an explicit cancel, a
    -- cascade, a run's end or GC ends one. Quitting the app never does.
    state                TEXT NOT NULL,
    -- 1 when the parent may answer this child's (non-Bash, non-write)
    -- permission prompts with Deny or an unchanged Allow.
    allow_parent_permits INTEGER NOT NULL DEFAULT 0,
    -- 'auto' | 'stuck' | 'off': which child events reach the parent.
    notify_parent        TEXT NOT NULL DEFAULT 'auto',
    -- The child's identity colour slot in the parent's UI, once assigned.
    identity_slot        INTEGER,
    created_at           TEXT NOT NULL,
    -- When `state` last changed: a stale `reserved` row (a launch that never
    -- finalised) is reclaimed by age.
    updated_at           TEXT NOT NULL,
    ended_at             TEXT,
    UNIQUE (parent_id, request_id)
);

-- Cap counts and ListChildren: children of one parent, and of one root.
CREATE INDEX idx_agent_lineage_parent ON agent_lineage(parent_id, state);
CREATE INDEX idx_agent_lineage_root ON agent_lineage(root_id, state);

-- Consecutive failed launches per parent: the breaker that stops a parent
-- whose children keep failing to start from burning its whole budget. Keyed by
-- parent, not request id, so retrying with a fresh id does not reset it.
CREATE TABLE agent_spawn_failures (
    parent_id   TEXT PRIMARY KEY,
    count       INTEGER NOT NULL,
    last_reason TEXT NOT NULL,
    last_at     TEXT NOT NULL
);

-- Child events waiting to reach a parent. Written by the host's child watcher,
-- drained through the parent's send gate so a Stop, an interrupt or a rate
-- limit holds them. A row is consumed only when the parent's CLI accepted it,
-- never when it was written.
CREATE TABLE agent_event_outbox (
    id              INTEGER PRIMARY KEY,
    parent_session  TEXT NOT NULL,
    child           TEXT NOT NULL,
    -- 'finished' | 'failed' | 'interrupted' | 'cancelled' | 'needs_permission' |
    -- 'needs_input'.
    kind            TEXT NOT NULL,
    -- The event's identity: the child's completed-turn ordinal or the permit
    -- request id. With the columns above it makes a second watcher pass a no-op.
    edge_id         TEXT NOT NULL,
    payload         TEXT NOT NULL,
    -- 'pending' | 'claimed' | 'sent' | 'accepted' | 'delivered' | 'unconfirmed'
    -- | 'undeliverable'. claimed = handed to the parent's send gate; sent = the
    -- line was written; accepted = the CLI started on it (never resend it);
    -- delivered = the turn that carried it reported it.
    state           TEXT NOT NULL DEFAULT 'pending',
    -- The delivery's idempotency key (one per drained batch).
    send_key        TEXT,
    -- Which host instance is delivering it, and since when (lock handover takes
    -- over a stale claim).
    claimed_by      TEXT,
    claimed_at      TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    created_at      TEXT NOT NULL,
    accepted_at     TEXT,
    delivered_at    TEXT,
    UNIQUE (parent_session, child, kind, edge_id)
);

-- The drainer's query: what is still owed to one parent.
CREATE INDEX idx_agent_event_outbox_parent ON agent_event_outbox(parent_session, state);
