-- Persist the agent's last assistant reply so a restored or re-adopted session
-- keeps showing its finished-turn message in the rail across an app restart,
-- instead of reverting to the bare status verb once the in-memory status
-- channel is recreated empty. Additive nullable column; existing rows carry no
-- message until their next turn is captured, so no backfill is needed.
ALTER TABLE agent_sessions ADD COLUMN last_message TEXT;
