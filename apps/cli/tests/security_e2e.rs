//! The security pass, as tests rather than as a checklist.
//!
//! Each of these is a claim that was previously argued from a code reading. The
//! difference matters: a code reading is correct until someone edits the code,
//! and none of these failures are loud. A leaked credential in a log is
//! invisible until it is exfiltrated; an inherited session marker costs a
//! transcript with no error anywhere; a world-readable key file looks exactly
//! like a private one until another account on the box reads it.
//!
//! Deliberately driving a **real** host with a **real** spawned agent, because
//! three of the four claims are about what crosses a process boundary.

mod common;

use std::time::Duration;

use common::{
    AgentBehaviour, await_report, boot_serve, cli, install_claude_shim, run_prompt,
};

/// The environment scrub, observed from the only place it can be: inside a
/// spawned child.
///
/// `serve` drops inherited Claude Code session markers at boot so nothing it
/// spawns is mistaken for a nested child session. The consequence of a leak is
/// quiet and expensive — a `claude` that sees `CLAUDE_CODE_CHILD_SESSION` turns
/// transcript saving off, so the conversation never reaches disk and the session
/// cannot be imported or resumed. Nothing fails; the work is simply gone.
///
/// The scrub runs on the host's *own* process, which is why it cannot be checked
/// by inspecting the spawn call: the mechanism is inheritance, and the only
/// witness is the child. `boot_serve` stamps the markers on every host it starts
/// precisely so this can be asked.
#[test]
fn a_spawned_agent_never_inherits_the_hosts_session_markers() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "security-scrub",
        AgentBehaviour::default(),
    );

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    run_prompt(&data_dir, &cwd, "hello");

    assert_eq!(
        await_report(&report, "leaked_markers", Duration::from_secs(30)).as_deref(),
        Some("none"),
        "the spawned agent inherited session markers the host is supposed to drop — \
         a `claude` seeing these stops saving its transcript, silently",
    );

    serve.kill_hard();
}

/// **Log hygiene.** The host's bearer token must never appear in anything it
/// writes, on either stream.
///
/// This is the check a journal grep would do on a real deployment, run against
/// the streams a journal would capture. It is worth automating rather than
/// eyeballing because the leak mode is a single careless `tracing::debug!` in a
/// path nobody re-reads — and on a server, stderr goes to the journal, which is
/// readable by more accounts than the token file is, which is the entire point
/// of the token file being `0600`.
///
/// The token is read from disk as the operator, which is the one context allowed
/// to have it. The agent's per-session secrets are deliberately never given to
/// this test at all — the fixture reports their presence, never their value —
/// so this covers the credential a test can legitimately hold.
#[test]
fn the_hosts_own_token_never_appears_in_its_logs() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "security-logs",
        AgentBehaviour::probing(),
    );

    // Exercise the paths that handle the credential: an authenticated RPC, a
    // spawn that mints a per-session secret, and a failed auth.
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let _ = cli(&data_dir).arg("status").output();
    run_prompt(&data_dir, &cwd, "hello");
    let _ = cli(&data_dir).args(["transcript", "security-logs"]).output();
    // A caller presenting a credential it does not hold — the branch most
    // tempted to log what it rejected.
    let _ = cli(&data_dir)
        .arg("ls")
        .env(oximux_remote_local::SESSION_ENV_VAR, "security-logs")
        .env(oximux_remote_local::SESSION_TOKEN_ENV_VAR, "not-the-real-secret")
        .output();
    let _ = await_report(&report, "ls_exit", Duration::from_secs(30));

    let token = std::fs::read_to_string(oximux_remote_local::token_path(&data_dir))
        .expect("the host wrote a token file");
    let token = token.trim();
    assert!(!token.is_empty(), "the token file is empty — this test would be vacuous");

    let logs = serve.stderr();
    assert!(
        !logs.contains(token),
        "the host logged its own bearer token.\n\
         On a server these lines go to the journal, which more accounts can read \
         than can read the 0600 token file.\n--- logs ---\n{logs}",
    );

    serve.kill_hard();
}

/// A rejected credential is not echoed back either. The error has to say *what*
/// was wrong without repeating the material — otherwise a wrapper that logs
/// stderr on failure re-leaks whatever the caller was holding.
#[test]
fn a_refused_credential_is_never_echoed_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "security-echo",
        AgentBehaviour::default(),
    );

    const WRONG: &str = "deadbeefdeadbeefdeadbeefdeadbeef";
    let out = cli(&data_dir)
        .arg("ls")
        .env(oximux_remote_local::SESSION_ENV_VAR, "security-echo")
        .env(oximux_remote_local::SESSION_TOKEN_ENV_VAR, WRONG)
        .output()
        .expect("ls");

    assert_eq!(out.status.code(), Some(5), "a wrong credential is denied");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains(WRONG), "the rejected secret came back on stdout: {stdout}");
    assert!(!stderr.contains(WRONG), "the rejected secret came back on stderr: {stderr}");
    assert!(!serve.stderr().contains(WRONG), "the host logged the secret it rejected");

    serve.kill_hard();
}

/// The host's own runtime directory and token file are owner-only, asserted by
/// reading the permissions back rather than by trusting the call that set them.
///
/// A server is the deployment where other accounts on the box are most likely to
/// exist, so this matters more here than on a desktop. Readback rather than
/// "we called chmod" because the interesting failures are the ones where the
/// call succeeded and the result was not what was intended — a umask, an
/// inherited ACL, a filesystem that does not implement the mode.
#[test]
fn the_hosts_runtime_state_is_owner_only_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");

    install_claude_shim(&shim_dir);
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "security-perms",
        AgentBehaviour::default(),
    );

    let token = oximux_remote_local::token_path(&data_dir);
    assert!(token.is_file(), "the host wrote no token file");
    assert!(
        oximux_owner_only::is_restricted_to_owner(&token).expect("read back the token's mode"),
        "the bearer token is readable by more than its owner: {}",
        token.display(),
    );
    // A directory needs its own predicate: owner-only means `0o700` here and
    // `0o600` for a file, so asking the file question about a directory reports
    // a correctly-restricted `0o700` as a failure. (It did, on the first run of
    // this test — the assertion was wrong, not the host.)
    assert!(
        oximux_owner_only::is_dir_restricted_to_owner(&data_dir)
            .expect("read back the dir's mode"),
        "the host's data directory is reachable by more than its owner: {}",
        data_dir.display(),
    );

    serve.kill_hard();
}
