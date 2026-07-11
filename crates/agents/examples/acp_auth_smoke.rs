//! Headless end-to-end verification of the ACP auth flow: a logged-out agent
//! must surface its methods (not a dead-end error), and after the client
//! `authenticate`s the session must open on the SAME connection (no respawn).
//!
//! Spawns the sibling `mock_acp_auth_agent`; asserts an `AuthRequired` event
//! carrying the advertised method arrives, drives `authenticate` with it, and
//! then observes `SessionInit`. Exits non-zero otherwise.
//!
//! Run: `cargo run -p oximux-agents --example acp_auth_smoke`

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use oximux_agents::thread::acp::AcpConnection;
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::ThreadEvent;

fn main() {
    let agent_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mock_acp_auth_agent")))
        .expect("locate mock_acp_auth_agent");
    if !agent_bin.exists() {
        eprintln!(
            "FAIL: {} not built — run `cargo build -p oximux-agents --example mock_acp_auth_agent` first",
            agent_bin.display()
        );
        std::process::exit(1);
    }
    let cwd = std::env::current_dir().expect("cwd");
    println!("spawning mock agent: {}", agent_bin.display());
    let (conn, rx) =
        AcpConnection::spawn(&agent_bin.to_string_lossy(), &[], &cwd, None).expect("spawn AcpConnection");

    let mut saw_auth_required = false;
    let mut authenticated = false;
    let mut saw_session_init = false;
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
                    // The agent needs login → the card's methods arrive. Pick the
                    // first and authenticate (what a pill click does).
                    ThreadEvent::AuthRequired { methods, .. } if !authenticated => {
                        if let Some(first) = methods.first() {
                            saw_auth_required = true;
                            authenticated = true;
                            println!("[client] authenticating with `{}`", first.id);
                            conn.authenticate(&first.id).expect("authenticate");
                        }
                    }
                    // The session opened on the same connection → auth succeeded.
                    ThreadEvent::SessionInit { .. } => {
                        saw_session_init = true;
                        break;
                    }
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

    if saw_auth_required && saw_session_init {
        println!("\nPASS: logged-out agent surfaced methods, authenticate opened the session (no respawn)");
        std::process::exit(0);
    } else {
        println!(
            "\nFAIL: saw_auth_required={saw_auth_required} (want true), saw_session_init={saw_session_init} (want true)"
        );
        std::process::exit(1);
    }
}
