---
name: oximux-cli
description: |
  Drive OxiMux from the command line: start agent sessions, watch them to
  completion, decide their permission requests, and manage the worktrees they
  work in. Activate when the task involves the `oximux` command, an OxiMux
  host, an agent session id, or coordinating work that another agent is doing.
---

# Driving OxiMux from the CLI

`oximux` talks to a **host**: either `oximux serve` (headless, over SSH) or the
desktop app with local CLI access enabled. With no hosts paired, every command
talks to this machine and needs no configuration.

Run `oximux agent-context` for the complete command tree as JSON. It is read
from the parser itself, so it can never describe a verb the binary lacks. This
guide is the part that JSON cannot tell you: which verbs to reach for, and the
two contracts that catch people out.

## Contract 1: accepting is not finishing

Send-style verbs return when the host **accepts** the work, not when the agent
finishes. `oximux send S "carry on" --no-wait` succeeding means the prompt was
queued — nothing more. To learn what the agent did, watch the session.

## Contract 2: `--timeout` does not bound a turn

Three different clocks, and confusing them is the most common mistake:

| Flag | Bounds | Scope |
| --- | --- | --- |
| `--timeout` | one host reply — **except on `wait`, where it bounds the whole wait** | global, default 10s |
| `--turn-timeout` | the whole turn | `run`, `send` |
| `--stalled-after` | silence, not total time | `run`, `send`, `wait` |

On `run` and `send`, `--timeout` is an RPC budget: it never cuts a turn short,
and those verbs stream **unbounded** by default. That is right at a terminal and
wrong in a script, because a turn parked on a permission request ends only when
something decides it.

`wait` is the exception, and the one that catches people. There `--timeout` is
the total budget for the wait itself — so a bare `oximux wait S --until idle`
gives up after **10 seconds**, and any `--stalled-after` larger than that can
never fire. Always raise `--timeout` on a `wait` you expect to take minutes.

Use both budgets together: a generous `--turn-timeout` with a tight
`--stalled-after`. A turn timeout alone cannot tell a thinking agent from a
wedged one — raise it and a wedged agent burns the lot, lower it and a working
one is cut off.

Either expiry exits **4** and leaves the agent running. Only the command
stopped waiting.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | succeeded |
| 1 | ran and failed |
| 2 | wrong arguments; nothing was attempted |
| 3 | no host answered — retrying may help |
| 4 | timed out |
| 5 | not allowed — retrying will not help |

Branch on the number. `--json` prints `{"ok":true,"data":…}` or
`{"ok":false,"error":{code,message,next_steps}}`; streaming verbs emit NDJSON
event lines and then a final result object. `error.data` appears when a failure
leaves something addressable behind — a turn timeout carries the `session_id`
of the agent still running.

## Starting work

```sh
# Stay attached and stream the turn to completion.
oximux run "add a regression test for the parser" --mode acceptEdits

# Fire and forget: prints the session id, exits immediately.
oximux run "audit the error paths" --bg --json
```

Without `--mode`, a session starts in the backend's default, which for most
agents asks before every tool. An unattended `run` then streams up to the first
request and waits there forever. `acceptEdits` is the usual choice for a
scripted run **with claude** — mode ids are the backend's own, so read them from
`oximux model ls <SESSION>` rather than assuming this one exists.

`--worktree <SLUG>` creates a worktree under the project first and starts the
session inside it, so parallel runs do not edit each other's files. The project
is the `--cwd` (or current) directory, and it **must already be a project the
host knows** — check `oximux projects ls` first, or the run fails before it
starts.

`--output-schema` holds the final answer to a JSON Schema (a file path or
inline JSON). The agent is re-prompted with the validation errors up to twice;
a still-invalid answer exits 1. It needs the turn, so it cannot be combined
with `--bg`.

## Watching it finish

```sh
oximux ls                                    # sessions on this host
oximux wait S --until idle --timeout 900 --stalled-after 90
oximux transcript S --json                   # everything the session produced
oximux attach S --from 41                    # replay events AFTER seq 41
```

`wait --until` takes `done` (the turn ended), `needs-approval` (something is
waiting on a decision), or `idle` (done, and nothing is waiting).

One race to know about after `run --bg`: "done" is judged from the session's
retained events, and a session with no prompt in that window reads as done. A
`wait` issued in the same breath as the `--bg` that started the session can
therefore return immediately and wrongly. Give the session a moment, or attach
and watch for the first event, before trusting an instant `done`.

For an unattended run, `idle` is the one you want. Note what `done` does *not*
do: a session parked on a permission request has not ended its turn, so
`--until done` keeps waiting and then exits 4 on the timeout — it does not
return early. Waiting on `needs-approval` in parallel, or polling
`permit ls`, is how you find out that a decision is what the turn is missing.

## Deciding for an agent

A silent turn is usually a turn waiting on you.

```sh
oximux permit ls S --json
oximux permit allow S <REQUEST>
oximux permit deny S <REQUEST> --message "not on main"
oximux permit answer S <REQUEST> --answer "Option B"
```

The request id defaults to the latest pending one. `permit allow --input '<JSON>'`
replaces the tool's proposed input before approving — the agent is told the call
was allowed and runs your arguments instead of its own. It replaces the input
rather than merging into it, so pass every field the tool needs.

To redirect a running agent, `oximux steer S "check the null case first"`. It
needs a backend with a mid-turn message queue; claude and codex have none and
refuse it. There, use `oximux stop S` and then `oximux send S "…"`.

## Worktrees

```sh
oximux worktree create fix-parser --project /path/to/repo
oximux worktree ls --json
oximux worktree set <ID> --comment "rewriting the lexer" --phase in-progress
oximux worktree rm <ID>
```

`worktree set` is meant to be called **by the agent working there, as it
works** — it is how a long run stays legible from outside. `--phase` takes
`todo`, `in-progress`, `in-review`, or `done`; `""` clears either field.

`worktree rm` is refused, not forced, when the worktree has uncommitted
changes.

`rm`-style verbs are **idempotent**: `worktree rm`, `heartbeat rm` and
`state delete` all exit 0 for an id that never existed. Exit 0 therefore does
not mean the thing was there — if you need to know it existed, list it first.

## Switching model and mode

```sh
oximux model ls S            # models AND permission-mode ids this backend offers
oximux model set S <MODEL>
oximux mode set S <MODE>
```

`model set` may be refused by backends that fix the model at spawn when no
desktop view can respawn the child.

## Recurring work

```sh
oximux heartbeat create "sweep the queue" --name sweep --cron "*/15 * * * *" --session S
oximux heartbeat ls
oximux heartbeat rm <ID>
```

A heartbeat is a session's own wake-up: each fire sends the prompt into that
session. `--session` defaults to the session the command runs in.

A **schedule** is the other half: each fire opens a *fresh* session instead of
waking one that exists.

```sh
oximux schedule create "write the standup note" --name standup --cron "0 9 * * 1-5"
oximux schedule create "nightly report" --name nightly --daily 23:30
oximux schedule ls
oximux schedule logs <ID>
```

Cadence is exactly one of `--every N`, `--daily HH:MM`, `--weekly "DAY HH:MM"`,
or `--cron` with a five-field expression. Times are the **host's** local clock,
cron included — a schedule carries no timezone of its own.

`--cron` needs a host on protocol v23 or newer; against an older one the CLI
refuses with the version it needs rather than sending a frame that host cannot
decode. The five-minute floor applies to cron too, so `* * * * *` is refused,
as is an expression that parses but can never come around (`0 9 30 2 *`).

Note the two `--cron` flags differ: `heartbeat --cron` takes only the shapes
that map onto the preset cadences (`*/N * * * *`, `M H * * *`, `M H * * DOW`),
because its wire message predates full cron. Ranges and lists like
`0 9 * * 1-5` work on `schedule --cron` only.

## When you are the agent inside a session

If `OXIMUX_SESSION_ID` is set in your environment, this CLI reaches **only that
session**. That is a credential, not a hint — do not print it, pass it to a
subprocess you do not control, or write it anywhere it will be read back later.

In that scope, the `worktree set` and `state set` verbs above are how you
report progress, and `heartbeat create` with no `--session` arms your own
wake-up.
