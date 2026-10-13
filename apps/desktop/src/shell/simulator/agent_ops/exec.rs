//! `sim exec`: a raw `simctl` or `adb` command on the attached device, for
//! the long tail the dedicated verbs do not cover.
//!
//! Three gates, in order: Settings' switch (off unless the user turned it
//! on; [`precheck`] refuses without asking anyone), the device's consent,
//! then a question every time showing the exact command. Before any of them
//! is asked, [`check`] has refused what the trimmed allowlist leaves out:
//! subcommands that reach past the device (`spawn`, `io`, `keychain`, adb's
//! `root`…), any other device (`all`, `booted`, an id, adb's global
//! options, anywhere), and host paths outside the worktree.
//!
//! It runs unsandboxed in the app's process: an approved `adb shell` is
//! arbitrary code on the device, and an approved `install` reads a worktree
//! file the agent itself may not reach. Hence the switch, off by default,
//! and the question with the whole command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AsyncApp, Entity};
use oximux_agent_core::scrub_text::scrub_text;
use oximux_remote_proto::simulator::{SimErrorWire, SimExecOutputWire, SimExecWire, SimReplyWire, words};
use oximux_simulator::runner::{self, redact_argv};
use oximux_simulator::{DeviceId, Source};

use super::Target;
use super::device_state::confirm;
use super::verbs::{Out, android_device, on_own_thread, simctl_ready};
use crate::shell::simulator::hub::{AskKind, SimulatorHub};

/// What a refused call says when the switch is off.
pub(super) const OFF: &str = "exec is off; enable it in Settings → Mobile Emulator";
const IPHONE: &str = "exec runs simctl or adb; a real iPhone takes neither";
/// Output kept, stdout and stderr together.
const CAP: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// The longest command the question shows whole (a longer one is refused,
/// so the user never approves what they could not read).
const SHOWN: usize = 2000;

/// `simctl` subcommands that take the device and touch only it.
const SIMCTL: [&str; 8] = ["listapps", "appinfo", "terminate", "openurl", "ui", "status_bar", "privacy", "location"];
/// `adb` commands an agent may run on the device.
const ADB: [&str; 7] = ["shell", "install", "uninstall", "logcat", "emu", "push", "pull"];
/// adb words that reach past the device, refused wherever they appear.
const ADB_REFUSED: [&str; 6] = ["connect", "disconnect", "kill-server", "reboot", "root", "remount"];
/// adb's global options (which device, which server), refused anywhere: the
/// host names the device.
const ADB_GLOBAL: [&str; 7] = ["-s", "-t", "-d", "-e", "-H", "-P", "-L"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tool {
    Simctl,
    Adb,
}

impl Tool {
    /// The tool for `udid`; a real iPhone has none.
    pub(super) fn of(udid: &DeviceId) -> Result<Self, SimErrorWire> {
        match udid.source() {
            Source::Simctl => Ok(Self::Simctl),
            Source::Adb => Ok(Self::Adb),
            Source::Devicectl => Err(SimErrorWire::Refused(IPHONE.into())),
        }
    }
}

/// The gates that need no question: the switch, a device, one exec takes,
/// and the arguments. Run before consent is asked, so a call refused here
/// asks nobody anything.
pub(super) async fn precheck(hub: &Entity<SimulatorHub>, target: &Target, wire: &SimExecWire, cx: &mut AsyncApp) -> Result<(), SimErrorWire> {
    let (allowed, udid) = hub.read_with(cx, |hub, _| (hub.exec_allowed(), hub.device_for(&target.worktree)));
    if !allowed {
        return Err(SimErrorWire::Refused(OFF.into()));
    }
    let tool = Tool::of(&udid.ok_or(SimErrorWire::NoDevice)?)?;
    // Paths are resolved on disk: off the UI thread.
    let (argv, worktree) = (wire.argv.clone(), target.worktree.clone());
    on_own_thread(move || check(tool, &argv, &worktree).map(drop)).await?
}

/// `argv` if `tool` may run it on the attached device, with every host path
/// made absolute (it was checked inside `worktree`). `BadInput` names what
/// is not allowed.
pub(super) fn check(tool: Tool, argv: &[String], worktree: &Path) -> Result<Vec<String>, SimErrorWire> {
    let bad = |why: String| Err(SimErrorWire::BadInput(why));
    let Some(sub) = argv.first() else {
        return bad("name the command after `--`, e.g. `oximux sim exec -- ui appearance dark`".into());
    };
    match tool {
        Tool::Simctl => {
            if !SIMCTL.contains(&sub.as_str()) {
                return bad(format!("`simctl {sub}` is not allowed; exec runs only: {}", SIMCTL.join(", ")));
            }
            for word in argv {
                let lower = word.to_ascii_lowercase();
                if lower == "all" || lower == "booted" || is_udid(word) {
                    return bad("exec runs on the attached device only: leave out device ids, `all` and `booted`".into());
                }
                if lower.starts_with("file:") || is_host_path(word) {
                    return bad(format!("`{word}`: simctl exec takes no host files"));
                }
            }
            Ok(argv.to_vec())
        }
        Tool::Adb => {
            for word in argv {
                if is_global_option(word) {
                    return bad(format!("`{word}`: adb's global options are refused (the device is chosen for you)"));
                }
                if ADB_REFUSED.contains(&word.as_str()) {
                    return bad(format!("`{word}` is not allowed through exec"));
                }
            }
            if !ADB.contains(&sub.as_str()) {
                return bad(format!("`adb {sub}` is not allowed; exec runs only: {}", ADB.join(", ")));
            }
            adb_paths(sub, argv, worktree)
        }
    }
}

/// `argv` with the host side of `install`, `push` and `pull` checked and
/// made absolute.
fn adb_paths(sub: &str, argv: &[String], worktree: &Path) -> Result<Vec<String>, SimErrorWire> {
    let mut out = argv.to_vec();
    let operands: Vec<usize> = (1..argv.len()).filter(|&i| !argv[i].starts_with('-')).collect();
    let host: Vec<(usize, bool)> = match sub {
        "install" => operands.iter().map(|&i| (i, true)).collect(),
        // `push LOCAL… REMOTE`.
        "push" if operands.len() >= 2 => operands[..operands.len() - 1].iter().map(|&i| (i, true)).collect(),
        "push" => return Err(SimErrorWire::BadInput("push takes a file in the worktree, then where it goes on the device".into())),
        // `pull REMOTE… LOCAL`: the local side is required (else adb writes
        // where the app runs, outside any worktree).
        "pull" if operands.len() >= 2 => vec![(operands[operands.len() - 1], false)],
        "pull" => return Err(SimErrorWire::BadInput("pull takes the device's file, then where it goes in the worktree".into())),
        _ => Vec::new(),
    };
    for (i, exists) in host {
        out[i] = inside(&argv[i], worktree, exists)?.to_string_lossy().into_owned();
    }
    Ok(out)
}

/// `path` (relative to `worktree`) resolved, symlinks and all, when it is
/// inside `worktree`. A path that need not exist yet (a pull's target) is
/// checked by its folder.
fn inside(path: &str, worktree: &Path, exists: bool) -> Result<PathBuf, SimErrorWire> {
    let outside = || SimErrorWire::BadInput(format!("`{path}` is not inside this worktree"));
    if path.starts_with('~') {
        return Err(outside());
    }
    let root = std::fs::canonicalize(worktree).map_err(|_| outside())?;
    let joined = worktree.join(path);
    let real = match std::fs::canonicalize(&joined) {
        Ok(real) => real,
        Err(_) if !exists => {
            let name = joined.file_name().filter(|n| *n != "..").ok_or_else(outside)?;
            let parent = joined.parent().ok_or_else(outside)?;
            std::fs::canonicalize(parent).map_err(|_| outside())?.join(name)
        }
        Err(_) => return Err(SimErrorWire::BadInput(format!("`{path}` was not found in this worktree"))),
    };
    if real.starts_with(&root) { Ok(real) } else { Err(outside()) }
}

/// An adb global option, alone or with its value attached (`-sSERIAL`).
fn is_global_option(word: &str) -> bool {
    ADB_GLOBAL.contains(&word)
        || ["-s", "-H", "-P", "-L"].iter().any(|o| word.starts_with(o))
        || word.strip_prefix("-t").is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        || word.starts_with("--one-device")
}

/// A simulator UDID: 8-4-4-4-12 hex digits.
fn is_udid(word: &str) -> bool {
    let parts: Vec<&str> = word.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Looks like a file on this Mac (simctl reads such words on the host).
fn is_host_path(word: &str) -> bool {
    word.starts_with('/') || word.starts_with('~') || word.starts_with("./") || word.starts_with("../") || word == ".."
}

/// The command as the user is asked about it: every word quoted as a flow
/// step quotes it, so a space or a line break shows.
fn shown(program: &str, args: &[String]) -> Result<String, SimErrorWire> {
    let mut line = program.to_owned();
    for arg in args {
        let word = words::quote(arg).map_err(SimErrorWire::BadInput)?;
        line.push(' ');
        line.push_str(&word);
    }
    if line.chars().count() > SHOWN {
        return Err(SimErrorWire::BadInput(format!("the command is longer than {SHOWN} characters, too long to show the user whole")));
    }
    Ok(line)
}

/// Carry out an exec whose consent is settled: the device's id goes in,
/// the user is asked with the exact command, then it runs, bounded.
pub(super) async fn run(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, wire: SimExecWire, target: &Target, cx: &mut AsyncApp) -> Out {
    // Again here: the switch may have moved while consent was asked.
    if !hub.read_with(cx, |hub, _| hub.exec_allowed()) {
        return Err(SimErrorWire::Refused(OFF.into()));
    }
    let tool = Tool::of(udid)?;
    let worktree = target.worktree.clone();
    let argv = on_own_thread(move || check(tool, &wire.argv, &worktree)).await??;
    let (program, args, line) = match tool {
        Tool::Simctl => {
            let sim = simctl_ready(hub, udid, cx).await?;
            let mut args = vec!["simctl".to_owned(), argv[0].clone(), sim.as_str().to_owned()];
            args.extend(argv[1..].iter().cloned());
            let line = shown("xcrun", &args)?;
            ("xcrun".to_owned(), args, line)
        }
        Tool::Adb => {
            let device = android_device(hub, udid, cx).await?;
            let mut args = vec!["-s".to_owned(), device.serial().to_owned()];
            args.extend(argv);
            let line = shown("adb", &args)?;
            (device.adb().to_string_lossy().into_owned(), args, line)
        }
    };
    confirm(hub, udid, name, AskKind::Exec { command: line }, target, cx).await?;
    let ran = on_own_thread(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        runner::run_bounded(&program, &refs, TIMEOUT, CAP).map_err(|e| (redact_argv(&program, &refs), e))
    })
    .await?;
    let done = ran.map_err(|(what, e)| SimErrorWire::Failed(format!("{what} did not run: {e}")))?;
    Ok(SimReplyWire::ExecOutput(output(done)))
}

/// The reply: scrubbed, with a line saying why a stopped command stopped.
fn output(done: runner::Bounded) -> SimExecOutputWire {
    let mut stderr = String::from_utf8_lossy(&done.stderr).into_owned();
    let note = if done.timed_out {
        Some(format!("[oximux: stopped after {} s]", TIMEOUT.as_secs()))
    } else if done.truncated {
        Some(format!("[oximux: output cut at {} KiB; the command was stopped]", CAP / 1024))
    } else {
        None
    };
    if let Some(note) = note {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&note);
    }
    SimExecOutputWire {
        status: done.status,
        stdout: scrub_text(&String::from_utf8_lossy(&done.stdout)),
        stderr: scrub_text(&stderr),
        truncated: done.truncated,
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
