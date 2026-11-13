//! One line saying what a tool call is doing.
//!
//! The file it touches, the command it runs, the query it searches, where it
//! clicks, which agents it addresses. It lives beside [`ToolCall`] rather than
//! in a renderer because three different surfaces need the same sentence and
//! must not word it differently:
//!
//! - the chat card's collapsed header, which is what a person reads before
//!   expanding it;
//! - the **agents row's inline Allow**, where this line *is* the thing being
//!   approved — a row that summarised the arguments its own way would let a
//!   person approve one sentence while the host hashed another;
//! - a phone's permission push (P7), for the same reason.
//!
//! So it takes `(tool_name, input)` rather than a `&ToolCall`: the host holds
//! a pending proposal as those two fields, and a decider must be able to ask
//! what it says without building a transcript entry.
//!
//! [`ToolCall`]: crate::thread::ToolCall

use serde_json::Value;

/// A short human target for the tool line — the file it touches, the command it
/// runs, the query it searches, a click coordinate, or a subagent/task brief.
/// `None` when the input carries none. Kept legible for the collapsed header,
/// which is what the user sees without expanding the card.
pub fn tool_target(tool_name: &str, input: &Value) -> Option<String> {
    // A file the tool touches.
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A Codex `apply_patch` names its files inside `changes[]` rather than at
    // the top level: one file reads as its path, several as a count (the diff
    // rows caption each path individually). Gated by name, not shape — a
    // server-defined MCP tool is free to take an unrelated `changes` array.
    if tool_name == "apply_patch"
        && let Some(changes) = input.get("changes").and_then(Value::as_array).filter(|a| !a.is_empty())
    {
        if let [only] = changes.as_slice()
            && let Some(path) = only.get("path").and_then(Value::as_str)
        {
            return Some(path.to_string());
        }
        return Some(format!("{} files", changes.len()));
    }
    // The command a shell tool runs, minus any wrapper shell.
    if input.get("command").and_then(Value::as_str).is_some() {
        return Some(display_command(input).to_string());
    }
    // A pattern / url / query the tool acts on.
    for key in ["pattern", "url", "query"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A pointer target, e.g. computer-use clicks: `coordinate: [x, y]`.
    if let Some(coord) = input.get("coordinate").and_then(Value::as_array)
        && coord.len() >= 2
    {
        return Some(format!("({}, {})", round_num(&coord[0]), round_num(&coord[1])));
    }
    // A batch of UI actions → a count, not the whole array.
    if let Some(actions) = input.get("actions").and_then(Value::as_array) {
        let n = actions.len();
        return Some(format!("{n} action{}", if n == 1 { "" } else { "s" }));
    }
    // A Codex collab/sub-agent item: its action verb and who it targets, rather
    // than the `prompt` the generic fallback below would pick — the prompt
    // already reads in full on the card body, and "Spawn agent · 2 agents" is
    // what distinguishes two collab cards in a collapsed run. Gated on the item
    // `type` Codex carries on the input, so a Claude `Agent` call (no `type`
    // field) still falls through to its brief.
    if let Some(t) = collab_target(input) {
        return Some(t);
    }
    // Typed text / key chord, or a subagent/task brief → its first line.
    for key in ["text", "description", "prompt"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            let first = s.lines().next().unwrap_or(s);
            if !first.trim().is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// The shell command to present for this input: the script the agent actually
/// ran, rather than the login shell Codex wraps every exec in (`/bin/zsh -lc
/// '<script>'`), which is noise on every command card.
///
/// Codex reports the script itself in `commandActions`, so we read that instead
/// of parsing it back out of the wrapper — recovering it from the quoted string
/// would mean re-implementing shell quoting, and Codex switches between `'` and
/// `"` depending on the script's own quotes. Falls back to the raw command, so
/// a provider that already sends a bare command (Claude) is untouched. The raw
/// wrapper stays on the input for the copy and raw-JSON views.
pub fn display_command(input: &Value) -> &str {
    let raw = input.get("command").and_then(Value::as_str).unwrap_or_default();
    unwrapped_command(input).unwrap_or(raw)
}

/// The lone script `commandActions` reports for this exec, if it reports
/// exactly one. Several actions are left alone: re-joining them would mean
/// guessing the operator that ran between them, and `a && b` shown as two lines
/// reads as something the agent didn't do.
fn unwrapped_command(input: &Value) -> Option<&str> {
    let actions = input.get("commandActions")?.as_array()?;
    let [only] = actions.as_slice() else { return None };
    only.get("command").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// The header target for a Codex collab (`collabAgentToolCall`) or sub-agent
/// activity (`subAgentActivity`) card. `None` for any other input, including a
/// Claude subagent call.
///
/// A collab call reads as its action plus the number of agents it addresses
/// ("Spawn agent · 2 agents"); a single-agent call stays just the verb, since
/// "1 agent" is what every collab call that isn't a broadcast looks like. An
/// activity beat reads as the agent it belongs to.
fn collab_target(input: &Value) -> Option<String> {
    match input.get("type").and_then(Value::as_str)? {
        "collabAgentToolCall" => {
            let verb = input.get("tool").and_then(Value::as_str).map(collab_tool_label);
            let receivers = input
                .get("receiverThreadIds")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            match (verb, receivers) {
                (Some(v), n) if n > 1 => Some(format!("{v} · {n} agents")),
                (Some(v), _) => Some(v),
                (None, n) if n > 0 => Some(format!("{n} agents")),
                (None, _) => None,
            }
        }
        "subAgentActivity" => {
            let who = input
                .get("agentPath")
                .and_then(Value::as_str)
                .or_else(|| input.get("agentThreadId").and_then(Value::as_str))?;
            match input.get("kind").and_then(Value::as_str) {
                Some(kind) => Some(format!("{who} · {kind}")),
                None => Some(who.to_string()),
            }
        }
        _ => None,
    }
}

/// The humanized verb for a Codex collab action. An unknown future action
/// renders verbatim rather than as a guessed verb.
fn collab_tool_label(tool: &str) -> String {
    match tool {
        "spawnAgent" => "Spawn agent".to_string(),
        "sendInput" => "Send input".to_string(),
        "resumeAgent" => "Resume agent".to_string(),
        "wait" => "Wait".to_string(),
        "closeAgent" => "Close agent".to_string(),
        other => other.to_string(),
    }
}

/// A JSON number rendered as a whole integer (drops `.0` on coordinates).
fn round_num(v: &Value) -> String {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_f64().map(|f| (f.round() as i64).to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_target_prefers_file_then_command_then_pattern() {
        assert_eq!(tool_target("Edit", &json!({"file_path": "a.rs"})), Some("a.rs".to_string()));
        assert_eq!(tool_target("Bash", &json!({"command": "ls -la"})), Some("ls -la".to_string()));
        assert_eq!(tool_target("Grep", &json!({"pattern": "foo"})), Some("foo".to_string()));
        assert_eq!(tool_target("Task", &json!({"x": 1})), None);
    }

    #[test]
    fn display_command_unwraps_the_codex_login_shell() {
        // Wire shapes captured from `codex app-server` 0.144.3. Note the quote
        // style flips with the script's own quotes — which is exactly why the
        // structured action is read instead of the wrapper being parsed.
        let single = json!({
            "command": "/bin/zsh -lc 'echo hello'",
            "commandActions": [{"type": "unknown", "command": "echo hello"}]
        });
        assert_eq!(display_command(&single), "echo hello");

        let double = json!({
            "command": "/bin/zsh -lc \"sed -n '1,2p' sample.txt\"",
            "commandActions": [
                {"type": "read", "command": "sed -n '1,2p' sample.txt", "path": "/x/sample.txt"}
            ]
        });
        assert_eq!(display_command(&double), "sed -n '1,2p' sample.txt");

        // Operators live inside a single action — the card must show the whole
        // script, not a truncated first clause.
        let compound = json!({
            "command": "/bin/zsh -lc 'echo a && echo b || echo c'",
            "commandActions": [{"type": "unknown", "command": "echo a && echo b || echo c"}]
        });
        assert_eq!(display_command(&compound), "echo a && echo b || echo c");
    }

    #[test]
    fn display_command_falls_back_to_the_raw_command() {
        // Claude sends a bare command with no actions — unchanged.
        let claude = json!({"command": "ls -la"});
        assert_eq!(display_command(&claude), "ls -la");
        assert_eq!(tool_target("Bash", &claude), Some("ls -la".to_string()));

        // Several actions are ambiguous to re-join, so the raw wrapper shows
        // rather than a reconstruction that might not be what ran.
        let multi = json!({
            "command": "/bin/zsh -lc 'a; b'",
            "commandActions": [{"command": "a"}, {"command": "b"}]
        });
        assert_eq!(display_command(&multi), "/bin/zsh -lc 'a; b'");

        // A malformed/empty action never blanks the card.
        let empty = json!({"command": "/bin/zsh -lc 'x'", "commandActions": [{"command": "  "}]});
        assert_eq!(display_command(&empty), "/bin/zsh -lc 'x'");
        assert_eq!(display_command(&json!({"file_path": "a.rs"})), "");
    }

    #[test]
    fn tool_target_reads_codex_apply_patch_paths() {
        // One change → its path, like an Edit's `file_path`.
        assert_eq!(
            tool_target("apply_patch", &json!({"changes": [{"path": "a.rs", "kind": {"type": "update"}}]})),
            Some("a.rs".to_string())
        );
        // Several → a count; the per-file paths caption the diff rows instead.
        assert_eq!(
            tool_target("apply_patch", &json!({"changes": [{"path": "a.rs"}, {"path": "b.rs"}]})),
            Some("2 files".to_string())
        );
        // An unrelated tool that happens to take a `changes` array is not a
        // patch — it must not be read as one.
        assert_eq!(
            tool_target("mcp__store__patch_doc", &json!({"changes": [{"path": "/a/b", "op": "replace"}]})),
            None
        );
    }

    #[test]
    fn tool_target_reads_codex_collab_action_and_agent_count() {
        // The collab ACTION verb leads, humanized from the wire enum…
        assert_eq!(
            tool_target(
                "Agent",
                &json!({"type": "collabAgentToolCall", "tool": "spawnAgent",
                        "receiverThreadIds": ["t1"]})
            ),
            Some("Spawn agent".to_string())
        );
        // …with the receiver count only when it addresses more than one agent.
        assert_eq!(
            tool_target(
                "Agent",
                &json!({"type": "collabAgentToolCall", "tool": "sendInput",
                        "receiverThreadIds": ["t1", "t2"]})
            ),
            Some("Send input · 2 agents".to_string())
        );
        // The verb beats the `prompt` the generic fallback would otherwise pick —
        // the prompt reads on the card body instead.
        assert_eq!(
            tool_target(
                "Agent",
                &json!({"type": "collabAgentToolCall", "tool": "spawnAgent",
                        "receiverThreadIds": [], "prompt": "Verify the parser"})
            ),
            Some("Spawn agent".to_string())
        );
        // An unknown future action renders verbatim, never as a guessed verb.
        assert_eq!(
            tool_target(
                "Agent",
                &json!({"type": "collabAgentToolCall", "tool": "teleportAgent",
                        "receiverThreadIds": []})
            ),
            Some("teleportAgent".to_string())
        );
        // No `tool` on the wire → fall back to who it targets.
        assert_eq!(
            tool_target("Agent", &json!({"type": "collabAgentToolCall", "receiverThreadIds": ["t1", "t2"]})),
            Some("2 agents".to_string())
        );
        // An activity beat identifies its agent.
        assert_eq!(
            tool_target(
                "Agent activity",
                &json!({"type": "subAgentActivity", "agentPath": "reviewer",
                        "agentThreadId": "t7", "kind": "started"})
            ),
            Some("reviewer · started".to_string())
        );
        // A Claude subagent call carries no item `type` — it must keep reading as
        // its brief rather than being treated as a collab item.
        assert_eq!(
            tool_target("Task", &json!({"description": "Audit the parser", "prompt": "Check X"})),
            Some("Audit the parser".to_string())
        );
    }

    #[test]
    fn tool_target_covers_mcp_and_agent_shapes() {
        // computer-use click → coordinate tuple
        assert_eq!(
            tool_target("mcp__computer-use__left_click", &json!({"coordinate": [948, 810]})),
            Some("(948, 810)".to_string())
        );
        // computer-use batch → action count
        assert_eq!(
            tool_target("mcp__computer-use__computer_batch", &json!({"actions": [{}, {}, {}]})),
            Some("3 actions".to_string())
        );
        // web fetch → url; search → query
        assert_eq!(
            tool_target("WebFetch", &json!({"url": "https://x.dev"})),
            Some("https://x.dev".to_string())
        );
        assert_eq!(
            tool_target("WebSearch", &json!({"query": "gpui focus"})),
            Some("gpui focus".to_string())
        );
        // Agent → first line of the prompt
        assert_eq!(
            tool_target("Agent", &json!({"prompt": "Find the bug\nin detail"})),
            Some("Find the bug".to_string())
        );
    }
}
