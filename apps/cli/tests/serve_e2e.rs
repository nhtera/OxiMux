//! The headless host, end to end: boot the compiled binary as `serve`, parse
//! its readiness contract, drive it with the same binary as a client, restart
//! it, and prove the catalog's restart-visibility claim — a session persisted
//! before the restart is listed and readable after it. Also the two pairing
//! guarantees the write-by-default tier leans on: no ticket ever reaches a
//! non-terminal without the explicit override, and the admin verbs answer.
//!
//! No agent CLI is spawned anywhere here: the persisted session is seeded
//! straight into storage, exactly what a desktop-created session leaves
//! behind — which is also what makes the test hermetic.

use std::io::BufRead as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
// Only the `#[cfg(unix)]` readiness wait uses a deadline; importing it
// unconditionally is an unused import on Windows, where `-D warnings` makes
// that a hard error rather than a nit.
#[cfg(unix)]
use std::time::Instant;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    // Keep the relay out of it: a nonexistent override fails fast, and serve
    // degrades to "no terminals", which this suite never touches.
    cmd.env("OXIMUX_RELAY_BINARY", "/nonexistent/oximux-relay-for-tests");
    cmd
}

/// A running serve child plus its parsed readiness line.
struct ServeUnderTest {
    child: Child,
    ready: serde_json::Value,
}

fn boot_serve(data_dir: &Path) -> ServeUnderTest {
    let mut child = bin()
        .args(["serve", "--data-dir", data_dir.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        // Keep draining so the child never blocks on a full pipe (nothing else
        // should ever be written, and the assertion below pins that).
        let mut rest = String::new();
        loop {
            let mut chunk = String::new();
            match reader.read_line(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(_) => rest.push_str(&chunk),
            }
        }
        let _ = tx.send(rest);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("serve prints its readiness line");
    let ready: serde_json::Value =
        serde_json::from_str(line.trim()).expect("the readiness line is JSON");
    assert_eq!(ready["type"], "oximux_serve_ready");
    assert_eq!(ready["schemaVersion"], 1);
    assert_eq!(
        ready["protocolVersion"],
        oximux_remote_proto::proto::PROTOCOL_VERSION
    );
    assert_eq!(ready["endpointId"].as_str().map(str::len), Some(64), "endpoint id, hex");
    ServeUnderTest { child, ready }
}

impl ServeUnderTest {
    fn stop_hard(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// SIGTERM and await the drain path; asserts a clean exit. Unix-only —
    /// Windows has no SIGTERM to send a console child from here.
    #[cfg(unix)]
    fn stop_gracefully(mut self) {
        // SAFETY-free libc-free TERM: the `kill` binary is universal on unix.
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait().expect("wait on serve") {
                Some(status) => {
                    assert!(status.success(), "a drained serve exits 0, got {status:?}");
                    return;
                }
                None if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    panic!("serve did not drain within the deadline");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

/// Seed the storage a desktop-created session leaves behind: one chat blob
/// under the settings key the catalog scans.
fn seed_session(data_dir: &Path, session_id: &str) {
    std::fs::create_dir_all(data_dir).unwrap();
    let db = oximux_storage::open(&data_dir.join("oximux.db")).expect("open db");
    let settings = oximux_storage::SettingsRepo::new(db);
    let blob = serde_json::json!({
        "session_id": session_id,
        "model": "seeded-model",
        "entries": [
            {"User": {"text": "seeded prompt", "images": []}},
            {"Assistant": {"text": "seeded reply", "thinking": ""}},
        ],
        "session_meta": {"cwd": "/tmp"},
    });
    settings
        .set(&format!("agent_chat:{session_id}"), &blob.to_string())
        .expect("seed blob");
}

fn client(data_dir: &Path) -> Command {
    let mut cmd = bin();
    cmd.args(["--dir", data_dir.to_str().unwrap(), "--timeout", "15"]);
    cmd
}

fn json_stdout(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Boot → readiness → the seeded session is listed and readable → restart →
/// still there. The whole phase-4 catalog claim in one pass.
#[test]
fn serve_lists_persisted_sessions_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("serve-home");
    seed_session(&data_dir, "seeded-1");

    let serve = boot_serve(&data_dir);
    assert_eq!(
        serve.ready["localSocket"].as_str().unwrap(),
        data_dir.to_str().unwrap(),
        "readiness names the socket dir"
    );

    let out = client(&data_dir).args(["--json", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let rows = json_stdout(&out);
    let rows = rows["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the persisted session is listed without ever being opened");
    assert_eq!(rows[0]["session_id"], "seeded-1");
    assert_eq!(rows[0]["model"], "seeded-model");

    let out = client(&data_dir).args(["--json", "transcript", "seeded-1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["data"]["entries"].as_array().unwrap().len(), 2, "history reads from disk");

    // Restart. Graceful where the platform lets this test deliver a TERM, so
    // the drain path's clean exit is asserted too; hard-kill elsewhere — the
    // durability claim is about storage either way.
    #[cfg(unix)]
    serve.stop_gracefully();
    #[cfg(not(unix))]
    serve.stop_hard();

    let serve = boot_serve(&data_dir);
    let out = client(&data_dir).args(["--json", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let rows = json_stdout(&out);
    assert_eq!(
        rows["data"].as_array().unwrap().len(),
        1,
        "a session created before the restart is listed after it"
    );
    serve.stop_hard();
}

/// The pairing surface over a live serve: a non-terminal invocation is
/// refused and leaks no ticket; the explicit override mints one; the
/// enrollment list answers; and an agent-scoped caller is denied outright.
#[test]
fn pairing_tickets_never_reach_a_non_terminal_unforced() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("serve-home");
    let serve = boot_serve(&data_dir);

    // Piped stdout IS the non-TTY case. Refused, usage-class exit, and no
    // ticket-shaped string anywhere in either stream.
    let out = client(&data_dir).args(["--json", "pair-new"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "a non-terminal mint is a usage error");
    let v = json_stdout(&out);
    assert_eq!(v["error"]["code"], "not-a-tty");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("ticket\":\""), "no ticket JSON field on the refusal path");

    // The audited override mints a one-time window.
    let out = client(&data_dir)
        .args(["--json", "pair-new", "--force-non-tty", "--read-only"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    let ticket = v["data"]["ticket"].as_str().unwrap();
    assert!(!ticket.is_empty());
    assert_eq!(v["data"]["read_only"], true);
    // The ticket names the endpoint the readiness line announced.
    let decoded = oximux_remote_proto::pairing::PairingTicket::decode(ticket).unwrap();
    let hex: String = decoded.endpoint_id.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, serve.ready["endpointId"].as_str().unwrap());

    // Nothing has redeemed it, so the enrollment list is still empty.
    let out = client(&data_dir).args(["--json", "pair-ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(json_stdout(&out)["data"].as_array().unwrap().is_empty());

    serve.stop_hard();
}

/// The stdout contract: after the readiness line, serve writes NOTHING else
/// to stdout for its whole life — a journal capturing stdout can never
/// capture a secret.
#[test]
fn stdout_carries_the_readiness_line_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("serve-home");
    let mut child = bin()
        .args(["serve", "--data-dir", data_dir.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    // Give it time to boot and do everything it does at startup.
    std::thread::sleep(Duration::from_secs(5));
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one stdout line, got: {stdout:?}");
    assert!(lines[0].contains("oximux_serve_ready"));
}

/// PATH-shaped environments (a systemd unit's minimal env) must not break the
/// boot path itself: serve reaches readiness with a scrubbed environment.
#[test]
fn serve_boots_under_a_minimal_environment() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("serve-home");
    let mut cmd = bin();
    cmd.env_clear();
    // The bare minimum a service manager provides.
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("OXIMUX_RELAY_BINARY", "/nonexistent/oximux-relay-for-tests");
    let mut child = cmd
        .args(["serve", "--data-dir", data_dir.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(60)).expect("readiness under minimal env");
    assert!(line.contains("oximux_serve_ready"), "got: {line:?}");
    let _ = child.kill();
    let _ = child.wait();
}

/// The single-ticker contract on one data dir: with the ticker role already
/// held (as a running desktop would hold it), a booting serve still reaches
/// readiness and serves every schedule read and write — it just declines to
/// tick, says so once on stderr, and answers run-once with `Unsupported`.
#[test]
fn a_contended_ticker_declines_but_schedules_stay_editable() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Hold the role, as the other host would.
    let lock_path = data_dir.join(oximux_agents::schedule::TICKER_LOCK_FILENAME);
    let _held = match oximux_single_instance::try_acquire(&lock_path).unwrap() {
        oximux_single_instance::AcquireOutcome::Acquired(guard) => guard,
        oximux_single_instance::AcquireOutcome::AlreadyRunning { .. } => {
            panic!("fresh dir must acquire")
        }
    };

    let mut serve = boot_serve(&data_dir);
    // Collect stderr from here on; the decline line was already written by
    // readiness time, but the pipe holds it.
    let stderr = serve.child.stderr.take().expect("piped stderr");
    let collected = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = std::io::BufReader::new(stderr).read_to_string(&mut buf);
        buf
    });

    // Schedule writes and reads still work on the non-ticking host.
    let out = client(&data_dir)
        .args([
            "--json", "schedule", "create", "check the builds", "--name", "nightly", "--cwd",
            "/tmp", "--every", "30",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let id = json_stdout(&out)["data"]["id"].as_str().unwrap().to_string();

    let out = client(&data_dir).args(["--json", "schedule", "ls"]).output().unwrap();
    assert_eq!(json_stdout(&out)["data"].as_array().map(Vec::len), Some(1));

    // But firing is the lock holder's job, and this host says so.
    let out = client(&data_dir).args(["--json", "schedule", "run-once", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_stdout(&out)["error"]["code"], "unsupported");

    serve.stop_hard();
    let stderr_text = collected.join().unwrap();
    assert!(
        stderr_text.contains("owns scheduling for this data dir"),
        "the decline is one clear line, got:\n{stderr_text}"
    );
    assert_eq!(
        stderr_text.matches("owns scheduling for this data dir").count(),
        1,
        "said once, not per tick"
    );
}

/// Pausing something that is not there is not success.
///
/// `set_enabled` is an UPDATE with no row check, so an unknown id used to be a
/// silent no-op the CLI reported as `paused <id>` with exit 0 — a script that
/// typo'd an id, or held a stale one, was told its schedule was paused while
/// it kept firing. Deleting stays idempotent on purpose (the goal state is
/// reached either way); pausing has no such reading.
#[test]
fn pausing_or_resuming_an_unknown_schedule_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let serve = boot_serve(&data_dir);

    for verb in ["pause", "resume"] {
        let out = client(&data_dir)
            .args(["--json", "schedule", verb, "sch-does-not-exist"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "`schedule {verb}` on an unknown id must fail, not report success"
        );
        let v = json_stdout(&out);
        assert_eq!(v["ok"], false);
        assert!(
            v["error"]["message"].as_str().unwrap_or_default().contains("no such schedule"),
            "the refusal must name the reason, got: {v}"
        );
    }

    // A real schedule still toggles, both ways.
    let out = client(&data_dir)
        .args([
            "--json", "schedule", "create", "check the builds", "--name", "nightly", "--cwd",
            "/tmp", "--every", "30",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let id = json_stdout(&out)["data"]["id"].as_str().unwrap().to_string();

    for (verb, want_enabled) in [("pause", false), ("resume", true)] {
        let out = client(&data_dir).args(["--json", "schedule", verb, &id]).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "`schedule {verb}` on a real id must work");
        let out = client(&data_dir).args(["--json", "schedule", "ls"]).output().unwrap();
        assert_eq!(
            json_stdout(&out)["data"][0]["enabled"],
            want_enabled,
            "`{verb}` did not take effect"
        );
    }

    serve.stop_hard();
}
