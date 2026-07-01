//! Throwaway spike probe for the Claude Code `stream-json` headless transport.
//!
//! Drives a real `claude` subprocess in the persistent streaming mode used by the
//! future Agent Chat UI, and prints what a parser must handle: the event vocabulary,
//! streaming deltas, tool calls, and the interactive permission control-protocol.
//!
//! This is NOT shipped code — it is a reference harness captured during Phase 1 of
//! plans/260701-0314-oximux-agent-chat-ui-claude. Run against a THROWAWAY repo:
//!
//!   cargo run -p oximux-agents --example stream_json_probe -- /tmp/scratch-repo \
//!       "Use the Edit tool to change 'a' to 'b' in notes.txt, then confirm."
//!
//! Uses std::process (no tokio feature coupling) so it compiles regardless of the
//! crate's async feature set; the real connection (Phase 2) will use the app's executor.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

use serde_json::{json, Value};

const IDLE_SECS: u64 = 120;

fn main() {
    let mut args = std::env::args().skip(1);
    let repo = args.next().unwrap_or_else(|| ".".to_string());
    let prompt = args.next().unwrap_or_else(|| {
        "List the files in this directory using a tool, then summarize in one line.".to_string()
    });

    // The confirmed launch line for a persistent, structured, interactive Claude session.
    // --permission-prompt-tool stdio  => tool approvals arrive as `can_use_tool` control_requests.
    // --setting-sources project        => isolate from the user's global ~/.claude hooks, which
    //                                     otherwise corrupt the permission round-trip.
    let mut child: Child = Command::new("claude")
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--permission-prompt-tool",
            "stdio",
            "--setting-sources",
            "project",
        ])
        .current_dir(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude (is it on PATH?)");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Background reader thread -> channel of raw JSON lines (mirrors the Phase-2 !Send->Send shim).
    let (tx, rx) = mpsc::channel::<Option<String>>();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(Some(l)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None);
    });

    let send = |stdin: &mut std::process::ChildStdin, v: &Value| {
        let _ = writeln!(stdin, "{}", v);
        let _ = stdin.flush();
    };

    // Turn 1: the user prompt as a stream-json user message.
    send(
        &mut stdin,
        &json!({"type":"user","message":{"role":"user","content": prompt}}),
    );

    let mut event_counts: std::collections::BTreeMap<String, u32> = Default::default();
    let mut tool_calls = 0u32;
    let mut permission_requests = 0u32;
    let mut assistant_text = String::new();
    let mut session_id = String::new();

    loop {
        let line = match rx.recv_timeout(std::time::Duration::from_secs(IDLE_SECS)) {
            Ok(Some(l)) => l,
            Ok(None) => {
                println!("[stdout closed]");
                break;
            }
            Err(_) => {
                println!("[idle timeout]");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                println!("[non-json] {}", &line[..line.len().min(120)]);
                continue;
            }
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("?");
        let key = match ty {
            "stream_event" => format!(
                "stream_event:{}",
                v["event"]["type"].as_str().unwrap_or("?")
            ),
            "system" => format!("system/{}", v["subtype"].as_str().unwrap_or("?")),
            "result" => format!("result/{}", v["subtype"].as_str().unwrap_or("?")),
            other => other.to_string(),
        };
        *event_counts.entry(key).or_default() += 1;

        match ty {
            "system" if v["subtype"] == "init" => {
                session_id = v["session_id"].as_str().unwrap_or("").to_string();
                println!(
                    "init: session={} model={} permissionMode={}",
                    session_id,
                    v["model"].as_str().unwrap_or("?"),
                    v["permissionMode"].as_str().unwrap_or("?")
                );
            }
            // The clean interactive approval: reply allow with the (possibly updated) input.
            "control_request" if v["request"]["subtype"] == "can_use_tool" => {
                permission_requests += 1;
                let rid = v["request_id"].as_str().unwrap_or("").to_string();
                let tool = v["request"]["tool_name"].as_str().unwrap_or("?");
                let input = v["request"]["input"].clone();
                println!("  PERMISSION can_use_tool: {} suggestions={}", tool, v["request"]["permission_suggestions"]);
                // AUTO-ALLOW for the spike (the real UI shows Allow/Reject buttons here).
                send(
                    &mut stdin,
                    &json!({"type":"control_response","response":{
                        "subtype":"success","request_id":rid,
                        "response":{"behavior":"allow","updatedInput":input}
                    }}),
                );
            }
            "assistant" => {
                if let Some(content) = v["message"]["content"].as_array() {
                    for b in content {
                        match b["type"].as_str() {
                            Some("tool_use") => {
                                tool_calls += 1;
                                println!(
                                    "  tool_use: {} {}",
                                    b["name"].as_str().unwrap_or("?"),
                                    b["input"].to_string().chars().take(120).collect::<String>()
                                );
                            }
                            Some("text") => {
                                if let Some(t) = b["text"].as_str() {
                                    assistant_text.push_str(t);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "result" => {
                println!(
                    "\nresult: {}",
                    v["result"].as_str().unwrap_or("<none>")
                );
                println!(
                    "cost_usd={} num_turns={} denials={}",
                    v["total_cost_usd"],
                    v["num_turns"],
                    v["permission_denials"].as_array().map(|a| a.len()).unwrap_or(0)
                );
                break;
            }
            _ => {}
        }
    }

    println!("\n=== event histogram ===");
    for (k, n) in &event_counts {
        println!("{:4} {}", n, k);
    }
    println!(
        "tool_calls={} permission_requests={} session_id={}",
        tool_calls, permission_requests, session_id
    );
    if !assistant_text.trim().is_empty() {
        println!("assistant_text: {}", assistant_text.trim());
    }

    let _ = stdin; // dropping stdin closes it -> claude exits after the turn
    drop(child.stdin.take());
    let _ = child.wait();
}
