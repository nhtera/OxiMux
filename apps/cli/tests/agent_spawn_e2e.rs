//! The headless host spawning a **real agent process**, and what that process
//! is handed.
//!
//! Every other suite here stubs the launcher: `cli_e2e` registers a
//! `StubConnection` in-process, and `serve_e2e` says so out loud — "no agent CLI
//! is spawned anywhere here". That keeps them hermetic and fast, and it leaves
//! one claim unproven by construction, the claim the whole agent-CLI trust model
//! rests on:
//!
//! > every agent this host spawns is confined to its own session.
//!
//! A stub cannot show that. The credential is put into a child's *environment*,
//! so proving it needs a child with an environment — a real fork/exec, the real
//! `program_for_spawn("claude")` PATH resolution, the real stream-json decode,
//! and the real rebind from the opaque spawn handle onto the session id the
//! agent announces. This suite is that path, end to end.
//!
//! The agent is [`fixtures/fake-agent.sh`](fixtures/fake-agent.sh), reached by
//! putting a `claude` shim first on the host's PATH. Nothing in the production
//! path is aware of any of this: the launcher resolves `claude` exactly as it
//! would on a user's machine, and finds ours.
//!
//! One `sh` script rather than a `sh` + `.ps1` pair, for the reason
//! `crates/agents/src/thread/sh_fixture.rs` already established: Git for Windows
//! ships a full MSYS userland on every machine that can build this repo, and on
//! GitHub's `windows-latest` runners. Two scripts would mean two behaviours to
//! keep in step, of which the Windows one is the rarely-read half.

mod common;

use std::time::Duration;

use common::{
    AgentBehaviour, await_report, bin, boot_serve, cli, install_claude_shim, json_of,
    probe_output, report_value, run_prompt, session_of,
};

/// **The suite's reason to exist.**
///
/// A real spawned agent receives a real credential, and that credential reaches
/// its own session and no other. Four separate claims, each of which has to hold
/// for agent CLI access to be safe to leave on by default:
///
/// 1. the child is handed `OXIMUX_SESSION_ID` — it knows which credential it holds;
/// 2. the child is handed `OXIMUX_SESSION_TOKEN` — it can prove it;
/// 3. `ls` from inside sees exactly one session, its own — the opaque spawn
///    handle really was re-pointed at a registered session;
/// 4. a session-less full-host verb is **denied** — the confinement is a wall,
///    not a filter.
///
/// Claim 3 is the one only a real spawn can make. Everything up to the rebind is
/// stubbable; the rebind itself is a fact about a process that already exists.
/// The probe runs only once a prompt has arrived, which cannot happen before the
/// session is registered — so a successful `ls` from inside is an ordering proof
/// and not merely a permissions one. Before the rebind the same call is
/// fail-closed, the handle matching no session at all.
///
/// The expected id comes from the `run` reply rather than a constant: the host
/// names the session and the fixture adopts that name, exactly as the CLI it
/// stands in for does with `--session-id`. Asserting a constant instead meant
/// asserting that the fixture had won a race to announce, which is not a claim
/// this test is about and which it lost under load.
#[test]
fn a_spawned_agent_is_confined_to_the_session_it_announced() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "fake-session-confined",
        AgentBehaviour::probing(),
    );
    assert_eq!(serve.runtime_dir(), data_dir, "the readiness line names the data dir");

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = session_of(&run_prompt(&data_dir, &cwd, "hello"));

    // 1 + 2: the host granted a credential before the child existed.
    assert_eq!(
        await_report(&report, "session_id", Duration::from_secs(30)).as_deref(),
        Some("present"),
        "the spawned agent was not handed OXIMUX_SESSION_ID",
    );
    assert_eq!(
        report_value(&report, "session_token").as_deref(),
        Some("present"),
        "the spawned agent was not handed OXIMUX_SESSION_TOKEN",
    );

    // 3: the probe ran, and `ls` succeeded — so the opaque spawn handle really
    // was re-pointed at a registered session. Before that rebind the same call is
    // fail-closed, which is what makes this an ordering proof and not merely a
    // permissions one.
    assert_eq!(
        await_report(&report, "ls_exit", Duration::from_secs(30)).as_deref(),
        Some("0"),
        "the agent's own `ls` should succeed once its credential is bound",
    );
    let listed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(probe_output(&report)).expect("probe output"),
    )
    .expect("probe output is JSON");
    let rows = listed["data"].as_array().expect("data is an array");
    assert_eq!(rows.len(), 1, "a confined agent must see only its own session: {listed}");
    assert_eq!(
        rows[0]["session_id"], session,
        "and that session is the one this run created, not some other session's",
    );

    // 4: the wall. A session-less, full-host verb is refused outright.
    assert_eq!(
        report_value(&report, "projects_exit").as_deref(),
        Some("5"),
        "a confined agent must be denied the session-less surface (exit 5)",
    );

    serve.kill_hard();
}

/// The credential is granted *before* the child exists and only re-pointed at a
/// real session once the agent announces one. That ordering is deliberate and
/// fail-closed, and this pins the closed half: the fixture's own `--dir` is
/// never set here, so it makes no probe at all — and the run still succeeds,
/// because confinement is the host's job, not the agent's cooperation.
///
/// Put differently: an agent that declines to probe, or lies about what it
/// found, changes nothing about what it may reach. The wall is on the other
/// side of the socket.
#[test]
fn a_spawned_agent_that_never_probes_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "fake-session-quiet",
        AgentBehaviour::default(),
    );

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    run_prompt(&data_dir, &cwd, "hello");

    assert_eq!(
        await_report(&report, "session_id", Duration::from_secs(30)).as_deref(),
        Some("present"),
        "the credential is granted regardless of what the agent does with it",
    );
    assert!(
        report_value(&report, "ls_exit").is_none(),
        "no probe was configured, so none should have run",
    );

    serve.kill_hard();
}

/// An unknown agent id is refused rather than quietly defaulted.
///
/// Starting a *different* agent than the one asked for is worse than starting
/// none: a caller that asked for `codex` and silently got `claude` gets a
/// session that looks right and behaves wrong.
#[test]
fn an_unknown_agent_id_is_refused_rather_than_defaulted() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "fake-session-unknown",
        AgentBehaviour::default(),
    );

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = bin()
        .args(["--dir", data_dir.to_str().unwrap(), "--timeout", "60", "--json"])
        .args(["run", "hello", "--cwd", cwd.to_str().unwrap(), "--bg"])
        .args(["--agent", "no-such-agent"])
        .output()
        .expect("run");

    assert_ne!(out.status.code(), Some(0), "an unknown agent must not start a session");
    assert!(
        report_value(&report, "session_id").is_none(),
        "nothing should have been spawned for an unknown agent id",
    );

    serve.kill_hard();
}

/// A long, silent wait says it is still alive — on stderr, never stdout.
///
/// Both halves are the test. A blocking verb that prints nothing for minutes is
/// indistinguishable from a wedged one, and supervisors give up on quiet
/// children; that is why the keepalive exists. But stdout under `--json` is a
/// contract — event lines, then exactly one result object — and a liveness line
/// on it would break every consumer that parses the last line, or the whole
/// stream. So this asserts the signal is present *and* that it landed on the
/// other stream.
///
/// Needs a real wedged backend for the same reason the turn-timeout pin does: a
/// stub's stream is never silent for long enough to trip anything.
#[test]
fn a_silent_wait_reports_liveness_on_stderr_and_never_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve =
        boot_serve(&data_dir, &shim_dir, &report, "fake-session-quiet", AgentBehaviour::stalling());

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = session_of(&run_prompt(&data_dir, &cwd, "hello"));

    // Built without the `cli()` helper, which pins `--timeout 60`: clap refuses
    // the flag twice rather than quietly picking one, so the budget has to be
    // set once, here. Longer than one keepalive interval, short enough not to
    // dominate the run.
    let out = bin()
        .args(["--dir", data_dir.to_str().unwrap(), "--json"])
        .args(["--timeout", "20", "wait", &session, "--until", "idle"])
        .output()
        .expect("wait");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(4), "the wait should have timed out\nstderr: {stderr}");
    assert!(
        stderr.contains("keepalive"),
        "a 20s silent wait must report liveness at least once: {stderr}",
    );
    assert!(
        !stdout.contains("keepalive"),
        "liveness must never reach stdout — it is the machine-readable stream: {stdout}",
    );
    // And stdout still parses as exactly what the contract promises.
    let last = stdout.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("stdout's last line must be the result object ({e}): {stdout}"));
    assert_eq!(parsed["ok"], false, "a timed-out wait reports failure: {parsed}");

    serve.kill_hard();
}

/// `--turn-timeout` bounds a turn that never ends.
///
/// This suite is the only place the claim can be made. A turn ends when a real
/// backend says so, and every other suite stubs the launcher — so "the stream
/// stops on its own" is true there for reasons that have nothing to do with a
/// deadline. The stalling fixture takes the prompt, asks a permission and
/// answers nothing, which is exactly the shape that used to hang forever:
/// nobody is there to decide, so the turn cannot end by itself.
///
/// Regression pin. Before this, `run` and `send` passed `None` as the stream
/// deadline while the *global* `--timeout` sat in their help text looking like a
/// bound — so an unattended run had none at all. Note the global `--timeout 60`
/// that `cli()` sets is still in play here and is deliberately far larger than
/// the `--turn-timeout`: the assertion below is that the turn budget is what
/// fires, and the two are not the same clock.
#[test]
fn a_turn_that_never_ends_is_bounded_by_turn_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "fake-session-wedged",
        AgentBehaviour::stalling(),
    );

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    // Waited on with an explicit wall-clock bound rather than `output()`. The
    // regression this pins is an *unbounded* wait, so a broken deadline makes
    // the child never exit — and `output()` would hang the whole suite instead
    // of reporting, which is the one outcome a regression pin must not have.
    // The bound is generous on purpose: it asserts the deadline fired at all,
    // not its precision. A loaded CI runner takes seconds to get anywhere.
    let mut child = cli(&data_dir)
        .args(["run", "hello", "--cwd", cwd.to_str().unwrap(), "--turn-timeout", "3"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run");
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let status = loop {
        match child.try_wait().expect("wait on run") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        serve.kill_hard();
        panic!("`run --turn-timeout 3` never returned — the turn deadline is not being applied");
    };

    assert_eq!(
        status.code(),
        Some(4),
        "a bounded turn that never ends must exit 4 (timeout)",
    );

    // The agent is left alone — the CLI stopped waiting, it did not stop the
    // work. Anything else would make `--turn-timeout` unsafe to reach for.
    let listed = cli(&data_dir).args(["ls"]).output().expect("ls");
    let rows = json_of(&listed);
    assert_eq!(
        rows["data"].as_array().map(Vec::len),
        Some(1),
        "the session must survive its own turn timing out: {rows}",
    );

    serve.kill_hard();
}
