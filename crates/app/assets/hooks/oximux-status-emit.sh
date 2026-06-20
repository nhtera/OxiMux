#!/bin/sh
# OxiMux agent status sideband emitter.
#
# Usage: oximux-status-emit.sh <state>
#   <state> is one of: working | needs_approval | idle
#
# Installed by OxiMux and referenced from a Claude Code `--settings` hooks
# block (PreToolUse -> working, PermissionRequest -> needs_approval,
# Stop -> idle). On each event Claude Code runs this script with the event's
# JSON on stdin. We extract the tool name (when present) and write an
# OSC-9999 status packet to the controlling terminal:
#
#   ESC ] 9999 ; {"v":1,"state":"working","tool":"Edit"} BEL
#
# OxiMux's PTY scanner reads that packet from the raw output stream and drives
# the agent's structured status (dot, dashboard subline). The escape is
# swallowed by the terminal emulator, so nothing is shown on screen.
#
# IMPORTANT: hook stdout is captured and parsed by Claude Code, so the packet
# MUST go to the controlling TTY, never stdout. The env override
# OXIMUX_HOOK_TTY exists only so tests can redirect to a temp file.

state="$1"
[ -z "$state" ] && exit 0

tty_target="${OXIMUX_HOOK_TTY:-/dev/tty}"

# Extract the tool name for the tool-bearing events. Pure POSIX sed — no jq /
# python dependency. Takes everything up to the next double quote, so an
# embedded quote cannot break out of our JSON string.
tool=""
case "$state" in
    working | needs_approval)
        input=$(cat)
        tool=$(printf '%s' "$input" \
            | sed -n 's/.*"tool_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | head -n 1)
        ;;
esac

if [ -n "$tool" ]; then
    payload=$(printf '{"v":1,"state":"%s","tool":"%s"}' "$state" "$tool")
else
    payload=$(printf '{"v":1,"state":"%s"}' "$state")
fi

# OSC 9999 ; <payload> BEL  (ESC = \033, BEL = \007). Best-effort: a closed or
# unavailable tty must never fail the hook (which would stall the agent).
printf '\033]9999;%s\007' "$payload" > "$tty_target" 2>/dev/null

exit 0
