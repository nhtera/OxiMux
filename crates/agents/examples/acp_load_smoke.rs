//! Headless end-to-end verification of the `session/load` restore path: when the
//! agent advertises `loadSession`, the client must resume via `session/load` with
//! the stored id, SUPPRESS the agent's replayed history (OxiMux repaints its own
//! blob), keep the same session id, and still render live updates afterward.
//!
//! Spawns the sibling `mock_acp_load_agent` with a resume id; asserts the
//! `REPLAYED_HISTORY` chunk is NOT surfaced as an event, the `LIVE_REPLY` from a
//! post-load prompt IS, and `SessionInit` carries the resumed id. Exits non-zero
//! otherwise.
//!
//! Run: `cargo run -p oximux-agents --example acp_load_smoke`

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use oximux_agents::thread::acp::AcpConnection;
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::ThreadEvent;

const RESUME_ID: &str = "known-session";

fn main() {
    let agent_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mock_acp_load_agent")))
        .expect("locate mock_acp_load_agent");
    if !agent_bin.exists() {
        eprintln!(
            "FAIL: {} not built — run `cargo build -p oximux-agents --example mock_acp_load_agent` first",
            agent_bin.display()
        );
        std::process::exit(1);
    }
    let cwd = std::env::current_dir().expect("cwd");
    println!("spawning mock agent: {}", agent_bin.display());
    let (conn, rx) = AcpConnection::spawn(
        &agent_bin.to_string_lossy(),
        &[],
        &cwd,
        Some(RESUME_ID.to_string()),
    )
    .expect("spawn AcpConnection");

    let mut prompted = false;
    let mut saw_replayed = false;
    let mut saw_live = false;
    let mut resumed_id = String::new();
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
                    ThreadEvent::SessionInit { session_id, .. } if !prompted => {
                        resumed_id = session_id;
                        prompted = true;
                        conn.send_user_message("continue where we left off").expect("send");
                    }
                    ThreadEvent::AssistantTextDelta(t) => {
                        if t.contains("REPLAYED_HISTORY") {
                            saw_replayed = true;
                        }
                        if t.contains("LIVE_REPLY") {
                            saw_live = true;
                        }
                    }
                    ThreadEvent::TurnEnded { .. } if prompted => break,
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

    let id_ok = resumed_id == RESUME_ID;
    if !saw_replayed && saw_live && id_ok {
        println!(
            "\nPASS: session/load resumed id `{resumed_id}`, suppressed replayed history, rendered live reply"
        );
        std::process::exit(0);
    } else {
        println!(
            "\nFAIL: saw_replayed={saw_replayed} (want false), saw_live={saw_live} (want true), resumed_id={resumed_id:?} (want {RESUME_ID:?})"
        );
        std::process::exit(1);
    }
}
