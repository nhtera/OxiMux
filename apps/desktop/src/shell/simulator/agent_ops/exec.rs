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
use super::exec_files;
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
/// Emulator console commands (`adb emu …`) that act on the emulated device
/// only. Left out: `screenrecord` (writes host files), `avd` (the AVD's
/// files and snapshots), `kill`.
const EMU: [&str; 13] = ["geo", "sms", "gsm", "cdma", "power", "network", "sensor", "finger", "rotate", "event", "fold", "unfold", "ping"];
/// `adb emu network` subcommands (not `capture`, which writes a host file).
const NETWORK: [&str; 3] = ["speed", "delay", "status"];
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

/// What [`check`] allowed: the arguments, every host path made absolute and
/// real (checked inside the worktree), and which of them adb reads or writes
/// on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Checked {
    pub argv: Vec<String>,
    /// The worktree, real (symlinks resolved).
    pub root: PathBuf,
    /// Files adb reads (`install`, `push`), by position in `argv`.
    pub reads: Vec<usize>,
    /// Where a `pull` writes, by position, and how many device paths it pulls.
    pub writes: Option<(usize, usize)>,
}

/// `argv` if `tool` may run it on the attached device. `BadInput` names what
/// is not allowed.
pub(super) fn check(tool: Tool, argv: &[String], worktree: &Path) -> Result<Checked, SimErrorWire> {
    let bad = |why: String| Err(SimErrorWire::BadInput(why));
    let Some(sub) = argv.first() else {
        return bad("name the command after `--`, e.g. `oximux sim exec -- ui appearance dark`".into());
    };
    // Past `--` adb stops reading options: a word there would be a path this
    // check took for one.
    if argv.iter().any(|w| w == "--") {
        return bad("`--` is not allowed inside the command".into());
    }
    // adb hands `emu`'s words to the console as one line: a line break would
    // start another console command. No word needs one.
    if argv.iter().any(|w| w.chars().any(char::is_control)) {
        return bad("a control character (a line break, a tab) is not allowed in the command".into());
    }
    let root = std::fs::canonicalize(worktree).map_err(|_| SimErrorWire::NoDevice)?;
    let plain = |argv: &[String]| Ok(Checked { argv: argv.to_vec(), root: root.clone(), reads: Vec::new(), writes: None });
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
            plain(argv)
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
            if sub == "emu" && !argv.get(1).is_some_and(|c| EMU.contains(&c.as_str())) {
                return bad(format!("`adb emu` runs only: {}", EMU.join(", ")));
            }
            // `network capture` writes a host file.
            if sub == "emu" && argv[1] == "network" && !argv.get(2).is_some_and(|c| NETWORK.contains(&c.as_str())) {
                return bad(format!("`adb emu network` runs only: {}", NETWORK.join(", ")));
            }
            if matches!(sub.as_str(), "install" | "push" | "pull") { adb_paths(sub, argv, root) } else { plain(argv) }
        }
    }
}

/// `install`, `push` and `pull` with their host side checked and made real.
fn adb_paths(sub: &str, argv: &[String], root: PathBuf) -> Result<Checked, SimErrorWire> {
    let bad = |why: &str| Err(SimErrorWire::BadInput(why.into()));
    let mut checked = Checked { argv: argv.to_vec(), root, reads: Vec::new(), writes: None };
    let operands: Vec<usize> = (1..argv.len()).filter(|&i| !argv[i].starts_with('-')).collect();
    match sub {
        "install" => checked.reads = operands,
        // `push LOCAL… REMOTE`.
        "push" if operands.len() >= 2 => checked.reads = operands[..operands.len() - 1].to_vec(),
        "push" => return bad("push takes a file in the worktree, then where it goes on the device"),
        // `pull REMOTE… LOCAL`: the local side is required (else adb writes
        // where the app runs, outside any worktree).
        "pull" if operands.len() >= 2 => {
            let (remotes, local) = (&operands[..operands.len() - 1], operands[operands.len() - 1]);
            // adb names what it writes after the device path's last word: a
            // `..` there would climb out of the folder it writes into.
            if remotes.iter().any(|&i| argv[i].split('/').any(|w| w == "." || w == "..")) {
                return bad("a device path to pull may not contain `.` or `..`");
            }
            checked.writes = Some((local, remotes.len()));
        }
        "pull" => return bad("pull takes the device's file, then where it goes in the worktree"),
        _ => {}
    }
    for &i in &checked.reads {
        checked.argv[i] = inside(&argv[i], &checked.root, true)?.to_string_lossy().into_owned();
    }
    if let Some((i, _)) = checked.writes {
        checked.argv[i] = inside(&argv[i], &checked.root, false)?.to_string_lossy().into_owned();
    }
    Ok(checked)
}

/// `path` (relative to the worktree, `root`) resolved, symlinks and all, when
/// it is inside it and not in its `.git` folder. A path that need not exist
/// yet (a pull's target) is checked by its folder. One answer for "not
/// there" and "not inside", so this says nothing about files elsewhere.
fn inside(path: &str, root: &Path, exists: bool) -> Result<PathBuf, SimErrorWire> {
    let refused = || SimErrorWire::BadInput(format!("`{path}` is not a file in this worktree"));
    if path.starts_with('~') {
        return Err(refused());
    }
    let joined = root.join(path);
    let real = match std::fs::canonicalize(&joined) {
        Ok(real) => real,
        Err(_) if !exists => {
            let name = joined.file_name().filter(|n| *n != "..").ok_or_else(refused)?;
            let parent = joined.parent().ok_or_else(refused)?;
            std::fs::canonicalize(parent).map_err(|_| refused())?.join(name)
        }
        Err(_) => return Err(refused()),
    };
    match real.strip_prefix(root) {
        Ok(within) if !exec_files::in_git(within) => Ok(real),
        _ => Err(refused()),
    }
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
/// the user is asked with the exact command, then it runs, bounded. adb's
/// host files are handled by [`exec_files`]: what the user approved is what
/// adb reads, and where its pull lands.
pub(super) async fn run(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, wire: SimExecWire, target: &Target, cx: &mut AsyncApp) -> Out {
    let off = |hub: &Entity<SimulatorHub>, cx: &mut AsyncApp| !hub.read_with(cx, |hub, _| hub.exec_allowed());
    // Again here: the switch may have moved while consent was asked.
    if off(hub, cx) {
        return Err(SimErrorWire::Refused(OFF.into()));
    }
    let tool = Tool::of(udid)?;
    let worktree = target.worktree.clone();
    let checked = on_own_thread(move || check(tool, &wire.argv, &worktree)).await??;
    let (program, prefix, line) = match tool {
        Tool::Simctl => {
            let sim = simctl_ready(hub, udid, cx).await?;
            let mut args = vec!["simctl".to_owned(), checked.argv[0].clone(), sim.as_str().to_owned()];
            args.extend(checked.argv[1..].iter().cloned());
            let line = shown("xcrun", &args)?;
            ("xcrun".to_owned(), args, line)
        }
        Tool::Adb => {
            let device = android_device(hub, udid, cx).await?;
            let mut args = vec!["-s".to_owned(), device.serial().to_owned()];
            args.extend(checked.argv.iter().cloned());
            let line = shown("adb", &args)?;
            (device.adb().to_string_lossy().into_owned(), args, line)
        }
    };
    let real_device = udid.is_physical();
    confirm(hub, udid, name, AskKind::Exec { command: line, real_device }, target, cx).await?;
    // And after the answer: turning it off while asked takes effect.
    if off(hub, cx) {
        return Err(SimErrorWire::Refused(OFF.into()));
    }
    let skip = prefix.len() - checked.argv.len();
    let base = exec_files::private_base().map_err(|e| SimErrorWire::Failed(format!("no private folder for the transfer: {e}")))?;
    let ran = on_own_thread(move || transfer(&program, prefix, skip, &checked, &base)).await?;
    Ok(SimReplyWire::ExecOutput(output(ran?)))
}

/// Run the approved command, adb's host files going through a private
/// folder under `base` (see [`exec_files`]); `skip` words of `args` come
/// before `checked.argv`.
fn transfer(program: &str, mut args: Vec<String>, skip: usize, checked: &Checked, base: &Path) -> Result<(runner::Bounded, Option<u32>), SimErrorWire> {
    let failed = |e: std::io::Error| SimErrorWire::Failed(format!("no private folder for the transfer: {e}"));
    let private = if checked.reads.is_empty() && checked.writes.is_none() { None } else { Some(exec_files::private_dir(base).map_err(failed)?) };
    if let Some(dir) = &private {
        let inputs: Vec<PathBuf> = checked.reads.iter().map(|&i| PathBuf::from(&checked.argv[i])).collect();
        let staged = exec_files::stage_inputs(&checked.root, &inputs, dir.path())?;
        for (&i, copy) in checked.reads.iter().zip(staged) {
            args[skip + i] = copy.to_string_lossy().into_owned();
        }
        if let Some((i, _)) = checked.writes {
            let pulled = dir.path().join("pulled");
            std::fs::create_dir(&pulled).map_err(failed)?;
            args[skip + i] = pulled.to_string_lossy().into_owned();
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = runner::run_bounded(program, &refs, TIMEOUT, CAP)
        .map_err(|e| SimErrorWire::Failed(format!("{} did not run: {e}", redact_argv(program, &refs))))?;
    let placed = match (checked.writes, &private) {
        (Some((i, sources)), Some(dir)) if done.status == Some(0) => {
            let dest = Path::new(&checked.argv[i]);
            Some(exec_files::place_pulled(&checked.root, &dir.path().join("pulled"), dest, sources)?)
        }
        _ => None,
    };
    Ok((done, placed))
}

/// The reply: scrubbed, with a line saying why a stopped command stopped,
/// or how many pulled files were placed.
fn output((done, placed): (runner::Bounded, Option<u32>)) -> SimExecOutputWire {
    let mut stderr = String::from_utf8_lossy(&done.stderr).into_owned();
    let note = if let Some(n) = placed {
        Some(format!("[oximux: {n} file{} placed in the worktree]", if n == 1 { "" } else { "s" }))
    } else if done.timed_out {
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
