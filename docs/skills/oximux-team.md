---
name: oximux-team
description: |
  Run several OxiMux agents on one task, each with its own role, session, and
  worktree, and coordinate them through the shared state blackboard. Activate
  when the task involves `oximux team`, a run id, splitting work across roles,
  or agents that must claim work or publish facts to each other.
---

# Running a team

One task, several agents, each with its own role and session. The host tracks
them as a **run**; each role settles itself when it finishes.

Read `oximux-cli` first — the async contract, the turn budgets, and the exit
codes apply here unchanged.

## Starting a run

```sh
oximux team run \
  --name "parser rewrite" \
  --role plan="survey the lexer and write a plan to plans/lexer.md" \
  --role impl="implement the plan once plans/lexer.md exists" \
  --role-agent plan=claude \
  --role-agent impl=codex \
  --worktree-each \
  --json
```

`--role NAME=PROMPT`, repeated, 1 to 8 of them. `--cwd` is the project every
role works in (default: the current directory). `--agent` picks which
configured agent runs every role.

## One agent per role

`--role-agent NAME=AGENT_ID` gives one role its own agent; `--role-model
NAME=MODEL` gives it its own model. Both are matched by role name and repeat
per role. A role named by neither falls back to `--agent`, and then to the
host's default — so mixing them is normal:

```sh
oximux team run --name sweep \
  --role plan="survey" --role impl="build" --role review="check" \
  --agent claude --role-agent impl=codex --role-model plan=opus
```

That runs `impl` on codex and the other two on claude, with `plan` switched to
opus.

Two things to know before scripting it:

- A `--role-agent` naming a role no `--role` declared is a **usage error**
  (exit 2) listing the roles that do exist, and nothing starts. That is
  deliberate: a typo would otherwise silently apply to nothing, and the run has
  already begun by the time you could notice it ran on the wrong agent.
- `--role-model` is applied **at spawn**, not as a switch afterwards. That is
  the only moment it works for Claude and Codex, which take `--model` on the
  command line and refuse to change it at runtime.
- An agent that cannot be given a model at spawn — an ACP-based one, whose
  protocol has no model at connect time — **fails that role** rather than
  quietly opening on its default. Its teammates keep going, and the board says
  what to do about it.
- A model name the agent *does* accept at spawn but does not recognise is the
  agent's own business: OxiMux does not validate model names, so depending on
  the backend that surfaces as a failed launch or a failing first turn.

Both flags need a host speaking protocol v22 or newer. Against an older one,
`team run` with neither flag still works exactly as before; naming either is
refused with a message about the version (exit 3), not a broken run.

`team status` shows the agent per role once a run has one:

```
run-x9  sweep  open
  plan   done     session s-1  agent claude  model opus
  impl   running  session s-2  agent codex
```

A role that names no agent shows none rather than the word "default": the host
resolved its own and never reported which. Runs opened before per-role agents
existed read back the same way — no agent recorded, which is what they are.

Under `--json` the `agent_id` and `model` keys are always present, and `null`
when nothing was recorded — including from a host too old to have them. A
missing key never means "no agent".

`--worktree-each` gives each role its own worktree so roles editing the same
files do not collide. The host derives each path. Use it whenever two roles
touch the same tree — without it they share one checkout and will overwrite
each other.

The reply carries the run id and one session id per role. Every session verb in
`oximux-cli` works on those ids: `attach`, `permit ls`, `stop`.

## The settle protocol

A role is not finished when its turn ends. It is finished when it **says** it
is:

```sh
oximux team report --run <RUN> --role impl --status done \
  --summary "lexer rewritten, 14 tests added, all green"
```

`--status` is `done` or `failed`. `--summary` is what it did, or why it could
not — a failed role that explains itself is worth far more than one that just
stops.

If you are running as a role, this is your last action. Nothing else settles
you: a turn that ends without a report leaves the board showing you as still
working.

## Watching the board

```sh
oximux team status --run <RUN> --json   # every role and where it stands
oximux team ls                          # every run this host holds, newest first
```

## The state blackboard

Versioned keys that agents read and write. This is how roles share facts
without talking to each other.

```sh
oximux state get build/status
oximux state set build/status '"green"'
oximux state set counts '{"tests":14}'
oximux state delete build/status
oximux state watch --prefix build/ --since 118
```

Values are **JSON**, so a bare string needs its quotes: `'"green"'`, not
`green`.

An unset key prints `(unset)` and exits **0**. "Nobody has claimed this" is an
answer, not a failure — do not treat a missing key as an error.

### Claiming work without a race

`--if-version` writes only if the stored version is exactly that number, and
`0` means "only if absent". A mismatch exits **5** and prints the current
value, so the loser of a race learns who won in the same breath:

```sh
# Claim item 7. Exits 5 if another role already has it.
oximux state set work/item-7 '"claimed-by-impl"' --if-version 0
```

This is the only safe way for two roles to divide a queue. A plain `state set`
races.

### Resuming a watch

`state watch` prints every matching key and then streams changes until Ctrl+C.
`--since <SEQ>` resumes after a sequence number from a previous watch. Without
it, the watch starts from the board as it stands now.

A resume is **not** guaranteed to be gapless, and the CLI tells you when it is
not. If the host can no longer replay from your cursor, it re-sends the whole
board and emits a marker first — `{"resynced":true,"since":…,"seq":…}` under
`--json`, `— resynced —` in human form. Branch on it: it means transitions
happened that you will never see, so any state you were accumulating must be
rebuilt from the baseline rather than patched. A supervisor that ignores
`resynced` silently carries a hole in its history.

## A worked supervision loop

```sh
RUN=$(oximux team run --name sweep \
        --role a="fix the failing tests in crates/parser" \
        --role b="fix the failing tests in crates/lexer" \
        --worktree-each --json | jq -r .data.id)

# Roles run unattended, so drain what they park on until every role settles.
while oximux team status --run "$RUN" --json |
      jq -e '[.data.roles[] | select(.status == "running")] | length > 0' >/dev/null; do
  oximux team status --run "$RUN" --json |
    jq -r '.data.roles[].session_id | select(. != null)' |
    while read -r S; do
      oximux permit ls "$S" --json |
        jq -r '.data[] | select(.kind == "permission") | .request_id' |
        while read -r REQ; do oximux permit allow "$S" "$REQ"; done
    done
  sleep 5
done

oximux team status --run "$RUN"
```

That loop is deliberately optimistic about the host being up. In anything
unattended, check the exit code too — `team status` exits **3** when no
host answered, and the `while` above would read that failure as "no roles
running" and declare the run finished. Branch on the number before you trust
the JSON.

Five things to copy from that loop, whatever shape yours takes:

- Poll `team status`, not any single session. A run is done only when every
  role has settled, and a role's status is `running` until it reports.
- `session_id` is nullable — a role whose session could not start has none.
- Drain `permit ls` on each session. An unattended role parked on a permission
  request never settles on its own.
- Only rows with `"kind": "permission"` can be allowed. A `"question"` row needs
  `permit answer --answer …`; passing it to `permit allow` fails.
- Exit 3 from any of these verbs means the host is unreachable, not that the
  work is done. Treat it as a retry, not as a result.
