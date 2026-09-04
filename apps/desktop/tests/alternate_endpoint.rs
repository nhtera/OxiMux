//! The provider-env phase's headline criterion, proven end to end against a
//! real socket and a real child process rather than asserted about.
//!
//! **Why this lives in its own test binary.** It prepends a directory to the
//! process `PATH` so the agent binary resolves to a shim, which is the same
//! mechanism a real install is found by. `std::env::set_var` mutates state
//! shared by every thread in the process, so doing it inside the 1800-test lib
//! suite would be a data race against whatever else is running — flaky in both
//! directions, and unsound regardless of whether it happens to pass. One test,
//! one binary, one mutation, restored before the assertions.
//!
//! **What is and is not stubbed.** Only the far side of the socket. The
//! settings type, the env resolution, `ConnectSpec`, the StreamJson connect
//! arm, the spawn, and the endpoint are all real. The shim stands in for the
//! agent CLI, and it is the honest stand-in: it does the one thing the criterion
//! is about — read `ANTHROPIC_BASE_URL` from its environment and send traffic
//! there.
//!
//! The settings → `ChatBackend` leg is covered separately by
//! `workspace_root::tests::two_profiles_of_one_adapter_resolve_to_different_chat_env`
//! (that resolver is crate-private, so an integration test cannot call it).
//! Here the backend is built from `AgentLaunchSettings::env_for`, which is the
//! public accessor that resolver delegates to.

use std::io::{Read as _, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use oximux_agents::thread::{ChatBackend, ConnectSpec, ThreadEvent, Transport, connect};
use oximux_settings::AgentLaunchSettings;

/// What the alternate endpoint answers with. The turn has to carry it back, so
/// a pass cannot be explained by anything other than the round trip happening.
const MARKER: &str = "ALTERNATE-ENDPOINT-ANSWERED";

/// What the shim reports when `ANTHROPIC_BASE_URL` is absent — the negative
/// control's expected value, and the value the positive case would produce if
/// the env map were ever dropped on the way to the spawn.
const NO_ENDPOINT: &str = "NO_ENDPOINT";

/// A one-shot HTTP endpoint on loopback. Returns its base URL and a receiver
/// that yields the raw request bytes if one arrives.
fn alternate_endpoint() -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{MARKER}",
                    MARKER.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}"), rx, handle)
}

/// Write an executable `claude` into `dir` that asks whatever
/// `$ANTHROPIC_BASE_URL` names what it has to say, and reports the answer as
/// its turn result in the stream-json the decoder expects.
///
/// With no base URL set, `curl` is handed an empty target, fails, and the
/// result is [`NO_ENDPOINT`] — which is what makes the negative control below
/// able to tell "env arrived" from "env did not".
fn write_claude_shim(dir: &std::path::Path) {
    let shim = dir.join("claude");
    std::fs::write(
        &shim,
        "#!/bin/sh\n\
         answer=$(curl -s --max-time 10 \"$ANTHROPIC_BASE_URL/v1/messages\" 2>/dev/null)\n\
         [ -z \"$answer\" ] && answer=NO_ENDPOINT\n\
         printf '%s\\n' \
           '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"model\":\"m\",\"permissionMode\":\"default\"}'\n\
         printf '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"%s\"}\\n' \"$answer\"\n",
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Run one turn through the real connect path and return its result text.
fn run_turn(env: Vec<(String, String)>) -> Option<String> {
    let backend = ChatBackend {
        transport: Transport::StreamJson,
        acp_command: None,
        acp_args: Vec::new(),
        env,
        adapter_id: Some("claude-code".into()),
        profile: None,
    };
    let spec = ConnectSpec::for_backend(&backend, std::env::temp_dir(), None, None, None, None);
    let (_conn, rx) = connect(spec).expect("spawn through the real StreamJson arm");

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ThreadEvent::TurnEnded { result, .. }) => return result,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    None
}

/// An adapter pointed at an alternate base URL **from settings alone** launches,
/// sends its traffic there, and completes a turn carrying that endpoint's answer
/// — and the same adapter without that setting does not.
///
/// Both halves are one test because they share a single `PATH` mutation; see
/// the module docs. The negative control is the point: without it, a positive
/// that passes for the wrong reason (a hardcoded default, an ambient env var
/// already in the runner's environment) is indistinguishable from a real one.
#[test]
#[cfg_attr(not(unix), ignore = "the shim is a /bin/sh script")]
fn an_adapter_reaches_an_alternate_base_url_configured_only_in_settings() {
    // Probed by running it rather than via a `which` dependency — this is the
    // only thing the shim needs beyond `sh`.
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: no curl available to drive the shim");
        return;
    }

    let shim_dir = tempfile::tempdir().expect("tempdir");
    write_claude_shim(shim_dir.path());

    // `program_for_spawn` resolves the agent binary with `which`, reading this
    // process's PATH — so prepending the shim dir is exactly the mechanism that
    // finds a real install, not a test-only hook.
    let prior_path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: this binary contains one test, so nothing else is running.
    unsafe {
        std::env::set_var("PATH", format!("{}:{prior_path}", shim_dir.path().display()));
    }

    // ── negative control, first: no configured env, no endpoint reached ──
    let bare = run_turn(Vec::new());

    // ── the real thing: the base URL exists only in a settings blob ──────
    let (base_url, hits, server) = alternate_endpoint();
    let mut settings = AgentLaunchSettings::default();
    settings
        .entry_mut("claude-code")
        .env
        .insert("ANTHROPIC_BASE_URL".into(), base_url.clone());
    // The public accessor the crate-private backend resolver delegates to.
    let resolved = settings.env_for("claude-code", None);
    let configured = run_turn(resolved.clone());

    // SAFETY: restore before asserting so a failure can't leak a mutated PATH.
    unsafe { std::env::set_var("PATH", &prior_path) };

    assert_eq!(
        resolved,
        vec![("ANTHROPIC_BASE_URL".to_string(), base_url.clone())],
        "settings must resolve to the env the spawn is handed"
    );
    assert_eq!(
        bare.as_deref(),
        Some(NO_ENDPOINT),
        "control: with no configured env the agent must reach no endpoint — if this \
         says {MARKER}, the positive case below proves nothing"
    );

    let request = hits
        .recv_timeout(Duration::from_secs(5))
        .expect("the alternate endpoint must have received a request");
    assert!(
        request.starts_with("GET /v1/messages"),
        "traffic must go to the configured base URL's path: {request:?}"
    );
    assert_eq!(
        configured.as_deref(),
        Some(MARKER),
        "the turn must complete carrying the alternate endpoint's own answer"
    );
    let _ = server.join();
}
