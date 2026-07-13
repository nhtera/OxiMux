#!/usr/bin/env python3
"""Minimal stdio MCP server that elicits consent on every tool call.

Purpose: exercise the agent-chat MCP-elicitation card end-to-end. Wire it into a
CLI that forwards `elicitation/create` to its client (e.g. Codex), call the one
tool it exposes, and the host should surface an "MCP · elicitation-probe" card;
answering it round-trips the `{action, content}` reply back here.

Codex config (~/.codex/config.toml):

    [mcp_servers.elicit_probe]
    command = "python3"
    args = ["/ABS/PATH/scripts/mcp-elicitation-probe.py"]

Then, in an OxiMux Codex chat, ask the agent to call the `confirm_action` tool.

No third-party deps — line-delimited JSON-RPC 2.0 over stdio. Deliberately tiny;
not a general-purpose server.
"""
import json
import sys

PROTOCOL_VERSION = "2025-06-18"


def send(msg: dict) -> None:
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read() -> dict | None:
    line = sys.stdin.readline()
    if not line:
        return None
    line = line.strip()
    return json.loads(line) if line else read()


def result(req_id, payload) -> None:
    send({"jsonrpc": "2.0", "id": req_id, "result": payload})


def elicit(prompt: str):
    """Send an elicitation/create request to the client and await its reply.

    The client may interleave other traffic; skip anything that isn't the
    response to our request id (a pragmatic loop for a probe, not a full router).
    """
    req_id = "elicit-1"
    send({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "elicitation/create",
        "params": {
            "message": prompt,
            "requestedSchema": {
                "type": "object",
                "properties": {"confirmed": {"type": "boolean"}},
                "required": ["confirmed"],
            },
        },
    })
    while True:
        msg = read()
        if msg is None:
            return {"action": "cancel"}
        if msg.get("id") == req_id:
            return msg.get("result", {"action": "cancel"})
        # A request from the client mid-wait (e.g. ping) — answer minimally.
        if msg.get("method") == "ping":
            result(msg["id"], {})


def main() -> None:
    while True:
        msg = read()
        if msg is None:
            return
        method, req_id = msg.get("method"), msg.get("id")
        if method == "initialize":
            result(req_id, {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "elicitation-probe", "version": "0.1.0"},
            })
        elif method == "notifications/initialized":
            pass  # notification, no reply
        elif method == "tools/list":
            result(req_id, {"tools": [{
                "name": "confirm_action",
                "description": "Ask the user to confirm before proceeding.",
                "inputSchema": {"type": "object", "properties": {}},
            }]})
        elif method == "tools/call":
            reply = elicit("The elicitation probe wants to confirm this action. Proceed?")
            action = reply.get("action", "cancel")
            text = f"elicitation result: action={action} content={reply.get('content')}"
            result(req_id, {"content": [{"type": "text", "text": text}]})
        elif req_id is not None:
            # Unknown request → JSON-RPC method-not-found, so the client doesn't hang.
            send({"jsonrpc": "2.0", "id": req_id,
                  "error": {"code": -32601, "message": f"method not found: {method}"}})


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, KeyboardInterrupt):
        pass
