#!/usr/bin/env bash
#
# mem-smoke — the memory baseline every phase of the transcript/memory plan is
# measured against.
#
#   ./scripts/mem-smoke.sh              measure and print
#   ./scripts/mem-smoke.sh --check      measure, print, and fail on a threshold
#   ./scripts/mem-smoke.sh --json out.json    also write the machine-readable form
#
# ---------------------------------------------------------------------------
# What it measures, and what it deliberately does not
# ---------------------------------------------------------------------------
#
# Five numbers, against a real `oximux serve` host driven by the real CLI:
#
#   boot_baseline_kb        RSS at readiness, no sessions
#   peak_retention_multiple growth right after the last reply / bytes streamed
#   retention_multiple      growth at STEADY STATE / bytes streamed
#   reopen_ms               `oximux transcript` wall time for a streamed session
#   idle_delta_kb           steady state minus peak; NEGATIVE means released
#
# The two retention numbers are not redundant, and measuring only the first —
# which this script did until it was caught doing so — is actively misleading.
# The host is still settling when the last reply lands: three consecutive
# release-profile runs put `idle_delta_kb` at +320kB, -30MB and -45MB, which is
# not variance, it is a sample taken at three different points of the same
# unfinished teardown. Peak is what the machine has to survive; steady state is
# what actually accumulates across a workday, and it is the one the plan's "no
# monotonic ratchet" goal is about. A gate built on the peak alone would flap
# by tens of megabytes for reasons that have nothing to do with a regression.
#
# The subject is the HOST process, not the CLI that drives it: the host owns the
# transcript fold, which is where this plan's allocations live. The CLI is a
# short-lived client and its own RSS says nothing.
#
# It measures the headless surface only. The GPUI window is not booted here and
# should not be — no CI runner in this repo launches one (every job is
# check/test/package), so a harness that required a window would simply never
# run. GUI-side residency — the sprite atlas, decoded images, the transcript's
# render caches — is therefore OUT of this number by construction. Measure that
# locally on macOS with the one-pager below, and do not read a green mem-smoke
# as a statement about it.
#
# ---------------------------------------------------------------------------
# macOS one-pager: when a report comes in and this script says "fine"
# ---------------------------------------------------------------------------
#
# A GPUI app's residency is not all heap, so `ps`/RSS alone will mislead you.
# Split it:
#
#   footprint -p <pid>          Apple's own accounting: dirty vs compressed vs
#                               reclaimable. This is the number Activity Monitor
#                               shows as "Memory", and the one users report.
#   vmmap --summary <pid>       the breakdown that matters:
#                                 MALLOC_*        → the heap; mimalloc's domain
#                                 IOSurface       → GPU-shared surfaces
#                                 IOAccelerator   → Metal textures, the sprite
#                                                   atlas among them
#                                 __TEXT/__DATA   → mapped binary, not a leak
#   leaks <pid>                 only for a suspected true leak (unreachable
#                               blocks). Retention that is still referenced —
#                               which is nearly all of this plan's subject —
#                               shows up as nothing here.
#
# A growing MALLOC_* total with flat IOAccelerator is a fold/cache problem.
# The reverse is an image/atlas problem, which `remove_asset` (phase 2) owns.
#
# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------
#
#   OXIMUX_MEM_CHATS          streamed sessions to create        (default 16)
#   OXIMUX_MEM_CHUNKS         delta events per reply             (default 400)
#   OXIMUX_MEM_CHUNK_BYTES    bytes per delta                    (default 1024)
#
# The defaults are not arbitrary and should not be casually changed: they are
# exactly the workload `docs/memory-plan.md` records its baseline against, so
# that running this script with no arguments reproduces the documented numbers.
#
# **`retention_multiple` is only comparable within one workload.** Retention is
# strongly sub-linear in session count — a fixed per-session cost dominates
# small runs — so the same tree measures 18.3x at 6 sessions x 200KB and 8.4x at
# 16 x 400KB. Neither is wrong; they are answers to different questions. Change
# a knob and every threshold and every recorded number below becomes
# incomparable, which is worse than not measuring, because the numbers still
# look like they mean something.
#   OXIMUX_MEM_IDLE_SECONDS   idle window for the creep number   (default 60)
#   OXIMUX_MEM_PROFILE        debug | release                    (default debug)
#   OXIMUX_MEM_CLI            skip the build, use this binary
#
# Thresholds (only consulted under --check; see "Variance" below):
#
#   OXIMUX_MEM_MAX_RETENTION_MULTIPLE   default 12    (steady state)
#   OXIMUX_MEM_MAX_IDLE_GROWTH_KB       default 16384  (only positive counts)
#   OXIMUX_MEM_MAX_REOPEN_MS            default 5000
#
# ---------------------------------------------------------------------------
# Variance — read before tightening anything
# ---------------------------------------------------------------------------
#
# Runner variance is real and a flaky gate gets ignored, which is strictly worse
# than a loose one. The defaults above are deliberately generous: they are set
# to catch a regression of the kind this plan exists to fix (a retention
# multiple in the tens, a monotonic idle ratchet), not to police single-digit
# percentages. Tighten them in a later phase, from numbers this script actually
# produced on CI across several runs, and record the observed band here when you
# do. Do not tighten them from one local run on a quiet machine.
#
# Observed bands (fill in as they are gathered):
#   macOS / M-series, release, 16 chats x 400KB, n=3:
#     boot_baseline_kb    19328 - 20016   (+3.5%)
#     retention_multiple  8.10x - 9.47x   (+17%)
#     reopen_ms           14 - 16
#   macOS / M-series, debug (the default profile), 16 chats x 400KB, n=1:
#     boot_baseline_kb    34976
#     retention_multiple  9.55x
#     reopen_ms           42
#   ubuntu-latest: not yet gathered — the CI job below is what will gather it.
#     Its thresholds are set from the macOS numbers with wide headroom until
#     then, and should be revisited once several Linux runs exist.
#
set -euo pipefail

CHATS="${OXIMUX_MEM_CHATS:-16}"
CHUNKS="${OXIMUX_MEM_CHUNKS:-400}"
CHUNK_BYTES="${OXIMUX_MEM_CHUNK_BYTES:-1024}"
IDLE_SECONDS="${OXIMUX_MEM_IDLE_SECONDS:-60}"
PROFILE="${OXIMUX_MEM_PROFILE:-debug}"

MAX_RETENTION="${OXIMUX_MEM_MAX_RETENTION_MULTIPLE:-12}"
MAX_IDLE_GROWTH_KB="${OXIMUX_MEM_MAX_IDLE_GROWTH_KB:-16384}"
MAX_REOPEN_MS="${OXIMUX_MEM_MAX_REOPEN_MS:-5000}"

CHECK=0
JSON_OUT=""
while [ $# -gt 0 ]; do
    case "$1" in
    --check) CHECK=1 ;;
    --json) JSON_OUT="${2:?--json expects a path}"; shift ;;
    -h | --help) sed -n '2,95p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
AGENT_FIXTURE="$REPO_ROOT/scripts/mem-smoke-agent.sh"
[ -x "$AGENT_FIXTURE" ] || { echo "missing or non-executable: $AGENT_FIXTURE" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Platform: RSS sampling and a millisecond clock
# ---------------------------------------------------------------------------

case "$(uname -s)" in
Linux)
    PLATFORM=linux
    # /proc is the authority and costs nothing. `ps` on Linux reports the same
    # figure, but through a parse that differs between procps versions.
    rss_kb() { awk '/^VmRSS:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null || echo 0; }
    ;;
Darwin)
    PLATFORM=macos
    # `ps -o rss=` is resident kB. NOT the number Activity Monitor shows — see
    # the one-pager above — but the one that is comparable across platforms,
    # which is what a cross-platform threshold needs.
    rss_kb() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' || echo 0; }
    ;;
*)
    echo "mem-smoke supports Linux and macOS only (got $(uname -s))" >&2
    exit 1
    ;;
esac

# `date +%s%3N` is GNU-only; BSD date silently prints a literal `3N`, which
# would turn every duration into garbage rather than an error. Probe once.
if [ "$(date +%s%3N 2>/dev/null | tail -c 3)" = "3N" ]; then
    now_ms() { perl -MTime::HiRes -e 'printf "%.0f\n", Time::HiRes::time()*1000'; }
else
    now_ms() { date +%s%3N; }
fi

# ---------------------------------------------------------------------------
# The binary under measurement
# ---------------------------------------------------------------------------

if [ -n "${OXIMUX_MEM_CLI:-}" ]; then
    CLI="$OXIMUX_MEM_CLI"
else
    echo "building oximux-cli ($PROFILE) …" >&2
    if [ "$PROFILE" = "release" ]; then
        (cd "$REPO_ROOT" && cargo build --release -p oximux-cli >&2)
        CLI="$REPO_ROOT/target/release/oximux-cli"
    else
        (cd "$REPO_ROOT" && cargo build -p oximux-cli >&2)
        CLI="$REPO_ROOT/target/debug/oximux-cli"
    fi
fi
[ -x "$CLI" ] || { echo "no CLI binary at $CLI" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Scratch: a SHORT data directory
# ---------------------------------------------------------------------------
#
# Short on purpose, and not a style preference. The host's control socket is a
# unix socket at `<data-dir>/control-v1.sock`, and `sun_path` is ~104 bytes.
# `mktemp -d` on macOS hands back a `/var/folders/xx/…/T/tmp.XXXX` path that is
# most of that budget before the socket name is appended — and the bind failure
# it produces is reported as "another host is already serving here", which sends
# you looking for a process that does not exist. `/tmp` keeps it well clear.
SCRATCH="/tmp/oximux-mem.$$"
mkdir -p "$SCRATCH/data" "$SCRATCH/shim" "$SCRATCH/cwd"
SERVE_PID=""

cleanup() {
    if [ -n "$SERVE_PID" ] && kill -0 "$SERVE_PID" 2>/dev/null; then
        kill "$SERVE_PID" 2>/dev/null || true
        wait "$SERVE_PID" 2>/dev/null || true
    fi
    rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

# The injection point, same as the e2e suites': the launcher calls
# `program_for_spawn("claude")`, which is `which`, which finds this. No
# test-only branch ships in production code.
printf '#!/bin/sh\nexec "%s" "$@"\n' "$AGENT_FIXTURE" >"$SCRATCH/shim/claude"
chmod +x "$SCRATCH/shim/claude"

REPORT="$SCRATCH/agent-report.txt"
: >"$REPORT"

export PATH="$SCRATCH/shim:$PATH"
export OXIMUX_MEM_AGENT_CHUNKS="$CHUNKS"
export OXIMUX_MEM_AGENT_CHUNK_BYTES="$CHUNK_BYTES"
export OXIMUX_MEM_AGENT_REPORT="$REPORT"

cli() { "$CLI" --dir "$SCRATCH/data" --json "$@"; }

# ---------------------------------------------------------------------------
# (a) Boot baseline
# ---------------------------------------------------------------------------

echo "booting host …" >&2
"$CLI" serve --data-dir "$SCRATCH/data" >"$SCRATCH/ready.json" 2>"$SCRATCH/serve.log" &
SERVE_PID=$!

# stdout carries exactly one line, the versioned readiness object. Poll for it
# rather than blocking on `read`: if the host dies during boot the pipe closes
# and a blocking read would hang until the shell noticed, reporting a timeout
# where the log already says what went wrong.
waited=0
until [ -s "$SCRATCH/ready.json" ]; do
    if ! kill -0 "$SERVE_PID" 2>/dev/null; then
        echo "host exited before readiness; its log:" >&2
        tail -30 "$SCRATCH/serve.log" >&2
        exit 1
    fi
    sleep 0.2
    waited=$((waited + 1))
    if [ "$waited" -gt 150 ]; then
        echo "host did not become ready within 30s; its log:" >&2
        tail -30 "$SCRATCH/serve.log" >&2
        exit 1
    fi
done
grep -q oximux_serve_ready "$SCRATCH/ready.json" || {
    echo "readiness line is not the expected object: $(cat "$SCRATCH/ready.json")" >&2
    exit 1
}

BOOT_KB="$(rss_kb "$SERVE_PID")"
echo "boot baseline: ${BOOT_KB} kB" >&2

# ---------------------------------------------------------------------------
# (b) Streaming retention
# ---------------------------------------------------------------------------

FIRST_SESSION=""
i=1
while [ "$i" -le "$CHATS" ]; do
    out="$(cli --timeout 120 run "measure $i" --cwd "$SCRATCH/cwd" --mode acceptEdits \
        --turn-timeout 120 2>>"$SCRATCH/serve.log")" || {
        echo "run #$i failed; host log:" >&2
        tail -30 "$SCRATCH/serve.log" >&2
        exit 1
    }
    if [ -z "$FIRST_SESSION" ]; then
        # The streaming verbs emit NDJSON event lines then a final result
        # object, so the session id has to be picked out of the stream rather
        # than parsed from a single document. `session_id`, snake_case — the
        # event envelope and the final `data` object both use it.
        FIRST_SESSION="$(printf '%s' "$out" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)"
    fi
    echo "  chat $i/$CHATS → $(rss_kb "$SERVE_PID") kB" >&2
    i=$((i + 1))
done

# The peak. `sleep 3` covers the fold's own batching so this is not a sample
# taken mid-write — but it is deliberately NOT the settled number. Teardown of
# a finished turn continues well past this point; the steady-state sample after
# the idle window below is the one to compare across runs.
sleep 3
PEAK_KB="$(rss_kb "$SERVE_PID")"

# The assertion that keeps this number honest. A fixture that silently stopped
# streaming would report a beautiful retention multiple of nearly zero, and the
# harness would go green having measured nothing at all — which is exactly how
# the existing `fake-agent.sh` would behave if it were used here.
STREAMED_BYTES="$(awk -F= '/^streamed_bytes=/ {s += $2} END {print s + 0}' "$REPORT")"
if [ "$STREAMED_BYTES" -le 0 ]; then
    echo "the agent fixture reported 0 streamed bytes — this run measured nothing" >&2
    echo "(report: $REPORT, host log tail:)" >&2
    tail -30 "$SCRATCH/serve.log" >&2
    exit 1
fi

PEAK_RETENTION_KB=$((PEAK_KB - BOOT_KB))
PEAK_RETENTION_MULTIPLE="$(awk -v kb="$PEAK_RETENTION_KB" -v b="$STREAMED_BYTES" \
    'BEGIN { printf "%.2f", (kb * 1024) / b }')"

# ---------------------------------------------------------------------------
# (c) Reopen latency
# ---------------------------------------------------------------------------

REOPEN_MS=-1
if [ -n "$FIRST_SESSION" ]; then
    t0="$(now_ms)"
    cli --timeout 60 transcript "$FIRST_SESSION" >/dev/null 2>>"$SCRATCH/serve.log" || true
    t1="$(now_ms)"
    REOPEN_MS=$((t1 - t0))
else
    # Not a warning. A silently unmeasured number is how a harness reports
    # health it never checked; the parse above is the only thing standing
    # between "reopen is fast" and "reopen was never timed".
    echo "no session id parsed from the run stream — reopen cannot be measured" >&2
    echo "(the stream's envelope field is \`session_id\`; check it has not been renamed)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# (d) Idle creep
# ---------------------------------------------------------------------------

# (d) Steady state, and what idling did to it
#
# Sessions stay open throughout. A host that keeps growing here with no work
# arriving has a timer-driven leak; one that falls has finished releasing what
# the turns held. Both are worth knowing, and only the first is a failure.
echo "idling ${IDLE_SECONDS}s …" >&2
sleep "$IDLE_SECONDS"
STEADY_KB="$(rss_kb "$SERVE_PID")"

IDLE_DELTA_KB=$((STEADY_KB - PEAK_KB))
RETENTION_KB=$((STEADY_KB - BOOT_KB))
RETENTION_MULTIPLE="$(awk -v kb="$RETENTION_KB" -v b="$STREAMED_BYTES" \
    'BEGIN { printf "%.2f", (kb * 1024) / b }')"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

cat <<REPORT_EOF

mem-smoke — $PLATFORM, $PROFILE, $CHATS chats x $((CHUNKS * CHUNK_BYTES)) bytes streamed

  boot_baseline_kb          $BOOT_KB
  peak_kb                   $PEAK_KB    (right after the last reply)
  steady_state_kb           $STEADY_KB    (after ${IDLE_SECONDS}s idle)
  streamed_bytes            $STREAMED_BYTES

  peak_retention_multiple   ${PEAK_RETENTION_MULTIPLE}x
  retention_multiple        ${RETENTION_MULTIPLE}x   <- the one that matters
  idle_delta_kb             $IDLE_DELTA_KB   (negative = released)
  reopen_ms                 $REOPEN_MS

REPORT_EOF

if [ -n "$JSON_OUT" ]; then
    cat >"$JSON_OUT" <<JSON_EOF
{
  "platform": "$PLATFORM",
  "profile": "$PROFILE",
  "chats": $CHATS,
  "bytes_per_reply": $((CHUNKS * CHUNK_BYTES)),
  "boot_baseline_kb": $BOOT_KB,
  "peak_kb": $PEAK_KB,
  "steady_state_kb": $STEADY_KB,
  "streamed_bytes": $STREAMED_BYTES,
  "peak_retention_kb": $PEAK_RETENTION_KB,
  "peak_retention_multiple": $PEAK_RETENTION_MULTIPLE,
  "retention_kb": $RETENTION_KB,
  "retention_multiple": $RETENTION_MULTIPLE,
  "idle_delta_kb": $IDLE_DELTA_KB,
  "reopen_ms": $REOPEN_MS,
  "idle_seconds": $IDLE_SECONDS
}
JSON_EOF
    echo "wrote $JSON_OUT" >&2
fi

if [ "$CHECK" -eq 1 ]; then
    failed=0
    over() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a > b) }'; }

    if over "$RETENTION_MULTIPLE" "$MAX_RETENTION"; then
        echo "FAIL retention_multiple ${RETENTION_MULTIPLE}x > ${MAX_RETENTION}x" >&2
        failed=1
    fi
    # Only positive growth is a failure. A large negative delta is the host
    # releasing what its turns held, which is the behaviour being asked for —
    # gating on absolute movement would fail the good outcome.
    if over "$IDLE_DELTA_KB" "$MAX_IDLE_GROWTH_KB"; then
        echo "FAIL idle_delta_kb +$IDLE_DELTA_KB > $MAX_IDLE_GROWTH_KB (grew while idle)" >&2
        failed=1
    fi
    if [ "$REOPEN_MS" -ge 0 ] && over "$REOPEN_MS" "$MAX_REOPEN_MS"; then
        echo "FAIL reopen_ms $REOPEN_MS > $MAX_REOPEN_MS" >&2
        failed=1
    fi

    if [ "$failed" -eq 1 ]; then
        echo >&2
        echo "A threshold moved. Before widening it, read the 'Variance' note at the" >&2
        echo "top of this script — and if the number is a real regression, the phase" >&2
        echo "that caused it owns the fix, not the threshold." >&2
        exit 1
    fi
    echo "all thresholds met" >&2
fi
