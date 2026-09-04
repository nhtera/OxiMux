-- V027: record which agent, and which model, worked each team role.
--
-- V023 gave a run one agent for every role, because that is what the launcher
-- took. The loop teams are actually built around is the opposite — one agent
-- plans, another implements, a third reviews — so the choice belongs on the
-- role, not the run.
--
-- Nullable rather than NOT NULL DEFAULT '': "no agent recorded" is a real and
-- permanent state here, not an unset field waiting to be filled. It is what
-- every role created before this migration is, and what a role created without
-- an override still is — the host resolved its own default at launch and the
-- dispatcher never learned the name. A '' sentinel would make those
-- indistinguishable from a role someone deliberately left blank.
--
-- The value stored is the agent the dispatcher resolved for that role: the
-- role's own override when it had one, otherwise the run-level `--agent`.
-- Reading the board should answer "which agent worked this", not "was an
-- override typed", and the two only differ for roles nobody asked about.

ALTER TABLE team_run_roles ADD COLUMN agent_id TEXT;
ALTER TABLE team_run_roles ADD COLUMN model TEXT;
