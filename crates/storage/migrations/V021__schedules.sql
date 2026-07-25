-- V021: scheduled agent runs — a prompt sent into a fresh session on a repeating
-- schedule, plus the history of what each fire actually did.
--
-- `kind` is 'interval' | 'daily' | 'weekly', with the numeric columns read
-- according to it: `interval_minutes` for 'interval', `hour`/`minute` for
-- 'daily', and additionally `weekday` (0=Monday) for 'weekly'. Stored as
-- separate columns rather than an encoded expression string so a malformed row
-- is impossible to write and no parser sits between the table and the type.
--
-- `next_fire_at` is a materialized RFC-3339 local-offset timestamp rather than
-- being recomputed at every tick: the tick path compares it directly, and
-- persisting it means a restart resumes the same schedule rather than silently
-- shifting every wall-clock run to whenever the app happened to reopen.
--
-- `cwd` scopes a run to a project directory the desktop already has, matching
-- the containment CreateSession applies.

CREATE TABLE schedules (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    cwd              TEXT NOT NULL,
    prompt           TEXT NOT NULL,
    agent_id         TEXT,
    kind             TEXT NOT NULL,
    interval_minutes INTEGER,
    hour             INTEGER,
    minute           INTEGER,
    weekday          INTEGER,
    enabled          INTEGER NOT NULL DEFAULT 1,
    next_fire_at     TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

-- One row per fire attempt. `fired_at` is the scheduled instant, NOT the moment
-- the tick noticed it — together with `schedule_id` it is the idempotency key
-- that stops a restart near a fire boundary from running the same occurrence
-- twice.
--
-- `outcome` is 'ok' | 'failed'. `session_id` is the session the run created when
-- it succeeded; `detail` carries a short failure reason otherwise.
CREATE TABLE schedule_runs (
    schedule_id  TEXT NOT NULL,
    fired_at     TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    session_id   TEXT,
    detail       TEXT,
    PRIMARY KEY (schedule_id, fired_at)
);

CREATE INDEX schedule_runs_by_schedule ON schedule_runs (schedule_id, fired_at DESC);
