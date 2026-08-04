-- V022: what a schedule fires INTO. NULL (the only value any writer produces
-- today) means "spawn a fresh session per fire"; a session id means "send the
-- prompt into that existing session" — the fire path branches on it now so the
-- contract is settled, even though nothing writes a non-NULL value yet.
--
-- Additive with no default backfill: every existing schedule keeps its
-- fresh-session behavior.

ALTER TABLE schedules ADD COLUMN target_session_id TEXT;
