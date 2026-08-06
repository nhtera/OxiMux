-- V024: the coordination blackboard — a small versioned key/value store agents
-- share without routing through a transcript.
--
-- `version` is what makes concurrent writers safe: a caller reads a value with
-- its version and writes back conditional on that version still standing, so
-- two agents updating the same key cannot silently clobber each other. It is a
-- counter rather than a timestamp because two writes in the same clock tick
-- must still be distinguishable.

CREATE TABLE coord_state (
    key         TEXT PRIMARY KEY,
    -- The value as a JSON document. Stored as text, not parsed: the store has
    -- no opinion about the shape agents agree on between themselves.
    value_json  TEXT NOT NULL,
    version     INTEGER NOT NULL,
    updated_at  TEXT NOT NULL
);
