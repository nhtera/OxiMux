//! The hook verbs, run as an agent's hook runs them: the compiled binary in a
//! pane's environment (`OXIMUX_PTY_ID`, the user's `HOME`), against a live
//! terminal daemon booted from the real crate in this process.
//!
//! What they must prove is delivery — the frame or banner reaching the pane —
//! because the failure they guard against was silent: the verbs once dialed
//! the desktop's control socket with the relay's protocol, the handshake
//! failed, and every report was swallowed as a hook must.
//!
//! Unix-only: the data root is redirected through `HOME`, which the platform's
//! data-directory lookup honours there and not on Windows.
#![cfg(unix)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Notification, Request, Response};

/// The daemon's files under a redirected `HOME`, where the verbs will look.
fn runtime_dir(home: &Path) -> PathBuf {
    let data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join(".local/share")
    };
    data.join("dev.nhtera.oximux")
}

struct Daemon {
    rt: tokio::runtime::Runtime,
    client: RelayClient,
    pty_id: String,
    // Under `/tmp`, not the platform temp dir: a unix socket path is capped at
    // ~104 bytes, and `$TMPDIR` on macOS alone uses half of that.
    home: tempfile::TempDir,
}

/// A daemon with one pane in it, and a client subscribed to that pane.
fn boot() -> Daemon {
    let home = tempfile::Builder::new().tempdir_in("/tmp").expect("tempdir");
    let dir = runtime_dir(home.path());
    std::fs::create_dir_all(&dir).expect("mkdir");
    let supervisor = oximux_relay_supervisor::RelaySupervisor::new(dir.clone(), dir.join("logs"));
    std::fs::write(supervisor.token_path(), "hook-verbs-token").expect("token");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let (socket, token_file) = (supervisor.socket_path(), supervisor.token_path());
    rt.spawn(async move {
        let _ = oximux_relay::run_server(oximux_relay::ServerConfig::idle_disabled(socket, token_file)).await;
    });
    let socket = supervisor.socket_path();
    let (client, pty_id) = rt.block_on(async {
        let client = 'connect: {
            for _ in 0..300 {
                if let Ok(client) = RelayClient::connect(&socket, "hook-verbs-token").await {
                    break 'connect client;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("relay never came up");
        };
        let spawned = client
            .request(Request::Spawn {
                cwd: "/tmp".into(),
                cols: 80,
                rows: 24,
                shell: Some("/bin/sh".into()),
                args: vec!["-c".into(), "sleep 30".into()],
                env: Vec::new(),
                prefill: Vec::new(),
            })
            .await
            .expect("spawn");
        let Response::SpawnOk { pty_id, .. } = spawned else {
            panic!("spawn answered {spawned:?}");
        };
        (client, pty_id)
    });
    Daemon { rt, client, pty_id, home }
}

/// The CLI as the pane's hook runs it.
fn hook(daemon: &Daemon, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oximux-cli"))
        .args(args)
        .env("HOME", daemon.home.path())
        .env_remove("XDG_DATA_HOME")
        // Run from inside an OxiMux pane, the test runner would otherwise
        // hand the verbs the real relay's socket.
        .env_remove("OXIMUX_SOCKET_PATH")
        .env("OXIMUX_PTY_ID", &daemon.pty_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the CLI");
    child.stdin.take().expect("stdin").write_all(stdin.as_bytes()).expect("write stdin");
    child.wait_with_output().expect("wait")
}

/// The first notification on `rx` that `want` accepts, within 10 s. Subscribe
/// before running the hook: what reaches the pane earlier is not replayed.
fn next_matching(
    daemon: &Daemon,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Notification>,
    want: impl Fn(&Notification) -> bool,
) -> Option<Notification> {
    daemon.rt.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(n)) if want(&n) => return Some(n),
                Ok(None) => return None,
                _ => {}
            }
        }
        None
    })
}

#[test]
fn notify_reaches_the_pane_even_with_a_hooks_extra_argument() {
    let daemon = boot();
    let (_sub, mut rx) = daemon.client.subscribe_pty(&daemon.pty_id);
    // Codex's `notify` hook appends its JSON payload as a trailing argument.
    let out = hook(&daemon, &["notify", "--title", "Build", "--body", "done", r#"{"type":"agent-turn-complete"}"#], "");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "delivered, so nothing to say: {}", String::from_utf8_lossy(&out.stderr));
    let seen = next_matching(&daemon, &mut rx, |n| matches!(n, Notification::Attention { .. }));
    let Some(Notification::Attention { title, body, .. }) = seen else {
        panic!("no attention reached the pane: {seen:?}");
    };
    assert_eq!((title.as_str(), body.as_str()), ("Build", "done"));
}

#[test]
fn agent_status_frames_the_state_on_the_pane() {
    let daemon = boot();
    let (_sub, mut rx) = daemon.client.subscribe_pty(&daemon.pty_id);
    let out = hook(&daemon, &["agent-status", "--state", "idle"], r#"{"hook_event_name":"Stop"}"#);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let framed = next_matching(&daemon, &mut rx, |n| {
        matches!(n, Notification::Output { bytes, .. } if bytes.windows(7).any(|w| w == b"\x1b]9999;"))
    });
    assert!(framed.is_some(), "the status never reached the pane");
}

/// A pane names the relay that spawned it (`OXIMUX_SOCKET_PATH`), which may
/// serve another data root than this machine's default: that one is used.
#[test]
fn the_panes_own_relay_wins_over_the_default_data_root() {
    let daemon = boot();
    let (_sub, mut rx) = daemon.client.subscribe_pty(&daemon.pty_id);
    let socket = oximux_relay_supervisor::RelaySupervisor::new(runtime_dir(daemon.home.path()), PathBuf::new())
        .socket_path();
    let elsewhere = tempfile::Builder::new().tempdir_in("/tmp").expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_oximux-cli"))
        .args(["notify", "--title", "T", "--body", "B"])
        .env("HOME", elsewhere.path())
        .env_remove("XDG_DATA_HOME")
        .env("OXIMUX_SOCKET_PATH", &socket)
        .env("OXIMUX_PTY_ID", &daemon.pty_id)
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(next_matching(&daemon, &mut rx, |n| matches!(n, Notification::Attention { .. })).is_some());
}

#[test]
fn outside_oximux_both_verbs_stay_out_of_the_way() {
    // No daemon and no pane: an agent run from some other terminal. Exit 0
    // either way — a hook must never fail the agent's turn.
    let home = tempfile::Builder::new().tempdir_in("/tmp").expect("tempdir");
    for args in [&["notify", "--title", "x"][..], &["agent-status", "--state", "idle"][..]] {
        let out = Command::new(env!("CARGO_BIN_EXE_oximux-cli"))
            .args(args)
            .env("HOME", home.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("OXIMUX_SOCKET_PATH")
            .env("OXIMUX_PTY_ID", "pane-1")
            .stdin(Stdio::null())
            .output()
            .expect("run");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
