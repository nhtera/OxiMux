//! Headless end-to-end verification that a client `session/cancel` resolves an
//! agent's outstanding `session/request_permission` with a `Cancelled` outcome
//! (rather than leaving it parked forever — the round-2 wedge fix).
//!
//! Spawns the sibling `mock_acp_cancel_agent` over a real `AcpConnection`, sends
//! one prompt, and when the resulting permission card appears, calls
//! `AcpConnection::cancel`. The mock agent reports the outcome it received as an
//! assistant message; this driver asserts it was `cancelled`. Exits non-zero if
//! the outcome is anything else or the flow times out.
//!
//! Run: `cargo run -p oximux-agents --example acp_cancel_smoke`

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use oximux_agents::thread::acp::AcpConnection;
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::ThreadEvent;

fn main() {
    let agent_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mock_acp_cancel_agent")))
        .expect("locate mock_acp_cancel_agent");
    if !agent_bin.exists() {
        eprintln!(
            "FAIL: {} not built — run `cargo build -p oximux-agents --example mock_acp_cancel_agent` first",
            agent_bin.display()
        );
        std::process::exit(1);
    }
    let cwd = std::env::current_dir().expect("cwd");
    println!("spawning mock agent: {}", agent_bin.display());
    let (conn, rx) = AcpConnection::spawn(&agent_bin.to_string_lossy(), &[], &cwd, None)
        .expect("spawn AcpConnection");

    let mut prompted = false;
    let mut cancelled = false;
    let mut saw_cancelled_outcome = false;
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("TIMEOUT");
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_secs(2))) {
            Ok(ev) => {
                println!("EVENT: {ev:?}");
                match ev {
                    ThreadEvent::SessionInit { .. } if !prompted => {
                        prompted = true;
                        conn.send_user_message("do something dangerous").expect("send");
                    }
                    // The agent parked a permission request → Stop mid-request.
                    ThreadEvent::PermissionRequested { .. } if !cancelled => {
                        cancelled = true;
                        println!("[client] cancelling while a permission is pending");
                        conn.cancel().expect("cancel");
                    }
                    // The agent reports the outcome it received back on the wire.
                    ThreadEvent::AssistantTextDelta(t) => {
                        if t.contains("PERMISSION_OUTCOME: cancelled") {
                            saw_cancelled_outcome = true;
                        }
                    }
                    ThreadEvent::TurnEnded { .. } => break,
                    ThreadEvent::Error(e) => {
                        println!("!!! error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    conn.shutdown();
    drop(conn);

    if saw_cancelled_outcome {
        println!("\nPASS: cancel resolved the pending permission with a Cancelled outcome");
        std::process::exit(0);
    } else {
        println!("\nFAIL: agent did not receive a Cancelled outcome after client cancel");
        std::process::exit(1);
    }
}
