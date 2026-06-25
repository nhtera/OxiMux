-- Persist the agent's title (its most recent user prompt) so a restored or
-- re-adopted session keeps its rail title across an app restart, instead of
-- decaying to the bare status verb once the in-memory prompt cache is gone.
-- Additive nullable column; existing rows carry no title until their next
-- prompt is captured, so no backfill is needed.
ALTER TABLE agent_sessions ADD COLUMN title TEXT;
