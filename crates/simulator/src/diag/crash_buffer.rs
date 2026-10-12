//! Android's crash buffer (`logcat -b crash`): Java/Kotlin exceptions and
//! native signals, one summary each, and the warnings around them.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::crashes::{CrashList, keep_newest, newest};
use super::logs::{FIRST_APP_UID, LOG_TIMEOUT, app_uid, device_now};
use super::markers::{self, Kind};
use super::{Crash, DiagResult, FRAMES};
use crate::android::adb::Adb;

/// One `logcat -v threadtime` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine<'a> {
    /// `MM-DD hh:mm:ss.mmm`.
    pub time: &'a str,
    pub pid: u32,
    pub level: char,
    pub tag: &'a str,
    pub msg: &'a str,
}

/// `10-12 15:54:29.797  4321  4321 E AndroidRuntime: FATAL EXCEPTION: main`.
pub fn parse_threadtime(line: &str) -> Option<LogLine<'_>> {
    let time = line.get(..18)?;
    let rest = line.get(18..)?.trim_start();
    let (pid, rest) = rest.split_once(char::is_whitespace)?;
    let (_tid, rest) = rest.trim_start().split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let level = rest.chars().next()?;
    let (tag, msg) = rest.get(1..)?.split_once(": ")?;
    Some(LogLine { time, pid: pid.parse().ok()?, level, tag: tag.trim(), msg })
}

/// `pid 4321 (com.example.app)` at the end of a fatal signal line.
fn signal_process(msg: &str) -> Option<(u32, String)> {
    let at = msg.rfind("pid ")?;
    let (pid, name) = msg[at + 4..].split_once(' ')?;
    let name = name.trim().strip_prefix('(')?.strip_suffix(')')?;
    Some((pid.parse().ok()?, name.to_owned()))
}

fn blank(time: &str, pid: u32) -> Crash {
    Crash { id: String::new(), time: time.into(), app: String::new(), exception: String::new(), reason: String::new(), frames: Vec::new(), pid: Some(pid) }
}

/// The crashes in a `logcat -b crash -v threadtime` dump: Java/Kotlin
/// exceptions and native signals, oldest first.
pub fn parse_crash_buffer(text: &str) -> Vec<Crash> {
    let mut crashes: Vec<Crash> = Vec::new();
    // Java crashes being read, by process (two crashing at once interleave
    // their lines), and whether their causes have started.
    let mut java: HashMap<u32, (usize, bool)> = HashMap::new();
    // The native crash whose backtrace debuggerd may still be writing.
    let mut native: Option<usize> = None;
    for line in text.lines().filter_map(parse_threadtime) {
        let msg = line.msg.trim_end();
        let text = msg.trim();
        // `FATAL EXCEPTION: main`, or `*** FATAL EXCEPTION IN SYSTEM PROCESS`.
        if line.tag == "AndroidRuntime" && msg.contains("FATAL EXCEPTION") {
            crashes.push(blank(line.time, line.pid));
            java.insert(line.pid, (crashes.len() - 1, false));
            continue;
        }
        if let Some(signal) = msg.split_once("Fatal signal ").map(|(_, s)| s) {
            let (pid, app) = signal_process(msg).unzip();
            let name = signal.split_once('(').and_then(|(_, s)| s.split_once(')')).map(|(s, _)| s.to_owned());
            crashes.push(Crash {
                app: app.unwrap_or_default(),
                exception: name.unwrap_or_else(|| "signal".into()),
                reason: format!("signal {}", signal.split(" in tid").next().unwrap_or(signal)),
                ..blank(line.time, pid.unwrap_or(line.pid))
            });
            native = Some(crashes.len() - 1);
            continue;
        }
        if line.tag == "DEBUG" {
            let Some(at) = native else { continue };
            // debuggerd names the process it dumps: another one's lines are
            // not this crash's.
            let dumped = text.strip_prefix("pid: ").and_then(|t| t.split(',').next()).and_then(|p| p.trim().parse::<u32>().ok());
            if dumped.is_some_and(|p| Some(p) != crashes[at].pid) {
                native = None;
            // `#00 pc 000000000004e1a8  /apex/…/libc.so (abort+164)`.
            } else if text.starts_with('#') && text.contains(" pc ") && crashes[at].frames.len() < FRAMES {
                crashes[at].frames.push(text.split_whitespace().skip(3).collect::<Vec<_>>().join(" "));
            }
            continue;
        }
        let Some((at, causes)) = java.get_mut(&line.pid).filter(|_| line.tag == "AndroidRuntime") else { continue };
        let crash = &mut crashes[*at];
        if *causes {
            continue;
        }
        if let Some(process) = text.strip_prefix("Process: ") {
            crash.app = process.split(',').next().unwrap_or(process).trim().to_owned();
        } else if let Some(frame) = text.strip_prefix("at ") {
            if crash.frames.len() < FRAMES && !crash.exception.is_empty() {
                crash.frames.push(frame.to_owned());
            }
        } else if crash.exception.is_empty() {
            let (exception, reason) = text.split_once(": ").unwrap_or((text, ""));
            crash.exception = exception.to_owned();
            crash.reason = reason.to_owned();
        } else if text.starts_with("Caused by: ") || text.starts_with("... ") {
            // The first exception's own frames are enough.
            *causes = true;
        }
    }
    // The process and the millisecond: never the frames, which a native
    // crash's backtrace adds lines later (a read in between would give the
    // same crash a second id).
    for crash in &mut crashes {
        crash.id = format!("{}@{}", crash.pid.unwrap_or(0), crash.time);
    }
    crashes
}

/// The apps the user installed on an Android device (`pm list packages -3`):
/// a crash of anything else is the system's.
pub fn user_apps(adb: &Adb, serial: &str) -> crate::Result<HashSet<String>> {
    let out = adb.shell(serial, &["pm", "list", "packages", "-3"], Duration::from_secs(10))?;
    Ok(out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect())
}

/// `name` is `app`, or one of its processes (`com.x:remote`).
pub fn is_process_of(name: &str, app: &str) -> bool {
    name == app || name.strip_prefix(app).is_some_and(|rest| rest.starts_with(':'))
}

/// `app`'s crash.
pub fn is_app(crash: &Crash, app: &str) -> bool {
    is_process_of(&crash.app, app)
}

/// `ANR in com.example.app (com.example.app/.Main)` → `com.example.app`.
fn anr_target(line: &str) -> Option<&str> {
    line.split_once("ANR in ")?.1.split_whitespace().next()
}

/// `logcat -d` from the device's `start` (epoch seconds), `extra` options
/// after it; each line handed over as it comes.
fn dump(adb: &Adb, serial: &str, start: u64, extra: &[&str], line: &mut dyn FnMut(&str)) -> crate::Result<()> {
    let start = format!("{start}.000");
    let mut args = vec!["-d", "-v", "threadtime", "-T", &start];
    args.extend_from_slice(extra);
    adb.logcat_lines(serial, &args, LOG_TIMEOUT, line)
}

/// An Android device's crashes and warnings over the last `since_secs`.
/// With `app`, only that app's: its crashes, the system's ANR lines that
/// name it, and warnings its own processes wrote.
pub fn android_crashes(adb: &Adb, serial: &str, since_secs: u32, app: Option<&str>) -> DiagResult<CrashList> {
    let start = device_now(adb, serial)?.saturating_sub(u64::from(since_secs));
    let mut text = String::new();
    dump(adb, serial, start, &["-b", "crash"], &mut |l| {
        text.push_str(l);
        text.push('\n');
    })?;
    let mut seen = HashSet::new();
    let crashes: Vec<Crash> = parse_crash_buffer(&text).into_iter().filter(|c| app.is_none_or(|a| is_app(c, a)) && seen.insert(c.id.clone())).collect();
    let mut warnings = Vec::new();
    let mut keep = |line: &str, ok: bool| {
        if ok && markers::classify(line) == Some(Kind::Warning) {
            warnings.push(line.to_owned());
        }
    };
    match app {
        None => dump(adb, serial, start, &["-b", "main,system"], &mut |l| keep(l, true))?,
        Some(app) => {
            dump(adb, serial, start, &["-b", "system"], &mut |l| keep(l, anr_target(l).is_some_and(|t| is_process_of(t, app))))?;
            // What the app itself wrote (a promise rejection, a Flutter
            // error): by its own uid, never a system uid it shares.
            if let Some(uid) = app_uid(adb, serial, app)?.flatten().filter(|uid| *uid >= FIRST_APP_UID) {
                let uid = format!("--uid={uid}");
                dump(adb, serial, start, &["-b", "main", &uid], &mut |l| keep(l, anr_target(l).is_none()))?;
            }
        }
    }
    let (crashes, left_out) = keep_newest(crashes);
    Ok(CrashList { crashes, warnings: newest(warnings, left_out) })
}

/// The panel's watch on an Android device's crash buffer: crashes not in the
/// buffer at the first look.
#[derive(Debug, Default)]
pub struct BufferScan {
    /// Each crash seen, and how many frames it had then.
    seen: Option<HashMap<String, usize>>,
    /// The ones announced (not the baseline's).
    announced: HashSet<String>,
}

impl BufferScan {
    /// New crashes since the last call, and announced ones whose frames
    /// arrived since (a native backtrace lands after its signal line); the
    /// first call only takes note.
    pub fn poll(&mut self, adb: &Adb, serial: &str) -> crate::Result<Vec<Crash>> {
        let text = adb.logcat(serial, &["-d", "-b", "crash", "-v", "threadtime", "-t", "500"], Duration::from_secs(10))?;
        Ok(self.take(parse_crash_buffer(&text)))
    }

    pub(crate) fn take(&mut self, crashes: Vec<Crash>) -> Vec<Crash> {
        let Some(seen) = &mut self.seen else {
            self.seen = Some(crashes.into_iter().map(|c| (c.id, c.frames.len())).collect());
            return Vec::new();
        };
        let mut out = Vec::new();
        for crash in crashes {
            // Still being written (no `Process:` yet): looked at next time,
            // when the app it belongs to is known.
            if crash.app.is_empty() && !seen.contains_key(&crash.id) {
                continue;
            }
            match seen.insert(crash.id.clone(), crash.frames.len()) {
                None => {
                    self.announced.insert(crash.id.clone());
                    out.push(crash);
                }
                Some(before) if before < crash.frames.len() && self.announced.contains(&crash.id) => out.push(crash),
                Some(_) => {}
            }
        }
        out
    }
}
