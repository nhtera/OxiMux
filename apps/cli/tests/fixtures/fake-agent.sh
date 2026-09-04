#!/bin/sh
# A stand-in for a real agent CLI, speaking just enough stream-json for the
# headless launcher to adopt it as a session.
#
# It exists to answer one question no in-process stub can: does an agent this
# host spawned actually *receive* the confined credential, and does that
# credential reach exactly one session? Everything below is in service of that.
#
# The host inherits its environment into us, so the test hands over what we
# need through it:
#
#   OXIMUX_FAKE_AGENT_SESSION  the session id to announce, when the host names none
#   OXIMUX_FAKE_AGENT_REPORT   file to append findings to
#   OXIMUX_FAKE_AGENT_CLI      path to the oximux binary, for the confinement probe
#   OXIMUX_FAKE_AGENT_DIR      the host's runtime dir, for --dir
#   OXIMUX_FAKE_AGENT_STALL    if set, take the prompt and never answer it —
#                              the turn stays in flight, which is what the
#                              recovery suite needs to catch the host mid-turn
#
# and the *host* hands over OXIMUX_SESSION_ID / OXIMUX_SESSION_TOKEN, which is
# the thing under test. We never write the token anywhere — it is a live
# credential, and a fixture that leaked one into a file a test reads would be
# teaching exactly the wrong habit. Presence is all the test needs.

set -u

report="${OXIMUX_FAKE_AGENT_REPORT:-}"
cli="${OXIMUX_FAKE_AGENT_CLI:-}"
dir="${OXIMUX_FAKE_AGENT_DIR:-}"
sid="${OXIMUX_FAKE_AGENT_SESSION:-fake-session-1}"
# The `--model` we were spawned with, or empty. Recorded because it is the only
# place a test can see that a model reached the *process*: Claude and Codex take
# it here and refuse to change it afterwards, so a host that applies a model as
# a later switch silently leaves them on their default.
spawn_model=""

# Adopt the id the host named, exactly as the CLI we stand in for does.
#
# `--session-id` on a fresh launch, `--resume` on a restore; either way the
# real Claude answers with that id in its `system/init` rather than minting
# one. Ignoring them — announcing the env var regardless — modelled a backend
# that does not exist, and made the session id a race: the host waits a
# bounded moment for an announcement and otherwise keeps the id it chose, so
# under load the suite saw the host's id where it asserted ours. That is a
# fixture bug wearing a product bug's clothes, and it made three tests flaky.
#
# The env var stays as the fallback for a host that names nothing, which is
# what keeps this script runnable on its own.
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
    --model)
        if [ $# -gt 1 ] && [ -n "${2:-}" ]; then
            spawn_model="$2"
            shift
        fi
        ;;
    --model=*)
        spawn_model="${1#*=}"
        ;;
    esac
    shift
done

note() {
    if [ -n "$report" ]; then
        printf '%s\n' "$1" >>"$report"
    fi
}

# 1. What the host handed us, before anything else can disturb it.
if [ -n "${OXIMUX_SESSION_ID:-}" ]; then
    note "session_id=present"
else
    note "session_id=absent"
fi
if [ -n "${OXIMUX_SESSION_TOKEN:-}" ]; then
    note "session_token=present"
else
    note "session_token=absent"
fi

# 1b. Inherited Claude Code session markers. The host scrubs these from its own
#     environment at boot precisely so children never see them — a `claude` that
#     inherits one decides it is a nested child session and stops saving its
#     transcript, which loses the conversation silently. We are that child, so
#     we are the only place the scrub can actually be observed.
leaked=""
for marker in CLAUDECODE CLAUDE_CODE CLAUDE_CODE_CHILD_SESSION \
    CLAUDE_CODE_SESSION_ID CLAUDE_CODE_ENTRYPOINT; do
    eval "value=\${$marker:-}"
    if [ -n "$value" ]; then
        leaked="${leaked}${marker} "
    fi
done
note "leaked_markers=${leaked:-none}"
note "spawn_model=${spawn_model:-none}"

# 2. Announce ourselves. The launcher blocks on this line to learn the session
#    id it will register, so nothing before it may fail.
printf '{"type":"system","subtype":"init","session_id":"%s","model":"fake","permissionMode":"default"}\n' "$sid"

# 3. Wait for the first prompt.
#
#    Also the synchronisation this fixture depends on: the credential is minted
#    under an opaque handle *before* we exist and only re-pointed at the real
#    session id once the host has read the line above. A probe run any earlier
#    would race that rebind and see a fail-closed handle — correct behaviour,
#    but not what this test is asking about. A prompt cannot arrive until the
#    session is registered, so its arrival is the happens-after edge.
while IFS= read -r line; do
    case "$line" in
    '') continue ;;
    *) break ;;
    esac
done

# 4. The stall: ask permission for a tool, then answer nothing.
#
#    The permission request is what makes this state *durably* observable. A
#    turn's in-flight flag is runtime-only — a restored thread always comes back
#    with no turn active — so a stall that only went quiet would leave nothing on
#    disk for the next boot to disagree about. A pending permission does persist,
#    and settling it is exactly what the drain promises: the tool call is marked
#    rejected rather than left waiting for an answer no living process will give.
if [ -n "${OXIMUX_FAKE_AGENT_STALL:-}" ]; then
    printf '{"type":"control_request","request_id":"rid-stall","request":{"subtype":"can_use_tool","tool_name":"Edit","display_name":"Edit","input":{"file_path":"notes.txt","old_string":"a","new_string":"b","replace_all":false},"description":"notes.txt","tool_use_id":"toolu_stall"}}\n'
    note "stalling=1"
    while IFS= read -r _; do :; done
    exit 0
fi

# 5. The probe: what can this confined agent's own CLI reach?
if [ -n "$cli" ] && [ -n "$dir" ]; then
    "$cli" --dir "$dir" --json ls >"${report}.ls" 2>/dev/null
    note "ls_exit=$?"

    # A session-less, full-host surface. A confined agent must be refused it.
    "$cli" --dir "$dir" --json projects ls >/dev/null 2>&1
    note "projects_exit=$?"
fi

# 6. Answer the prompt and end the turn, so the pump folds a complete exchange
#    and the transcript has something to page.
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"fake agent reporting"}]}}\n'
printf '{"type":"result","subtype":"success","result":"done","total_cost_usd":0.0}\n'

# Stay alive so the connection is not torn down mid-assertion; the host kills
# us at drain. Reading to EOF is the portable way to idle without a `sleep`
# loop, and it exits the moment the host closes stdin.
while IFS= read -r _; do :; done
