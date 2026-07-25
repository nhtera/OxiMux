//! Headless end-to-end verification of the ACP embedded-terminal path.
//!
//! Installs a fake [`AcpTerminalHost`], spawns the sibling `mock_acp_terminal_agent`
//! over a real `AcpConnection`, sends one prompt, and asserts the client:
//!   1. advertised `terminal:true` (else the agent's `terminal/create` would be
//!      rejected and no terminal would appear), and
//!   2. emitted a `ToolTerminal` event binding the fake terminal id to the card.
//!
//! Exits non-zero if the flow doesn't complete — a real regression guard for the
//! agents-side wiring (the app's real host + inline render is verified in-GUI).
//!
//! Run: `cargo run -p oximux-agents --example acp_terminal_smoke`

use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use oximux_agents::thread::acp::{
    AcpConnection, AcpTerminalHost, TerminalExitLite, TerminalOutputLite, TerminalSpawnSpec,
    install_terminal_host,
};
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::ThreadEvent;

/// A host that never touches a real PTY: it hands back a canned id, canned
/// output, and an already-exited status — enough to prove the client serves
/// `terminal/*` and emits `ToolTerminal`.
struct FakeHost;

impl AcpTerminalHost for FakeHost {
    fn create(&self, spec: TerminalSpawnSpec) -> std::result::Result<String, String> {
        println!("[host] terminal/create: {} {:?}", spec.command, spec.args);
        Ok("fake-term-1".into())
    }
    fn output(&self, _id: &str) -> std::result::Result<TerminalOutputLite, String> {
        Ok(TerminalOutputLite {
            output: "tick 1\ntick 2\nDONE\n".into(),
            truncated: false,
            exit_status: Some(TerminalExitLite { exit_code: Some(0), signal: None }),
        })
    }
    fn wait_for_exit(&self, _id: &str) -> oneshot::Receiver<TerminalExitLite> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(TerminalExitLite { exit_code: Some(0), signal: None });
        rx
    }
    fn kill(&self, _id: &str) -> std::result::Result<(), String> {
        Ok(())
    }
    fn release(&self, id: &str) -> std::result::Result<(), String> {
        println!("[host] terminal/release: {id}");
        Ok(())
    }
}

fn main() {
    install_terminal_host(Arc::new(FakeHost));

    // The mock agent is our sibling example binary (same target/examples dir).
    let agent_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mock_acp_terminal_agent")))
        .expect("locate mock_acp_terminal_agent");
    if !agent_bin.exists() {
        eprintln!(
            "FAIL: {} not built — run `cargo build -p oximux-agents --example mock_acp_terminal_agent` first",
            agent_bin.display()
        );
        std::process::exit(1);
    }
    let cwd = std::env::current_dir().expect("cwd");
    println!("spawning mock agent: {}", agent_bin.display());
    let (conn, rx) = AcpConnection::spawn(&agent_bin.to_string_lossy(), &[], &cwd, None)
        .expect("spawn AcpConnection");

    let mut prompted = false;
    let mut saw_tool_terminal = false;
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
                        conn.send_user_message("run a command").expect("send");
                    }
                    ThreadEvent::ToolTerminal { terminal_id, .. } => {
                        saw_tool_terminal = terminal_id == "fake-term-1";
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

    if saw_tool_terminal {
        println!("\nPASS: client advertised terminal:true, served terminal/create, and emitted ToolTerminal");
        std::process::exit(0);
    } else {
        println!("\nFAIL: no ToolTerminal(fake-term-1) event observed");
        std::process::exit(1);
    }
}
