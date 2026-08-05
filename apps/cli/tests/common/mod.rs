//! Shared scaffolding for the suites that drive a **real** `oximux serve` with
//! a **real** spawned agent.
//!
//! `agent_spawn_e2e`, `recovery_e2e` and `security_e2e` all need the same three
//! things — a fake `claude` on PATH, a booted host, and a client pointed at it —
//! and they need them to behave identically, because they assert about the same
//! process from different angles. Three copies would drift.
//!
//! `allow(dead_code)` because Rust compiles this module separately into each
//! test binary, so anything one suite does not call is dead code *there* — and
//! with `-D warnings` that is a build failure rather than a nit. The alternative
//! (splitting helpers by consumer) would defeat the point of sharing them.
#![allow(dead_code)]

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The fake agent, resolved against this crate rather than the cwd — `cargo
/// test` does not promise which directory it runs from.
pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-agent.sh")
}

/// A POSIX `sh`, found even where PATH has none.
///
/// The resolution `crates/agents/src/thread/sh_fixture.rs` documents, kept local
/// because that module is `pub(crate)` to `oximux-agents` and widening a
/// production crate's API to share test scaffolding is the worse trade.
///
/// Windows-only: the unix shim carries a `#!/bin/sh` shebang and needs no
/// resolver.
#[cfg(windows)]
pub fn sh_program() -> PathBuf {
    if let Ok(p) = which::which("sh") {
        return p;
    }
    // <root>\cmd\git.exe → <root>\bin\sh.exe
    if let Ok(git) = which::which("git")
        && let Some(root) = git.parent().and_then(|p| p.parent())
    {
        let candidate = root.join("bin").join("sh.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "these suites need a POSIX sh and none was found: put `sh` on PATH \
         or install Git for Windows (its bin\\sh.exe is picked up via `git`)"
    );
}

/// Write a `claude` into `dir` that runs the fixture.
///
/// This is the injection point, and it is deliberately outside production code:
/// the launcher calls `program_for_spawn("claude")`, which is `which`, which
/// finds this. No test-only branch ships.
pub fn install_claude_shim(dir: &Path) {
    std::fs::create_dir_all(dir).expect("shim dir");
    let fixture = fixture();
    assert!(fixture.is_file(), "fixture missing: {}", fixture.display());

    #[cfg(not(windows))]
    {
        let shim = dir.join("claude");
        std::fs::write(&shim, format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", fixture.display()))
            .expect("write shim");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");
    }
    #[cfg(windows)]
    {
        // `which` on Windows honours PATHEXT, so the shim needs a real
        // extension; a bare `claude` would never be found.
        std::fs::write(
            dir.join("claude.cmd"),
            format!(
                "@echo off\r\n\"{}\" \"{}\" %*\r\n",
                sh_program().display(),
                fixture.display()
            ),
        )
        .expect("write shim");
    }
}

/// PATH with `dir` in front.
pub fn path_with(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries = vec![dir.to_path_buf()];
    entries.extend(std::env::split_paths(&existing));
    std::env::join_paths(entries).expect("join PATH")
}

/// The CLI binary with the runner's own credentials scrubbed, so nothing leaks
/// in from whatever environment `cargo test` happens to carry.
pub fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd.env("OXIMUX_RELAY_BINARY", "/nonexistent/oximux-relay-for-tests");
    cmd
}

/// How a suite wants its fake agent to behave.
#[derive(Default, Clone, Copy)]
pub struct AgentBehaviour {
    /// Run the confinement probe (needs the runtime dir).
    pub probe: bool,
    /// Take the prompt, ask a permission, and never answer — leaves a turn in
    /// flight for the recovery suite to interrupt.
    pub stall: bool,
}

impl AgentBehaviour {
    pub fn probing() -> Self {
        Self { probe: true, stall: false }
    }
    pub fn stalling() -> Self {
        Self { probe: false, stall: true }
    }
}

pub struct ServeUnderTest {
    child: Child,
    ready: Value,
    /// Everything the host wrote to stderr, drained continuously so a full pipe
    /// can never block it, and readable afterwards for the log-hygiene checks.
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

/// Boot serve with the fake agent first on PATH.
///
/// The agent inherits **this** environment — the launcher adds the credential on
/// top of the host's rather than replacing it — so the fixture's settings go
/// here rather than on the client invocation that triggers the spawn.
///
/// Every boot also carries inherited Claude Code session markers, because the
/// host is supposed to drop them and a child is the only place that is
/// observable. `security_e2e` asserts on it; the others simply must not break
/// under it, which is the more realistic condition anyway.
pub fn boot_serve(
    data_dir: &Path,
    shim_dir: &Path,
    report: &Path,
    session: &str,
    behaviour: AgentBehaviour,
) -> ServeUnderTest {
    let mut cmd = bin();
    cmd.args(["serve", "--data-dir", data_dir.to_str().unwrap()])
        .env("PATH", path_with(shim_dir))
        .env("OXIMUX_FAKE_AGENT_REPORT", report)
        .env("OXIMUX_FAKE_AGENT_SESSION", session)
        .env("OXIMUX_FAKE_AGENT_CLI", env!("CARGO_BIN_EXE_oximux-cli"))
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_CHILD_SESSION", "outer-session")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        // Every level, on purpose. serve defaults its filter to `info`
        // (`serve/mod.rs`), which excludes `tracing::debug!` — and a stray
        // `debug!` is the single most likely way a credential reaches a log,
        // precisely because debug output is what gets added while chasing a bug
        // and left behind afterwards. A log-hygiene check that reads only the
        // `info` stream cannot see the leak it exists to catch.
        //
        // Costs nothing here: stderr is drained into a buffer, never a terminal.
        .env("RUST_LOG", "trace");

    if behaviour.probe {
        cmd.env("OXIMUX_FAKE_AGENT_DIR", data_dir);
    } else {
        cmd.env_remove("OXIMUX_FAKE_AGENT_DIR");
    }
    if behaviour.stall {
        cmd.env("OXIMUX_FAKE_AGENT_STALL", "1");
    } else {
        cmd.env_remove("OXIMUX_FAKE_AGENT_STALL");
    }

    let mut child =
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn serve");

    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let sink = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(err);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        // Keep draining so the child never blocks on a full pipe.
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    let line = rx.recv_timeout(Duration::from_secs(60)).expect("serve prints its readiness line");
    let ready: Value = serde_json::from_str(line.trim()).expect("the readiness line is JSON");
    assert_eq!(ready["type"], "oximux_serve_ready");
    ServeUnderTest { child, ready, stderr: stderr_buf }
}

impl ServeUnderTest {
    /// The runtime directory the CLI wants for `--dir`.
    ///
    /// Read back from the readiness line rather than assumed, so this asserts
    /// the contract a scripted caller depends on. The field is named
    /// `localSocket` but carries the *directory* the socket lives in — which is
    /// what `--dir` takes, so piping one into the other works despite the name.
    pub fn runtime_dir(&self) -> PathBuf {
        PathBuf::from(self.ready["localSocket"].as_str().expect("localSocket"))
    }

    pub fn ready(&self) -> &Value {
        &self.ready
    }

    /// Everything the host has logged so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// SIGKILL — no drain, no flush, nothing runs.
    pub fn kill_hard(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The polite stop, and the one with a contract attached. `Child::kill` is
    /// SIGKILL, so it cannot stand in here — the whole point is the path that
    /// drains.
    #[cfg(unix)]
    pub fn stop_gracefully(mut self) -> String {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM)
            .expect("signal our own child");
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        self.stderr()
    }
}

/// The backstop, for every test that never reaches its explicit stop.
///
/// `kill_hard`/`stop_gracefully` are the last line of a test body, so an
/// assertion failure, a panic, or an interrupted `cargo test` skipped them and
/// orphaned a live `oximux serve` — one holding a socket and a SQLite handle
/// against a temp dir already deleted. CI hides it (fresh container per job);
/// developer machines collected them, seven alive on one, the oldest 11 hours.
///
/// Idempotent with the explicit stops: `Child::kill` is a no-op once the child
/// has been waited on, so this cannot signal a recycled pid.
impl Drop for ServeUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client invocation against `runtime_dir`, in JSON mode.
pub fn cli(runtime_dir: &Path) -> Command {
    let mut cmd = bin();
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "60", "--json"]);
    cmd
}

pub fn json_of(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Start a session against a booted host and return its id.
pub fn run_prompt(runtime_dir: &Path, cwd: &Path, prompt: &str) -> Value {
    let out = cli(runtime_dir)
        .args(["run", prompt, "--cwd", cwd.to_str().unwrap(), "--bg"])
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    json_of(&out)
}

/// Where the fixture writes its `ls` output — `<report>.ls`, spelled the same
/// way the script spells it.
pub fn probe_output(report: &Path) -> PathBuf {
    let mut name = report.as_os_str().to_os_string();
    name.push(".ls");
    PathBuf::from(name)
}

/// Findings the fixture appended, as `key=value` lines.
pub fn report_value(report: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(report).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

/// Wait for the fixture to have written `key`, so assertions do not race the
/// child's own writes.
pub fn await_report(report: &Path, key: &str, within: Duration) -> Option<String> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(v) = report_value(report, key) {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
