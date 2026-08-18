# Memory plan — measured, not assumed

Baseline established 2026-08-18, phase 1 of the transcript-rendering and
memory work.

This document exists so that no later phase argues about memory from intuition.
Every number below was produced by `scripts/mem-smoke.sh`, and every phase from 2
onward is expected to re-run it and record its own before/after here.

One rule governs the whole approach: **measure first, then fix what the
measurement points at.** Numbers borrowed from another codebase — however
similar its architecture — are a hypothesis about this one, never a finding
about it. Section 3 is what happens when that distinction is taken seriously.

## 1. What is measured, and what is not

`scripts/mem-smoke.sh` boots a real `oximux serve` host, streams real agent
partials at it through `scripts/mem-smoke-agent.sh`, and samples the **host
process's** RSS at four checkpoints. The host is the subject because it owns the
transcript fold, which is where this plan's allocations live; the CLI driving it
is short-lived and its own RSS says nothing.

**Out of scope by construction — do not read a green run as covering it:**

- **Everything GPUI.** The sprite atlas, decoded images, and the transcript's
  render caches are in the desktop app, and no CI job in this repo boots a
  window. Measure those locally on macOS with `footprint` / `vmmap --summary`
  (the script header carries the one-pager).
- **Terminal scrollback**, already capped at 5,000 rows
  (`crates/pty/src/portable_pty_backend.rs:45`).
- **Long-horizon behaviour.** The harness runs for minutes. The plan's "flat
  ±10% over an 8h mixed session" goal needs a soak, which this is not.

## 2. Baseline — 2026-08-18, before any phase-2..8 work

Host: macOS / Apple silicon. Workload: 16 sessions, one streamed reply each,
400 × 1024-byte deltas per reply (409,600 bytes per reply; 6,553,600 total).
Idle window 60s with all 16 sessions open.

| Measurement | release | debug |
|---|---|---|
| Boot baseline (no sessions) | **19.4 MB** | 35.7 MB |
| Peak, right after the last reply | 71–76 MB | 95 MB |
| Steady state, after 60s idle | **73–78 MB** | 96 MB |
| Steady-state retention / raw text | **8.35× – 9.15×** | 9.55× |
| Idle delta (steady − peak) | **+1.6 – +2.3 MB** | +2.3 MB |
| `oximux transcript` (reopen) | **14–17 ms** | 42 ms |

Observed variance across n=3 release runs: boot ±3.5%, retention 8.10×–9.47×,
reopen 14–17 ms. The CI thresholds are set well outside this band on purpose
(see `scripts/mem-smoke.sh`, "Variance").

**The multiple is only comparable within one workload.** Retention is strongly
sub-linear in session count — a fixed per-session cost dominates small runs — so
this same tree measures **18.3× at 6 sessions × 200KB** and **8.4× at 16 × 400KB**.
Neither number is wrong; they answer different questions. The harness therefore
defaults to exactly the workload in this table, so that `./scripts/mem-smoke.sh`
with no arguments reproduces it. A phase that changes a knob and compares the
result to a row above has measured nothing, while producing a number that looks
like it means something.

**Reading of the baseline.** Retention is roughly 9× the raw streamed text and
scales sub-linearly with session count. That multiple is the number phases 2, 5,
6 and 7 are aimed at — it is dominated by what the fold and the render path
keep, not by allocator behaviour (see section 3). Reopen at 14 ms is the other
load-bearing fact: paging a transcript back from disk is cheap, so evicting a
dormant chat is not a feel trade-off. Any later phase that hesitates to drop
state because "reopening would be slow" should be answered with this number.

## 3. The allocator: measured, and rejected

**Phase 1 as planned called for `mimalloc` as `#[global_allocator]`. It was
implemented, measured, and reverted. It is a regression on macOS.**

The premise, taken from a peer codebase's published results: system malloc —
macOS libmalloc especially — keeps a churny workload's high-water mark as
permanent RSS, and mimalloc returns those pages. Their measurement made it their
single largest residency lever. But a result measured on another program is a
claim *about that program*; whether it holds here is a separate question, and
the only way to answer it is to run it here.

Same harness, same workload, the only difference being `#[global_allocator]`
wired into `oximux-cli` (the binary `oximux serve` runs):

| Configuration | boot | retention (peak-sampled) | multiple |
|---|---|---|---|
| release, system malloc | 19.3 MB | 53.5 MB | **8.36×** |
| release, mimalloc | 23.3 MB | 92.5 MB | **14.45×** |
| release, mimalloc, `MIMALLOC_PURGE_DELAY=0` | 23.7 MB | 90.3 MB | 14.10× |
| debug, system malloc | 35.7 MB | 59.6 MB | 9.30× |
| debug, mimalloc | 39.0 MB | 87.5 MB | 13.67× |

Consistently **+47% to +73% retention and +4 MB boot**, in both profiles and at
two workload sizes. Forcing immediate page purge does not recover it, which
rules out deferred decommit as the explanation — mimalloc is holding more, not
merely returning it later.

**Why the premise did not transfer.** Idle delta is small under *both*
allocators (+0.3 to +2.0 MB per minute, and negative once a transcript-fetch
spike is excluded). Nothing is ratcheting. The retained bytes are **live data
the fold is still holding**, not freed-but-unreturned pages — so there is no
watermark for a better allocator to hand back, and what a per-thread-arena
allocator does instead is add its own segment overhead across the host's tokio
worker threads.

**Decision (2026-08-18):** no global allocator override. The retention this plan
cares about is structural, and phases 2, 5, 6 and 7 are what remove it.

**Limits of this finding, stated so nobody over-reads it:**

- macOS only. No Linux machine was available, and the published result that
  motivated the swap was measured on Linux/glibc — whose arena behaviour is not
  libmalloc's. The CI job added in this phase runs on Linux and will produce the
  first Linux numbers.
- Headless host only. The GPUI desktop app's allocation profile — image decode
  churn above all — is unmeasured, and is the surface on which the largest
  allocator win was reported elsewhere.

Revisit if either gap closes with contrary evidence. Until then this is a
measured "no", not a "we didn't try".

## 4. What phase 1 shipped

- `scripts/mem-smoke.sh` — the harness, `--check` to enforce thresholds,
  `--json` for machine-readable output.
- `scripts/mem-smoke-agent.sh` — a **partial-emitting** agent fixture. This is
  load-bearing: the existing `apps/cli/tests/fixtures/fake-agent.sh` sends one
  complete `assistant` message and no partials, so a retention number taken
  against it would be near zero regardless of workload. The harness refuses to
  report a run in which the fixture streamed zero bytes.
- A `mem-smoke` CI job (Linux), thresholds set wide pending the first Linux
  numbers.
- This document.

## 4a. GPUI asset release at the pinned rev — available, no fork needed

Recorded because the transcript work planned around the opposite answer.

The pinned GPUI **does** expose everything needed to give an image back:

| API | What it frees |
|---|---|
| `Image::get_render_image(self: Arc<Self>, window, cx)` | (reads) the decoded `Arc<RenderImage>` from the asset cache |
| `App::drop_image(Arc<RenderImage>, Option<&mut Window>)` | the GPU **sprite-atlas tile**, on every window |
| `Image::remove_asset(self: Arc<Self>, cx)` | the decoded bitmap in the **asset cache** |

Order matters: `drop_image` needs the `Arc<RenderImage>`, and the only public
route to it is asking the asset cache — so it must happen *before*
`remove_asset` evicts the entry that answer comes from.
`apps/desktop/src/shell/agent_chat/image_cache.rs::release` does exactly this.

Two things worth knowing before trusting a cache eviction to reclaim memory:

- **Three copies, not one.** An attached image is resident as encoded bytes
  (ours), a decoded RGBA `RenderImage` (GPUI's asset cache), and an atlas tile
  (GPU). A ~2MB screenshot decodes to ~33MB. Dropping only our `Arc` reclaims
  the *smallest* of the three, which is how an eviction can look implemented and
  free almost nothing.
- **`App::remove_asset` alone does not free the tile.** It removes the entry
  from `loading_assets` and nothing else. An eviction path that calls only
  `remove_asset` leaves the GPU allocation behind.

## 4b. Phase 2 — the publish path, and why a revision gate was the wrong tool

`AgentChatView::publish_remote_transcript` re-serialized the whole fold on every
settled batch, so a turn making twenty tool calls re-serialized the entire
conversation twenty times, each pass O(all entries ever).

The planned fix was a revision gate, mirroring the one persistence uses
(`crates/agent-core/src/thread/state.rs`). **It cannot skip anything here.**
`ChatThread::apply` calls `touch()` unconditionally on entry — deliberately, its
comment says an event that folds to nothing still bumps — and the publish is
reached only after a non-empty non-delta batch has been applied. Every call
therefore arrives at a fresh revision. The gate would compile, read as a fix,
and never once skip.

The same gate *does* work in the headless host (`apps/cli/src/serve/pump.rs`),
and the difference is instructive: there it is consulted from a 10-second timer
tick, which can fire with no intervening event. **A revision gate is only worth
anything where the caller can arrive without a mutation.** A per-batch caller
never can.

What removes the work is coalescing, which the design already permits — the
snapshot exists for a client *opening* the session, and one opening mid-stream
folds live deltas from the backlog on top of it. Publishing is now throttled to
one per 250ms with a trailing publish, so staleness stays bounded when events
stop. Bind and rekey bypass the throttle: a freshly bound peer holds no snapshot
at all and must not wait.

**Not measurable by `scripts/mem-smoke.sh`, structurally.** Both of Phase 2's
defects live in `apps/desktop`, which links GPUI; the harness measures
`oximux serve`, which is `oximux-cli`, which a CI gate explicitly forbids from
linking GPUI. There is no workload the harness can run that reaches this code.
What Phase 2 changes is bounded analytically instead: the image cache moves from
*unbounded* (no `remove`, `clear` or `retain` existed anywhere in the crate) to a
64MB encoded ceiling with full three-copy release, and the publish path from
O(transcript) per settled event to O(transcript) per 250ms. A GUI-side
`vmmap` measurement over an image-heavy session is still owed; see section 1 for
why CI cannot supply it.

## 5. Standing expectations for later phases

Every phase from 2 onward:

1. Runs `scripts/mem-smoke.sh` before and after its change.
2. Records both numbers in its PR **and appends a row to section 6 below**.
3. Treats a retention-multiple increase as a defect of that phase, not as a
   reason to widen a threshold.

## 6. Phase log

| Date | Phase | boot | steady state | retention multiple | Note |
|---|---|---|---|---|---|
| 2026-08-18 | 1 (baseline) | 19.4 MB | 73–78 MB | 8.35×–9.15× | release, macOS, 16×400KB |
| 2026-08-18 | 1 (baseline) | 35.0 MB | 96 MB | 9.55× | debug, macOS, 16×400KB (harness default) |
| 2026-08-19 | 2 | — | — | — | desktop-only; unmeasurable by this harness (section 4b). Image cache bounded at 64MB from unbounded; publish coalesced to 250ms |
