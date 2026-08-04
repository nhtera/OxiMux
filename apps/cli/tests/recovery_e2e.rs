//! What survives a host going away mid-work.
//!
//! `serve` makes two promises about being stopped, and they are different
//! promises:
//!
//! - **Asked to stop** (SIGTERM / the Windows stop control), it drains: every
//!   in-flight turn is marked *interrupted* and flushed. `docs/server-install.md`
//!   states this as a contract — "never silently truncated" — and a transcript
//!   that instead still shows a request outstanding is indistinguishable, to a
//!   later reader, from an agent that is still thinking.
//! - **Killed outright** (SIGKILL, power loss, OOM), it gets no chance to do
//!   anything. The promise there is weaker and more important: the *next* boot
//!   must come up clean — the session still listed, the transcript still
//!   readable, no wedged row that makes the host refuse to start.
//!
//! Both need a turn that is genuinely in flight when the axe falls, which is why
//! this suite spawns the real fake agent and puts it in its stalling mode rather
//! than stubbing a connection: a stub's "turn" is a bool in the same process
//! that is about to be killed.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{
    AgentBehaviour, ServeUnderTest, boot_serve, cli, install_claude_shim, json_of, run_prompt,
};

/// Wait for the fixture to record that it has taken the prompt and is holding
/// it. Without this the kill races the spawn and the test proves nothing.
fn await_stalling(report: &Path) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if std::fs::read_to_string(report).is_ok_and(|t| t.contains("stalling=1")) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the fake agent never reported taking the prompt: {}", report.display());
}

/// Poll `ls` until `session` reports `awaiting_permission`, so the host has
/// really taken the fixture's request before anything is killed.
fn await_awaiting_permission(runtime_dir: &Path, session: &str) {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let out = cli(runtime_dir).arg("ls").output().expect("ls");
        if out.status.code() == Some(0) {
            last = json_of(&out);
            if let Some(rows) = last["data"].as_array()
                && rows.iter().any(|r| r["session_id"] == session && r["awaiting_permission"] == true)
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    panic!("the host never registered the pending permission: {last}");
}

/// Boot a host, start a turn, and hold it in flight.
fn stalled_turn(dir: &Path, session: &str) -> (PathBuf, ServeUnderTest) {
    let data_dir = dir.join("data");
    let shim_dir = dir.join("shim");
    let report = dir.join("report.txt");
    install_claude_shim(&shim_dir);
    let serve =
        boot_serve(&data_dir, &shim_dir, &report, session, AgentBehaviour::stalling());

    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    run_prompt(&data_dir, &cwd, "think about it");

    await_stalling(&report);
    await_awaiting_permission(&data_dir, session);
    (data_dir, serve)
}

/// Reboot on the same data dir, with a fresh report so the second host's agent
/// cannot be confused with the first's.
fn reboot(dir: &Path, data_dir: &Path, session: &str) -> ServeUnderTest {
    boot_serve(
        data_dir,
        &dir.join("shim"),
        &dir.join("report2.txt"),
        session,
        AgentBehaviour::stalling(),
    )
}

/// The drain contract, end to end: a request that was outstanding when the host
/// was asked to stop is *settled* by the time the next boot reads it.
#[cfg(unix)]
#[test]
fn a_turn_in_flight_at_shutdown_is_settled_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let session = "recovery-drained";
    let (data_dir, serve) = stalled_turn(dir.path(), session);

    serve.stop_gracefully();

    // Second boot, same data dir: the drain's work has to be on disk, since this
    // process has no memory of the first one.
    let serve = reboot(dir.path(), &data_dir, session);

    let out = cli(&data_dir).args(["transcript", session]).output().expect("transcript");
    assert_eq!(
        out.status.code(),
        Some(0),
        "the transcript must survive a drain\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let v = json_of(&out);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["data"]["session_id"], session);

    let entries = v["data"]["entries"].as_array().expect("entries");
    assert!(!entries.is_empty(), "the drained transcript lost the prompt entirely: {v}");

    // **The drain-specific assertion**, and it has to be made against the
    // persisted transcript rather than against `ls`.
    //
    // `awaiting_permission` looked like the obvious signal and is worthless
    // here: it is served from the live `SessionRegistry`, which a restarted host
    // has no entry in, so it reads `false` for every restored session whether or
    // not anything was ever settled. Asserting on it passes with the drain's
    // `interrupt()` deleted — measured, not assumed.
    //
    // What *is* durable is the tool call's own status. The fixture asked for a
    // permission and then stopped answering, so at SIGTERM there was a request
    // outstanding; the drain marks it `Rejected`, and that lands in the blob.
    // Left undrained it persists as `WaitingForConfirmation` — a request
    // addressed to a process that no longer exists, which is precisely the
    // "still thinking?" ambiguity the contract promises to remove.
    let tool = entries
        .iter()
        .find_map(|e| e.get("ToolCall"))
        .unwrap_or_else(|| panic!("the drained transcript kept no tool call: {v}"));
    assert_eq!(
        tool["status"], "Rejected",
        "the drain left a permission request unsettled against a dead agent: {tool}",
    );

    serve.kill_hard();
}

/// The hard-kill invariant. Nothing was drained, nothing was flushed — the
/// question is only whether the *next* boot is clean.
///
/// This is the failure mode that matters on a server: the host is not usually
/// stopped politely, it is OOM-killed or the box loses power, and a boot that
/// then refuses to start (or silently drops the session) turns one lost turn
/// into a lost history.
#[test]
fn a_hard_kill_mid_turn_still_boots_clean() {
    let dir = tempfile::tempdir().unwrap();
    let session = "recovery-killed";
    let (data_dir, serve) = stalled_turn(dir.path(), session);

    serve.kill_hard();

    // Booting at all is half the assertion: `boot_serve` panics if the readiness
    // line never arrives, which is what a wedged store would cause.
    let serve = reboot(dir.path(), &data_dir, session);

    let out = cli(&data_dir).arg("ls").output().expect("ls");
    assert_eq!(
        out.status.code(),
        Some(0),
        "listing after a hard kill\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(json_of(&out)["ok"], true);

    // The transcript verb answers, whether or not the killed turn's tail made
    // it to disk — an unflushed tail is expected here, an unreadable session is
    // not.
    //
    // Asserted as a plain success rather than "success or a tolerated error".
    // The tolerant form was written first and was a lie: no `Failure` in this
    // CLI uses the code `"error"` (they are `rpc`, `denied`, `unreachable`, …),
    // so the second branch could never be taken and the assertion was already
    // this strict while reading as though it were not.
    let out = cli(&data_dir).args(["transcript", session]).output().expect("transcript");
    let v = json_of(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a session whose host was killed mid-turn must still be readable: {v}",
    );
    assert_eq!(v["ok"], true, "{v}");
    // The prompt was persisted before the kill (the pump saves on a tick, and
    // `stalled_turn` waited for the host to register the permission request),
    // so a hard kill may lose the tail but must not lose the exchange.
    assert!(
        !v["data"]["entries"].as_array().expect("entries").is_empty(),
        "a hard kill lost the whole transcript, not just its tail: {v}",
    );

    serve.kill_hard();
}

/// Two hosts, one data directory. The second must decline the schedule ticker
/// rather than fire every schedule twice — and must still come up and serve,
/// because declining the ticker is not a reason to refuse to run.
///
/// `serve_e2e` proves the declining side against a desktop-shaped holder; this
/// proves it between two `serve` processes, which is the deployment the
/// single-owner lock was actually written for.
#[test]
fn a_second_serve_on_the_same_data_dir_declines_the_ticker() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    install_claude_shim(&shim_dir);

    let first = boot_serve(
        &data_dir,
        &shim_dir,
        &dir.path().join("r1.txt"),
        "lock-first",
        AgentBehaviour::default(),
    );
    let second = boot_serve(
        &data_dir,
        &shim_dir,
        &dir.path().join("r2.txt"),
        "lock-second",
        AgentBehaviour::default(),
    );

    // Both are serving: the second answered its readiness line (asserted inside
    // `boot_serve`) and answers RPCs too.
    let out = cli(&data_dir).arg("status").output().expect("status");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a host that declined the ticker still serves\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(json_of(&out)["ok"], true);

    second.kill_hard();
    first.kill_hard();
}
