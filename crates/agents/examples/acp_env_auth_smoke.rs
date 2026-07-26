//! Headless verification of the ACP EnvVar-auth flow: an agent that wants an
//! ENV-VAR credential must (1) surface the EnvVar method when spawned WITHOUT it,
//! and (2) open the session when RESPAWNED with the var set — proving the env
//! override reaches the child process and the worker's auto-authenticate closes
//! the "set env, then authenticate" loop.
//!
//! Spawns the sibling `mock_acp_env_auth_agent` twice: once bare (asserts an
//! `AuthRequired` carrying the EnvVar method + its variable name), once via
//! `spawn_with_env` with the key set + the method to auto-authenticate (asserts
//! `SessionInit`). Exits non-zero otherwise.
//!
//! Run: `cargo run -p oximux-agents --example acp_env_auth_smoke`

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use oximux_agents::thread::acp::AcpConnection;
use oximux_agents::thread::connection::AgentConnection;
use oximux_agents::thread::event::{AuthMethodKind, ThreadEvent};

const KEY_VAR: &str = "OXIMUX_MOCK_KEY";

fn main() {
    let agent_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mock_acp_env_auth_agent")))
        .expect("locate mock_acp_env_auth_agent");
    if !agent_bin.exists() {
        eprintln!(
            "FAIL: {} not built — run `cargo build -p oximux-agents --example mock_acp_env_auth_agent` first",
            agent_bin.display()
        );
        std::process::exit(1);
    }
    let cwd = std::env::current_dir().expect("cwd");
    let bin = agent_bin.to_string_lossy().to_string();

    // Phase 1 — bare spawn (no env): the agent must surface the EnvVar method with
    // its variable name, NOT a dead-end error.
    println!("[phase 1] spawning WITHOUT the key");
    let (conn, rx) = AcpConnection::spawn(&bin, &[], &cwd, None).expect("spawn bare");
    let mut env_var_surfaced = false;
    if let Some(ThreadEvent::AuthRequired { methods, .. }) = wait_for_auth_or_init(&rx) {
        println!("[phase 1] AuthRequired methods: {methods:?}");
        env_var_surfaced = methods.iter().any(|m| {
            matches!(&m.kind, AuthMethodKind::EnvVar { vars, .. } if vars.iter().any(|v| v == KEY_VAR))
        });
    }
    conn.shutdown();
    drop(conn);

    // Phase 2 — respawn WITH the key set + the method to auto-authenticate: the
    // session must open (proves the env override reached the child, and the worker
    // authenticated the seeded method without re-prompting).
    println!("[phase 2] respawning WITH the key + auto-authenticate");
    let (conn, rx) = AcpConnection::spawn_with_env(
        &bin,
        &[],
        &cwd,
        None,
        vec![(KEY_VAR.to_string(), "s3cr3t".to_string())],
        Some("api-key".to_string()),
        Vec::new(),
    )
    .expect("spawn with env");
    let session_opened =
        matches!(wait_for_auth_or_init(&rx), Some(ThreadEvent::SessionInit { .. }));
    conn.shutdown();
    drop(conn);

    if env_var_surfaced && session_opened {
        println!("\nPASS: EnvVar method surfaced bare; respawn-with-env opened the session");
        std::process::exit(0);
    } else {
        println!(
            "\nFAIL: env_var_surfaced={env_var_surfaced} (want true), session_opened={session_opened} (want true)"
        );
        std::process::exit(1);
    }
}

/// Drain events until the first `AuthRequired` or `SessionInit` (or an error /
/// timeout), returning it. Ignores pre-handshake chatter.
fn wait_for_auth_or_init(rx: &Receiver<ThreadEvent>) -> Option<ThreadEvent> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("TIMEOUT");
            return None;
        }
        match rx.recv_timeout(remaining.min(Duration::from_secs(2))) {
            Ok(ev @ ThreadEvent::AuthRequired { .. }) => return Some(ev),
            Ok(ev @ ThreadEvent::SessionInit { .. }) => return Some(ev),
            Ok(ThreadEvent::Error(e)) => {
                println!("!!! error: {e}");
                return None;
            }
            Ok(ev) => println!("(ignoring {ev:?})"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}
