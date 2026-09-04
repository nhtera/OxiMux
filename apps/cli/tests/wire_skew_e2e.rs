//! Cross-version wire skew: the current tree against a **released** binary,
//! in both directions, over one scripted session.
//!
//! The protocol's append-only discipline is enforced by review and by unit
//! pins, but nothing else in the repo ever runs two different builds against
//! each other — and "a v19 peer keeps working" is a claim about a binary that
//! no longer exists in this tree. These tests run it for real:
//!
//! - **old client → new host**: every day-one verb still works, and the live
//!   stream stays decodable even across events the old build has no decoder
//!   for (the host downgrades them — see `HostEvent::new_for_peer`).
//! - **new client → old host**: the current CLI drives a released `serve`
//!   through a full turn.
//!
//! Gated on `OXIMUX_SKEW_CLI` — the path to a released `oximux` binary — so
//! `cargo test` stays hermetic by default. CI downloads the latest release
//! and sets it; locally:
//!
//! ```sh
//! OXIMUX_SKEW_CLI=~/.oximux-old/oximux cargo test --test wire_skew_e2e
//! ```
#![cfg(unix)]

mod common;

use std::io::BufRead as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// The released binary under test, or `None` (→ the suite no-ops).
fn old_cli() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("OXIMUX_SKEW_CLI")?);
    assert!(path.is_file(), "OXIMUX_SKEW_CLI is set but not a file: {}", path.display());
    Some(path)
}

/// A command on the old binary with the same env scrubbing `common::bin` gives
/// the current one — nothing may leak in from the runner's own session.
fn old_bin(old: &PathBuf) -> Command {
    let mut cmd = Command::new(old);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd.env("OXIMUX_RELAY_BINARY", "/nonexistent/oximux-relay-for-tests");
    cmd
}

/// The protocol version a binary was built with, from `version`'s
/// `(protocol vNN)` suffix. Drives which shape of the edited-approval event
/// the old client is entitled to see.
fn protocol_of(old: &PathBuf) -> u32 {
    let out = old_bin(old).arg("version").output().expect("old binary runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let vpos = text.find("(protocol v").expect("version prints its protocol") + "(protocol v".len();
    text[vpos..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("protocol version is a number")
}

/// Read NDJSON lines from a streaming child until `pred` matches one, a
/// deadline passes, or the stream ends. Returns the matching line and kills
/// the child either way — `attach` streams until Ctrl+C by design.
fn stream_until(
    mut child: std::process::Child,
    deadline: Duration,
    pred: impl Fn(&Value) -> bool,
) -> Result<Value, Vec<String>> {
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let until = std::time::Instant::now() + deadline;
    let result = loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break Err(seen.clone());
        }
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && pred(&v)
                {
                    break Ok(v);
                }
                seen.push(line);
            }
            Err(_) => break Err(seen.clone()),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn json_stdout(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON.\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Boot the RELEASED `serve` and return its child plus its data directory.
///
/// A local sibling of `common::boot_serve`, which is hard-wired to the current
/// binary. Shared by every direction-2 test so the released host is booted one
/// way — a second copy would drift the moment the readiness contract moves.
fn boot_released_serve(
    old: &PathBuf,
    data: &std::path::Path,
    shim: &std::path::Path,
    tmp: &std::path::Path,
) -> (std::process::Child, String) {
    let mut child = old_bin(old)
        .args(["serve", "--data-dir", data.to_str().unwrap()])
        .env("PATH", common::path_with(shim))
        .env("OXIMUX_FAKE_AGENT_REPORT", tmp.join("report"))
        .env("OXIMUX_FAKE_AGENT_SESSION", "skew-old-host")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("released serve boots");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });
    let ready = rx.recv_timeout(Duration::from_secs(60)).expect("released serve readiness line");
    let ready: Value = serde_json::from_str(ready.trim()).expect("readiness is JSON");
    let dir = ready["dataDir"].as_str().expect("dataDir").to_string();
    (child, dir)
}

/// Direction 1: a RELEASED client drives the current tree's host through
/// spawn → wait → approval, and its live stream survives an event minted
/// after it shipped. A client below v20 must see the Notice downgrade — never
/// its own "event this CLI cannot render" marker (`"event":null`), which
/// would mean the host pushed vocabulary the peer never declared.
#[test]
fn an_old_client_survives_a_new_hosts_full_stream() {
    let Some(old) = old_cli() else {
        eprintln!("skipped: OXIMUX_SKEW_CLI is unset");
        return;
    };
    let old_protocol = protocol_of(&old);

    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let shim = tmp.path().join("bin");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    common::install_claude_shim(&shim);
    let report = tmp.path().join("report");
    let serve = common::boot_serve(
        &data,
        &shim,
        &report,
        "skew-session",
        common::AgentBehaviour::stalling(),
    );
    let dir = serve.runtime_dir();
    let dir_s = dir.to_str().unwrap();

    // The released client's day-one verbs against the new host.
    let out = old_bin(&old).args(["--dir", dir_s, "--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "old status: {}", String::from_utf8_lossy(&out.stderr));

    let out = old_bin(&old)
        .args(["--dir", dir_s, "--json", "run", "--bg", "--cwd", cwd.to_str().unwrap(), "edit it"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "old run: {}", String::from_utf8_lossy(&out.stderr));
    let session = json_stdout(&out)["data"]["session_id"].as_str().unwrap().to_string();

    let out = old_bin(&old)
        .args(["--dir", dir_s, "--timeout", "30", "--json", "wait", &session, "--until", "needs-approval"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "old wait: {}", String::from_utf8_lossy(&out.stderr));

    // Approve with an EDIT (current CLI): the new host records it as
    // `PermissionEdited` — the event the old client has no decoder for.
    let out = common::bin()
        .args([
            "--dir", dir_s, "--json", "permit", "allow", &session,
            "--input", r#"{"file_path":"notes.txt","old_string":"a","new_string":"EDITED","replace_all":false}"#,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "allow: {}", String::from_utf8_lossy(&out.stderr));

    // The old client replays the stream from the start. What it is entitled
    // to depends on what it declared: below v20 the host must hand it the
    // same seq as a Notice; at or above, the structured event itself.
    let child = old_bin(&old)
        .args(["--dir", dir_s, "--json", "attach", &session, "--from", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let expect_structured = old_protocol >= 20;
    let found = stream_until(child, Duration::from_secs(30), |v| {
        let event = &v["event"];
        if expect_structured {
            event.get("PermissionEdited").is_some()
        } else {
            event
                .get("Notice")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("approval edited"))
        }
    });
    let found = found.unwrap_or_else(|seen| {
        panic!(
            "the old client (protocol v{old_protocol}) never saw the edited approval; \
             lines seen:\n{}",
            seen.join("\n")
        )
    });
    assert!(found["event"] != Value::Null, "the old client decoded it, not marked it unknown");

    // And the current client sees the structured event on the same stream.
    let child = common::bin()
        .args(["--dir", dir_s, "--json", "attach", &session, "--from", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    stream_until(child, Duration::from_secs(30), |v| {
        v["event"].get("PermissionEdited").is_some()
    })
    .unwrap_or_else(|seen| {
        panic!("the current client never saw PermissionEdited; lines seen:\n{}", seen.join("\n"))
    });

    serve.kill_hard();
}

/// Direction 2: the current CLI drives a RELEASED `serve` through a complete
/// turn — spawn, stream to completion, list, transcript. The released host
/// knows nothing of this tree; every verb here must degrade to exactly what
/// that host shipped with.
#[test]
fn a_new_client_drives_an_old_host_through_a_full_turn() {
    let Some(old) = old_cli() else {
        eprintln!("skipped: OXIMUX_SKEW_CLI is unset");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let shim = tmp.path().join("bin");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    common::install_claude_shim(&shim);

    let (mut child, dir) = boot_released_serve(&old, &data, &shim, tmp.path());

    // A full turn, streamed to completion by the current client.
    let out = common::bin()
        .args(["--dir", &dir, "--json", "run", "--cwd", cwd.to_str().unwrap(), "say hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "run: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let result = lines.last().expect("a final result object");
    assert_eq!(result["ok"], true, "the turn completed: {result}");
    let session = result["data"]["session_id"].as_str().expect("session id").to_string();

    let out = common::bin().args(["--dir", &dir, "--json", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let rows = json_stdout(&out);
    assert!(
        rows["data"].as_array().is_some_and(|r| r.iter().any(|s| s["session_id"] == *session)),
        "the released host lists the session: {rows}"
    );

    let out = common::bin().args(["--dir", &dir, "--json", "transcript", &session]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "transcript from the released host: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Direction 2, the team surface: a v22 client must not send v22 ordinals to a
/// host that predates them.
///
/// This is the case the rest of this suite does not reach. `TeamRunCreateV2`
/// and `TeamStatusV2` are appended ordinals — a host that does not know them
/// cannot decode the frame and answers `BadRequest("undecodable request
/// frame")`, so an updated CLI would report a malformed request against a host
/// that was working a moment earlier, with nothing pointing at the version.
///
/// So: a plain `team run` must fall back to the v18 verb and work, `team
/// status` must read the board back, and only a run that actually names a
/// per-role agent may be refused — with a sentence about versions, not a dead
/// socket.
#[test]
fn a_new_client_runs_a_team_on_an_old_host() {
    let Some(old) = old_cli() else {
        eprintln!("skipped: OXIMUX_SKEW_CLI is unset");
        return;
    };
    // Only meaningful against a host below the per-role floor. A release at or
    // above it serves the v2 verbs and there is nothing to fall back from.
    let host_version = protocol_of(&old);
    if host_version >= oximux_remote_proto::proto::TEAM_PER_ROLE_MIN_VERSION {
        eprintln!("skipped: the released peer already speaks v{host_version}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let shim = tmp.path().join("bin");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    common::install_claude_shim(&shim);
    let (mut child, dir) = boot_released_serve(&old, &data, &shim, tmp.path());

    // A run naming no per-role agent IS the v18 request, so it must work.
    let out = common::bin()
        .args([
            "--dir", &dir, "--json", "team", "run", "--name", "skew", "--role", "impl=go",
            "--cwd", cwd.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a plain team run must degrade to the v18 verb: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run_id = json_stdout(&out)["data"]["id"].as_str().expect("a run id").to_string();

    // And the board reads back, without an agent column the old host has no
    // field for.
    let out = common::bin()
        .args(["--dir", &dir, "--json", "team", "status", "--run", &run_id])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "team status must degrade too: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let board = json_stdout(&out);
    assert_eq!(board["data"]["id"], run_id.as_str());

    // Only the thing the old host genuinely cannot do is refused — and it is
    // refused with a sentence, not a dropped connection.
    let out = common::bin()
        .args([
            "--dir", &dir, "team", "run", "--name", "skew2", "--role", "impl=go",
            "--role-agent", "impl=claude", "--cwd", cwd.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "unreachable-class, not a crash: {stderr}");
    assert!(stderr.contains("protocol v22"), "it names the version needed: {stderr}");
    // The symptom this gate exists to prevent, measured against this very
    // binary: without the client-side refusal the host cannot decode the
    // ordinal and answers `BadRequest("undecodable request frame")`, which
    // reaches the user as a complaint about the frame rather than the version.
    assert!(
        !stderr.contains("undecodable"),
        "and never blames the frame for what is a version problem: {stderr}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The v23 twin of [`a_new_client_runs_a_team_on_an_old_host`]: every preset
/// cadence keeps working against a pre-cron host, and only `--cron` is refused
/// — with a sentence naming the version, never a complaint about the frame.
#[test]
fn a_new_client_schedules_on_an_old_host() {
    let Some(old) = old_cli() else {
        eprintln!("skipped: OXIMUX_SKEW_CLI is unset");
        return;
    };
    let host_version = protocol_of(&old);
    if host_version >= oximux_remote_proto::proto::SCHEDULE_CRON_MIN_VERSION {
        eprintln!("skipped: the released peer already speaks v{host_version}");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let shim = tmp.path().join("bin");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    common::install_claude_shim(&shim);
    let (mut child, dir) = boot_released_serve(&old, &data, &shim, tmp.path());

    // A preset cadence IS the v10 request, so it must still work untouched —
    // this is what `create_request` sending V2 only for cron buys.
    let out = common::bin()
        .args([
            "--dir", &dir, "--json", "schedule", "create", "run the report", "--name", "skew",
            "--daily", "09:00", "--cwd", cwd.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a preset cadence must keep speaking v10: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let created = json_stdout(&out);
    assert!(created["data"]["id"].as_str().is_some(), "a schedule id comes back");
    assert!(
        created["data"].get("cron").is_some(),
        "the `cron` key is present even on the v10 path, so a script reads it the same way"
    );
    assert!(created["data"]["cron"].is_null(), "and it is null, because v10 cannot say");

    // The listing degrades too, rather than demanding a verb the host lacks.
    let out = common::bin().args(["--dir", &dir, "--json", "schedule", "ls"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "schedule ls must degrade: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Only cron itself is refused.
    let out = common::bin()
        .args([
            "--dir", &dir, "schedule", "create", "run the report", "--name", "skew2",
            "--cron", "0 9 * * 1-5", "--cwd", cwd.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "unreachable-class, not a crash: {stderr}");
    assert!(stderr.contains("protocol v23"), "it names the version needed: {stderr}");
    assert!(
        !stderr.contains("undecodable"),
        "and never blames the frame for what is a version problem: {stderr}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

