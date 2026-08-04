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
    AgentBehaviour, await_report, bin, boot_serve, install_claude_shim, probe_output,
    report_value, run_prompt,
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
///    handle really was re-pointed at the id the agent announced;
/// 4. a session-less full-host verb is **denied** — the confinement is a wall,
///    not a filter.
///
/// Claim 3 is the one only a real spawn can make. Everything up to the rebind is
/// stubbable; the rebind itself is a fact about a process that already exists.
#[test]
fn a_spawned_agent_is_confined_to_the_session_it_announced() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");
    let session = "fake-session-confined";

    install_claude_shim(&shim_dir);
    let serve =
        boot_serve(&data_dir, &shim_dir, &report, session, AgentBehaviour::probing());
    assert_eq!(serve.runtime_dir(), data_dir, "the readiness line names the data dir");

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    run_prompt(&data_dir, &cwd, "hello");

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
    // was re-pointed at the session the agent announced. Before that rebind the
    // same call is fail-closed, which is what makes this an ordering proof and
    // not merely a permissions one.
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
    assert_eq!(rows[0]["session_id"], session, "and that session is the one it announced");

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
