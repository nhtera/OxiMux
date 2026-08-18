#!/bin/sh
# A stand-in agent CLI that STREAMS, for `scripts/mem-smoke.sh`.
#
# Why this exists next to `apps/cli/tests/fixtures/fake-agent.sh` rather than
# replacing it: that fixture answers a security question (does a spawned agent
# receive the confined credential?) and emits exactly one complete `assistant`
# message. A memory harness driven by it would report a streaming-retention
# number of roughly zero no matter how large the workload — the fold never sees
# a partial, so none of the per-delta allocation this plan is about ever
# happens. Extending the security fixture to stream instead would make its
# assertions race a thousand extra lines for no benefit to them.
#
# So: same stream-json dialect, opposite emphasis. Nothing here probes
# anything; it emits a controlled number of `content_block_delta` text deltas
# and then the complete `assistant` message a real Claude sends at block end,
# which is what makes the measured retention comparable to production.
#
# Knobs (read from the environment, since the host owns the argv):
#
#   OXIMUX_MEM_AGENT_CHUNKS       delta events per reply            (default 400)
#   OXIMUX_MEM_AGENT_CHUNK_BYTES  bytes of text per delta           (default 512)
#   OXIMUX_MEM_AGENT_REPORT       file to append `streamed_bytes=N` to
#   OXIMUX_MEM_AGENT_SESSION      session id to announce when the host names none
#
# The payload is deliberately plain ASCII words with no quotes, backslashes or
# newlines, so every line below can be assembled with `printf` and no JSON
# escaping. A payload needing escapes would put a `sed` in the streaming loop,
# which is the one place in this script where cost would distort the number it
# exists to produce.

set -u

chunks="${OXIMUX_MEM_AGENT_CHUNKS:-400}"
chunk_bytes="${OXIMUX_MEM_AGENT_CHUNK_BYTES:-512}"
report="${OXIMUX_MEM_AGENT_REPORT:-}"
sid="${OXIMUX_MEM_AGENT_SESSION:-mem-smoke-session}"

# Adopt the id the host named, exactly as the real CLI does — `--session-id` on
# a fresh launch, `--resume` on a restore. Announcing our own regardless would
# race the host's bounded wait for the announcement; see the long note on the
# same loop in `fake-agent.sh`, which this mirrors on purpose.
while [ $# -gt 0 ]; do
    case "$1" in
    --session-id | --resume)
        if [ $# -gt 1 ] && [ -n "${2:-}" ]; then
            sid="$2"
            shift
        fi
        ;;
    --session-id=* | --resume=*)
        sid="${1#*=}"
        ;;
    esac
    shift
done

# Build one delta payload of exactly $chunk_bytes characters. Doubling rather
# than appending a fixed unit: appending would be O(n) shell string copies for
# a large chunk, and this runs before the timer matters but still inside every
# spawn.
unit="the quick brown fox jumps over the lazy dog while the transcript fold grows "
payload="$unit"
while [ "${#payload}" -lt "$chunk_bytes" ]; do
    payload="$payload$payload"
done
payload="$(printf '%.*s' "$chunk_bytes" "$payload")"

# Announce. The launcher blocks on this line to learn the session id, so
# nothing before it may fail or print.
printf '{"type":"system","subtype":"init","session_id":"%s","model":"mem-smoke","permissionMode":"acceptEdits"}\n' "$sid"

# One reply per PROMPT, for as long as the host keeps sending them. `run` sends
# exactly one; `send` against a live session sends more.
#
# The `"type":"user"` filter is load-bearing, not defensive tidying. The host
# also writes control requests down this same pipe — `run --mode acceptEdits`
# opens the turn with a `set_permission_mode` request — and a fixture that
# treated every non-empty line as a prompt answered twice per session and
# doubled the streamed-byte count, which silently halves the retention multiple
# this whole harness exists to report.
#
# Control requests are read and dropped. The host does not block on a response
# for the one mode request `run` sends (verified against a live serve), and a
# fixture that hand-rolled the control-response protocol would be one more
# thing to keep in step with the real dialect for no measurement benefit. If a
# future flag makes the host wait, this loop is where the reply belongs.
while IFS= read -r line; do
    case "$line" in
    *'"type":"user"'*) ;;
    *) continue ;;
    esac

    printf '{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_mem","type":"message","role":"assistant","content":[]}}}\n'
    printf '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}\n'

    # The streaming loop, and the whole point of the fixture. Each iteration is
    # one `content_block_delta` — the event the fold appends to a live entry and
    # the transcript re-renders on.
    #
    # The full text is accumulated in a FILE, not a shell variable: `text=$text$payload`
    # is a fresh copy of the whole string per iteration, which is O(n^2) and
    # makes a 1MB reply take minutes. Appending to a file is O(n), and `cat`
    # streams it back out for the completed message below.
    i=0
    text_file="$(mktemp)"
    while [ "$i" -lt "$chunks" ]; do
        printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"%s"}}}\n' "$payload"
        printf '%s' "$payload" >>"$text_file"
        i=$((i + 1))
    done

    printf '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}\n'

    # The complete message, as a real backend sends at block end. It is the
    # second full copy of the reply the host handles per turn, and leaving it
    # out would understate retention by roughly half.
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"'
    cat "$text_file"
    printf '"}]}}\n'

    printf '{"type":"result","subtype":"success","result":"done","total_cost_usd":0.0}\n'

    if [ -n "$report" ]; then
        printf 'streamed_bytes=%s\n' "$((chunks * chunk_bytes))" >>"$report"
    fi
    rm -f "$text_file"
done

# Reading to EOF above is also how we idle: the loop returns when the host
# closes stdin at drain, so there is no sleep and no orphan.
