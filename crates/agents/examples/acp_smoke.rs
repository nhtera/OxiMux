//! Throwaway live smoke test for the ACP (Gemini) connection (not shipped).
//!
//! Spawns `gemini --experimental-acp`, sends one prompt, and prints every decoded
//! `ThreadEvent` until the turn ends (or a timeout). Proves the Phase-1 lifecycle
//! round-trip against a real agent.
//!
//! Run: `cargo run -p oximux-agents --example acp_smoke`

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use oximux_agents::thread::acp::AcpConnection;
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::ThreadEvent;

fn main() {
    let cwd = std::env::current_dir().expect("cwd");
    // Retargetable: `ACP_CMD="npx -y @agentclientprotocol/codex-acp@latest" …`.
    let cmdline = std::env::var("ACP_CMD")
        .unwrap_or_else(|_| "gemini --experimental-acp".to_string());
    let parts: Vec<String> = cmdline.split_whitespace().map(str::to_string).collect();
    let (command, args) = parts.split_first().expect("empty ACP_CMD");
    println!("spawning `{cmdline}` in {}", cwd.display());

    let (conn, rx) =
        AcpConnection::spawn(command, args, &cwd).expect("spawn AcpConnection");

    let mut prompted = false;
    let deadline = Instant::now() + Duration::from_secs(90);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("TIMEOUT");
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_secs(5))) {
            Ok(ev) => {
                println!("EVENT: {ev:?}");
                match ev {
                    ThreadEvent::SessionInit { .. } if !prompted => {
                        prompted = true;
                        let prompt = std::env::var("ACP_PROMPT")
                            .unwrap_or_else(|_| "Reply with exactly one word: pong".to_string());
                        println!(">>> sending prompt: {prompt}");
                        conn.send_user_message(&prompt).expect("send");
                    }
                    ThreadEvent::TurnEnded { .. } => {
                        println!("<<< turn ended");
                        break;
                    }
                    ThreadEvent::Error(e) => {
                        println!("!!! error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !prompted {
                    println!("(waiting for SessionInit…)");
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                println!("channel closed");
                break;
            }
        }
    }

    println!(">>> shutdown; polling for reap (up to 6s)");
    conn.shutdown();
    let reap_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let out = std::process::Command::new("pgrep")
            .args(["-f", "experimental-acp"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if out.is_empty() {
            println!("REAPED cleanly (no gemini process)");
            break;
        }
        if Instant::now() >= reap_deadline {
            println!("ORPHAN remains: {out}");
            break;
        }
    }
    // Keep `conn` alive until here so its Drop doesn't race the poll above.
    drop(conn);
    println!("done");
}
