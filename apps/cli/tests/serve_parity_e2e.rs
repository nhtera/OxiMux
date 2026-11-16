//! What a headless host says is true, read back by a client.
//!
//! Phase 4b's claim is that `oximux serve` runs the same delivery and truth
//! machinery as the app: one `SendGate`, one outbox, one `SessionHealth`. The
//! unit suites prove each piece in isolation and the desktop proves it in a
//! window; what neither can prove is the headless path end to end — that a
//! real serve, spawning a real agent process, publishes a verdict a real
//! client reads back.
//!
//! That gap is not academic. Every one of these states is *invisible* on a
//! server: there is no rail, no chip, no badge. A session that is rate-limited
//! and a session whose process died look identical to `ls`, and both look
//! identical to a session that is simply thinking. The host already tells them
//! apart; this suite is the proof that the telling survives the whole way out
//! to a caller on the box.
//!
//! The agent is `fixtures/fake-agent.sh`, as in `agent_spawn_e2e` and
//! `recovery_e2e`: a stub connection cannot be killed, cannot exit, and cannot
//! hold a turn open across a host restart, which is most of what is under test
//! here.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{
    AgentBehaviour, ServeUnderTest, boot_serve, cli, install_claude_shim, json_of, run_prompt,
    session_of,
};

/// `oximux status <session> --json`, as a scripted caller reads it.
fn status(runtime_dir: &Path, session: &str) -> Value {
    let out = cli(runtime_dir).args(["status", session]).output().expect("status");
    assert_eq!(
        out.status.code(),
        Some(0),
        "status failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    json_of(&out)
}

/// Poll `status` until the health satisfies `ready`, or fail saying what it
/// last said.
///
/// Polled rather than slept on: every state here is produced by another
/// process — the agent announcing, the pump folding, the host publishing — and
/// the time each takes is a property of the machine, not of the contract.
fn await_status(runtime_dir: &Path, session: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = status(runtime_dir, session);
        if ready(&last["data"]) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{session} never reached {what}; last status: {last}");
}

/// A booted serve whose agent is holding a permission request, and the session
/// it is holding it for.
fn stalled_turn(root: &Path, session: &str) -> (std::path::PathBuf, String, ServeUnderTest) {
    let data_dir = root.join("data");
    let shim_dir = root.join("shim");
    let report = root.join("report.txt");
    install_claude_shim(&shim_dir);
    let serve = boot_serve(&data_dir, &shim_dir, &report, session, AgentBehaviour::stalling());

    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = session_of(&run_prompt(&data_dir, &cwd, "hold a tool call"));
    await_status(&data_dir, &session, "needsPermission", |d| {
        d["health"]["status"] == "needsPermission"
    });
    (data_dir, session, serve)
}

/// The persisted blob a host leaves behind for one session.
fn blob(data_dir: &Path, session: &str) -> Value {
    let db = oximux_storage::open(&data_dir.join("oximux.db")).expect("open db");
    let settings = oximux_storage::SettingsRepo::new(db);
    let raw = settings
        .get(&format!("agent_chat:{session}"))
        .expect("read")
        .unwrap_or_else(|| panic!("{session} left no blob"));
    serde_json::from_str(&raw).expect("the blob is JSON")
}

/// The headline truth row: a session waiting on a person says so, and says its
/// process is alive while it waits.
///
/// Both halves matter and they are different claims. "Waiting on a person" is
/// what a supervisor branches on; "alive" is what stops it concluding the
/// agent died, which silence alone cannot distinguish. And the host instance
/// id comes with them, so a client that reconnects can tell a restarted host
/// from the one it was talking to.
#[test]
fn a_headless_session_waiting_on_a_person_says_so_and_says_it_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let (data_dir, session, serve) = stalled_turn(dir.path(), "parity-waiting");

    let v = status(&data_dir, &session);
    assert_eq!(v["data"]["health_published"], true, "{v}");
    assert_eq!(v["data"]["health"]["status"], "needsPermission", "{v}");
    assert_eq!(
        v["data"]["health"]["liveness"], "live",
        "a process the host is talking to is alive, not merely quiet: {v}",
    );
    assert_eq!(
        v["data"]["health"]["exited_unexpectedly"], false,
        "nothing died, so nothing may say it did: {v}",
    );
    assert!(
        v["data"]["host_instance_id"].as_str().is_some_and(|id| !id.is_empty()),
        "every verdict names the host process that reached it: {v}",
    );

    serve.kill_hard();
}

/// A provider that answers and leaves takes its session back to dormant,
/// leaving its finished turn behind and nothing that answers for it.
///
/// The `exited` verdict itself is **not** readable here, and that is a
/// property of the host rather than of this test. The pump publishes it in one
/// frame and unregisters the session in the same breath (`pump.rs`: "this is
/// the only frame that says the process died"), so a subscriber that was
/// already attached sees it and a reader arriving afterwards finds no session
/// to ask. What such a reader can check is the consequence — and the
/// consequence is the thing that would be wrong if the exit were mishandled: a
/// registry entry left behind would answer for a dead process until the host
/// restarted.
#[test]
fn a_provider_that_answers_and_leaves_goes_back_to_dormant_with_its_turn_kept() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");
    install_claude_shim(&shim_dir);
    let serve =
        boot_serve(&data_dir, &shim_dir, &report, "parity-exited", AgentBehaviour::exiting());

    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = session_of(&run_prompt(&data_dir, &cwd, "answer and leave"));

    let v = await_status(&data_dir, &session, "no published verdict", |d| {
        d["health_published"] == false
    });
    assert_eq!(v["data"]["health"], Value::Null, "nothing is running it: {v}");

    // Still listed, and its turn is on disk: dormant means "not running", not
    // "gone".
    let out = cli(&data_dir).arg("ls").output().expect("ls");
    let rows = json_of(&out);
    assert!(
        rows["data"].as_array().expect("rows").iter().any(|r| r["session_id"] == session.as_str()),
        "a session whose provider left is still the host's to resume: {rows}",
    );
    let persisted = blob(&data_dir, &session);
    let entries = persisted["entries"].as_array().expect("entries");
    assert!(
        entries.iter().any(|e| e.get("Assistant").is_some()),
        "the turn it completed before leaving is kept: {persisted}",
    );

    serve.kill_hard();
}

/// A message sent while the agent is waiting on a person is held, survives the
/// host being killed, and comes back held — not sent.
///
/// The whole point of holding it is that nobody has reviewed it. A restart is
/// exactly the moment that is true, so a queue that drained itself on the way
/// back up would deliver, unseen, the one message most likely to be stale. The
/// unit suites prove the gate and the blob separately; this proves the round
/// trip through a real host that was killed rather than asked to stop.
#[test]
fn a_message_held_by_a_prompt_survives_a_kill_and_comes_back_held() {
    let dir = tempfile::tempdir().unwrap();
    let (data_dir, session, serve) = stalled_turn(dir.path(), "parity-held");
    let before = status(&data_dir, &session)["data"]["host_instance_id"].clone();

    // Accepted, not written: the agent is holding a permission request, so the
    // gate holds the line. `--no-wait` because the turn it would wait for
    // cannot start until the person answers.
    let out = cli(&data_dir)
        .args(["send", &session, "look at this later", "--no-wait"])
        .output()
        .expect("send");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a send behind a prompt is accepted and queued\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // Killed, not drained: the queue has to be on disk already, written by the
    // pump's own save rather than by a shutdown that got the chance to tidy up.
    await_status(&data_dir, &session, "a persisted queue", |_| {
        blob(&data_dir, &session)["queued"].as_array().is_some_and(|q| !q.is_empty())
    });
    serve.kill_hard();

    let persisted = blob(&data_dir, &session);
    let queued = persisted["queued"].as_array().expect("a queue").clone();
    assert_eq!(queued.len(), 1, "exactly the one message, once: {persisted}");
    assert_eq!(queued[0]["text"], "look at this later");
    assert!(
        queued[0]["send_key"].as_str().is_some(),
        "its key persisted with it, so a client asking what became of it can be answered: {}",
        queued[0],
    );
    let entries = persisted["entries"].as_array().expect("entries");
    let prompts = entries.iter().filter(|e| e.get("User").is_some()).count();
    assert_eq!(prompts, 1, "the held message was never written to the agent: {persisted}");

    // Back up, and the restored line waits for a person.
    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "parity-held",
        AgentBehaviour::stalling(),
    );
    // Opening the session is what starts its pump, and a message is the only
    // thing that opens one — so this second send is the act of reopening, not
    // part of what is being measured.
    let out = cli(&data_dir)
        .args(["send", &session, "and this one too", "--no-wait"])
        .output()
        .expect("send");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let v = await_status(&data_dir, &session, "a held message", |d| {
        d["health"]["held_restored"].as_u64().is_some_and(|n| n >= 1)
    });
    assert_eq!(
        v["data"]["health"]["held_restored"], 1,
        "the message from before the kill is held, and only it: {v}",
    );
    assert_ne!(
        v["data"]["host_instance_id"], before,
        "a new host process, so a client knows not to resume what it was tracking: {v}",
    );

    serve.kill_hard();
}

/// A session this host is not running answers promptly — and reading it does
/// not start it.
///
/// Both halves are the point. On a server most sessions are dormant most of
/// the time, so the verb that exists to diagnose one must not hang on the
/// common case; and a diagnostic that spawned the provider it was asked about
/// would answer a question nobody asked, at the cost of a real process. The
/// first draft of this verb did exactly that — `GetSessionInfo` is in serve's
/// build list — and the symptom was a session reading `restoredUnconfirmed`
/// seconds after its agent had exited, which was this verb's own echo rather
/// than the host's verdict.
#[test]
fn reading_a_session_that_is_not_running_answers_at_once_and_starts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (data_dir, session, serve) = stalled_turn(dir.path(), "parity-dormant");
    serve.kill_hard();

    let shim_dir = dir.path().join("shim");
    let report = dir.path().join("report.txt");
    let serve = boot_serve(
        &data_dir,
        &shim_dir,
        &report,
        "parity-dormant",
        AgentBehaviour::stalling(),
    );
    // How many agent processes have ever run, counted from the fixture's own
    // report: it appends a line per spawn, so a resurrection is visible.
    let spawns = |report: &Path| {
        std::fs::read_to_string(report)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "session_id=present")
            .count()
    };
    let before = spawns(&report);

    let started = Instant::now();
    let v = status(&data_dir, &session);
    assert_eq!(v["data"]["health_published"], false, "{v}");
    assert_eq!(v["data"]["health"], Value::Null, "{v}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it answered in {:?}, which means it waited on something",
        started.elapsed(),
    );
    assert_eq!(spawns(&report), before, "reading a session must not spawn its agent");

    serve.kill_hard();
}
