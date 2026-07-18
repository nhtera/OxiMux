-- V019: persist remote-control paired devices so the authorized set, each
-- device's scope, and revocation survive an app restart. The in-memory
-- `AuthStore` (oximux-remote-host) seeds from this at boot and writes through on
-- register / revoke. One row per app-signing Ed25519 public key (lowercase hex).
--
-- `scope` is 'full' (a static/global pairing — the confirmed default) or
-- 'sessions' (a session-bound one-time ticket). `scope_sessions` holds the
-- newline-joined session ids when scope = 'sessions', else NULL — session ids
-- are single-line tokens, so no escaping is needed and no serde dep is pulled in.
-- `revoked` is a soft delete: a revoked device stays recorded (so a re-`Register`
-- with the still-known pairing secret can't silently resurrect it) until the
-- user explicitly removes it.

CREATE TABLE remote_devices (
    pubkey          TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    scope           TEXT NOT NULL,
    scope_sessions  TEXT,
    revoked         INTEGER NOT NULL DEFAULT 0,
    paired_at       TEXT NOT NULL,
    last_seen       TEXT
);
