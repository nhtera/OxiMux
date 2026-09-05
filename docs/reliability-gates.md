# Reliability gates

A **gate** is a reliability claim with its evidence attached. The ledger lives at
[`config/reliability-gates.toml`](../config/reliability-gates.toml) and is
validated in CI by:

```
cargo run -p xtask -- reliability-gates
```

## Why this exists

OxiMux plan documents have twice carried a "VERIFIED" row that was false. Once
the evidence was a truncated `ls` that showed the first screenful and was read as
the whole set. Once it was an API that genuinely existed — just not on the host
that mattered.

Neither was a lie, and neither was caught in review. That is the point: prose has
no shape a reader can check. A sentence claiming coverage looks exactly like a
sentence that has it.

So the claim gets a shape. Every gate names the invariant, how you would know it
broke, and — the field that does the real work — **where the invariant must hold
versus where it is actually proven**.

## The one contract that matters

```toml
platforms         = ["macos", "windows", "linux"]   # where it must hold
covered_platforms = ["macos"]                       # where it is proven
```

`platforms` is the claim. `covered_platforms` is the evidence. The gap between
them is a field rather than a silence.

**A declared gap is data, not a failure.** CI does not fail because something is
uncovered — it fails when the two lists contradict each other, or when a gap is
declared with no explanation of why it exists. Both of the false rows above were
exactly that gap going unrecorded.

`agents` / `covered_agents` work the same way for claims that vary by agent
rather than by platform.

## What the validator does and does not do

It checks **shape and internal consistency only**:

- a `schema_version` this validator does not understand
- unknown fields and invalid `status` values (rejected at parse)
- duplicate gate ids
- an empty `platforms` — silently omitting the distinction is the error the
  ledger exists to prevent, so a gate must say where its invariant must hold
  even when nothing proves it yet
- empty `id`, `title`, `owner`, `invariant`, or `oracle`
- a `test_files` entry that does not exist on disk
- an `assertions` entry not defined in any of that gate's `test_files` — this is
  how coverage disappears quietly: a test is renamed, everything else still
  passes, and the gate goes on citing evidence that is gone
- `covered_platforms ⊄ platforms`, `covered_agents ⊄ agents`
- a declared gap, or a `flaky` status, with empty `coverage_notes`
- a gate citing no `test_files` whose status is not `accepted-gap`
- a `covered_platforms` entry contradicted by a crate-level `cfg` on one of the
  cited files — a file gated `#![cfg(unix)]` compiles to nothing on Windows, so
  it cannot prove Windows however green CI looks

It **never asserts that a test passes.** That is `cargo test`'s job, and
duplicating it here would make the ledger a second, worse CI.

If that line ever blurs — if contributors start deleting gates to get CI green —
the mechanism has failed and should be cut back rather than loosened.

## Fields

| Field | Meaning |
|---|---|
| `schema_version` | Top of file, not per gate. Currently `1`. |
| `id` | Stable `area.short-claim`. Referenced from code comments, so a rename is breaking. |
| `title` | One line, human-facing. |
| `status` | `experimental` · `soaking` · `stable` · `flaky` · `accepted-gap` |
| `owner` | The crate or subsystem that would fix this if it broke. |
| `invariant` | What must be true, as a property — not a test name. |
| `oracle` | How a regression would present. |
| `platforms` / `covered_platforms` | The claim and its evidence. |
| `agents` / `covered_agents` | Same, per agent. |
| `coverage_notes` | Why a gap exists and what would close it. Required when there is a gap or the status is `flaky`. |
| `test_files` | Repo-relative evidence files. Existence is checked; content is not. |
| `assertions` | Test function names within them. Existence is checked. |

### On `status`

`accepted-gap` is the status that keeps the ledger honest. Without it, an area
with no automated coverage has to either overstate itself or go unlisted, and
both are how the original problem happened.
`desktop.single-instance-guard` is the worked example: a process-level property
no test binary can assert, verified by hand, recorded so the gap is not
forgotten rather than to claim anything.

`flaky` is first-class so a known-flaky test is tracked data with a recorded
cause, rather than a code comment the next person reads as folklore. A `flaky`
gate must say what the symptom is and what is known about the cause.

## Reading a green run

Two traps the fields cannot express, both live in this repo today, both recorded
in `coverage_notes` on the rows they affect:

- **A test can pass without asserting anything.** The `wire_skew_e2e` suite
  returns early when `OXIMUX_SKEW_CLI` is unset, so the Linux and Windows CLI
  steps report it green having tested nothing. Only the macOS skew step is
  evidence.
- **A test can be flaky in one direction.** `spawn_args_reach_child_process`
  fails only under parallel contention, so a green isolated run says less than
  it appears to.

When you write an `oracle`, say what a *false* pass would look like. That is
usually the more useful half.

## When to add one

**When something breaks.** Not speculatively.

A gate is worth writing when a failure was expensive, subtle, or presented as
something other than itself — the bugs that survive longest are the ones that
look like a different bug. If you cannot write a useful `oracle`, you probably do
not yet understand the failure well enough to gate it.

The ledger is capped at ~25 gates and warns past that. The honest response to
growth is to cut back to the gates that map to real incidents, not to raise the
cap. A small honest ledger is worth more than a large stale one, and deleting a
gate that no longer earns its place is maintenance, not regression.

## Known limitation

`xtask ci-check` runs every check including this one, but CI invokes the
subcommands individually and does not call `ci-check`. Adding a check to the
`ci-check` chain therefore does **not** put it in CI — it needs its own step in
`.github/workflows/ci.yml`. `data-dir-lint` and `appearance-lint` are in the
chain but not in CI today.
