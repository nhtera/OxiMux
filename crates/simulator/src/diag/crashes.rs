//! Crash reports, one summary each: a simulator's `.ips` files (written by
//! macOS's crash reporter, about 20 s after the app dies) and Android's crash
//! buffer (`logcat -b crash`).
//!
//! [`ReportScan`] and [`BufferScan`] are the panel's watchers: their first
//! look is a baseline, so a crash from before the panel opened never shows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::logs::{LOG_TIMEOUT, Level, device_now, predicate, sim_args, sim_executable};
use super::markers::{self, Kind};
use super::{Crash, DiagResult, FRAMES};
use crate::android::adb::Adb;
use crate::runner::Runner;
use crate::simctl::SimUdid;

/// Crashes in a window, and the lines that warn of trouble short of one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrashList {
    pub crashes: Vec<Crash>,
    /// Marked lines (a hang, an unhandled rejection), newest last.
    pub warnings: Vec<String>,
}

/// The most warnings a list carries (the newest).
const MAX_WARNINGS: usize = 20;
/// An `.ips` larger than this is not a crash report worth reading.
const MAX_REPORT_BYTES: u64 = 16 * 1024 * 1024;

// ---- a simulator's `.ips` reports ----

/// Where a simulator app's reports land: macOS's own folder (where they do
/// on Xcode 26), and the device's, in case a runtime writes there.
pub fn report_dirs(home: &Path, udid: &str) -> [PathBuf; 2] {
    [
        home.join("Library/Logs/DiagnosticReports"),
        home.join("Library/Developer/CoreSimulator/Devices").join(udid).join("data/Library/Logs/CrashReporter"),
    ]
}

/// One `.ips` report: who crashed, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ips {
    /// The executable's path (a simulator app's runs inside its device).
    pub proc_path: String,
    pub bundle_id: Option<String>,
    pub crash: Crash,
}

impl Ips {
    /// An app on the simulator `udid`.
    pub fn is_of(&self, udid: &str) -> bool {
        self.proc_path.contains(&format!("/CoreSimulator/Devices/{udid}/"))
    }

    /// `app`'s, by bundle id.
    pub fn is_app(&self, app: &str) -> bool {
        self.bundle_id.as_deref() == Some(app)
    }
}

/// An `.ips`: a JSON header line, then the JSON report.
pub fn parse_ips(text: &str) -> Option<Ips> {
    let (header, body) = text.split_once('\n')?;
    let header: Value = serde_json::from_str(header).ok()?;
    let body: Value = serde_json::from_str(body).ok()?;
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let bundle_id = str_of(&header, "bundleID").or_else(|| body.pointer("/bundleInfo/CFBundleIdentifier").and_then(Value::as_str).map(str::to_owned));
    let exception = body.get("exception").map(|e| {
        let kind = str_of(e, "type").unwrap_or_default();
        let detail = [str_of(e, "signal"), str_of(e, "subtype")].into_iter().flatten().collect::<Vec<_>>().join(", ");
        if detail.is_empty() { kind } else { format!("{kind} ({detail})") }
    });
    // What the app said as it died (an uncaught exception's reason), else
    // how it was ended.
    let said = body.get("asi").and_then(Value::as_object).and_then(|asi| {
        asi.values().filter_map(Value::as_array).flatten().filter_map(Value::as_str).map(str::trim).find(|s| !s.is_empty()).map(str::to_owned)
    });
    let reason = said.or_else(|| body.pointer("/termination/indicator").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
    let images = body.get("usedImages").and_then(Value::as_array);
    let frame = |f: &Value| {
        let image = f.get("imageIndex").and_then(Value::as_u64).and_then(|i| images?.get(i as usize)).and_then(|img| str_of(img, "name"));
        let image = image.unwrap_or_else(|| "???".into());
        match (str_of(f, "symbol"), f.get("imageOffset").and_then(Value::as_u64)) {
            (Some(symbol), _) => format!("{image} {symbol}"),
            (None, Some(offset)) => format!("{image} +0x{offset:x}"),
            (None, None) => image,
        }
    };
    // An uncaught exception's own stack says where it was thrown; else the
    // thread that crashed.
    let stack = body.get("lastExceptionBacktrace").and_then(Value::as_array).filter(|f| !f.is_empty()).or_else(|| {
        let at = body.get("faultingThread").and_then(Value::as_u64)? as usize;
        body.get("threads")?.as_array()?.get(at)?.get("frames")?.as_array()
    });
    let frames = stack.map(|f| f.iter().take(FRAMES).map(frame).collect()).unwrap_or_default();
    let crash = Crash {
        id: str_of(&header, "incident_id").or_else(|| str_of(&body, "incident"))?,
        time: str_of(&body, "captureTime").or_else(|| str_of(&header, "timestamp")).unwrap_or_default(),
        app: bundle_id.clone().or_else(|| str_of(&body, "procName")).unwrap_or_default(),
        exception: exception.unwrap_or_default(),
        reason,
        frames,
        pid: body.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()),
    };
    Some(Ips { proc_path: str_of(&body, "procPath").unwrap_or_default(), bundle_id, crash })
}

/// The `.ips` files in `dirs` changed at or after `since`, oldest first.
fn reports_since(dirs: &[PathBuf], since: SystemTime) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "ips"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let at = meta.modified().ok()?;
            (at >= since && meta.len() <= MAX_REPORT_BYTES).then(|| (at, e.path()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

fn read_ips(path: &Path) -> Option<Ips> {
    parse_ips(&std::fs::read_to_string(path).ok()?)
}

/// The crashes of `udid`'s apps (one `app`'s) reported since `since`.
pub fn sim_reports(home: &Path, udid: &str, since: SystemTime, app: Option<&str>) -> Vec<Crash> {
    let mut seen = HashSet::new();
    reports_since(&report_dirs(home, udid), since)
        .iter()
        .filter_map(|path| read_ips(path))
        .filter(|ips| ips.is_of(udid) && app.is_none_or(|a| ips.is_app(a)))
        .map(|ips| ips.crash)
        .filter(|crash| seen.insert(crash.id.clone()))
        .collect()
}

/// A simulator's crashes and warnings over the last `since_secs`.
pub fn sim_crashes(runner: &dyn Runner, home: &Path, udid: &SimUdid, since_secs: u32, app: Option<&str>) -> DiagResult<CrashList> {
    let process = app.map(|app| sim_executable(runner, udid, app)).transpose()?;
    let since = SystemTime::now() - Duration::from_secs(u64::from(since_secs));
    let mut crashes = sim_reports(home, udid.as_str(), since, app);
    // The log's marked lines: warnings, and a fatal error's own words for
    // the report of the process that said them.
    let mut warnings = Vec::new();
    match marked_sim_lines(runner, udid, since_secs, process.as_deref()) {
        Ok(lines) => {
            for line in lines {
                match markers::classify(&line) {
                    Some(Kind::Warning) => warnings.push(line),
                    Some(Kind::Fatal) => give_reason(&mut crashes, &line),
                    None => {}
                }
            }
        }
        Err(e) => warnings.push(format!("(the log could not be read for warnings: {e})")),
    }
    Ok(CrashList { crashes, warnings: newest(warnings) })
}

/// `log show` for every marker over the window.
fn marked_sim_lines(runner: &dyn Runner, udid: &SimUdid, since_secs: u32, process: Option<&str>) -> crate::Result<Vec<String>> {
    let any_marker = markers::MARKERS
        .iter()
        .map(|(marker, _)| format!("eventMessage CONTAINS \"{marker}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    let predicate = match predicate(process, Level::Default, None) {
        Some(process) => format!("{process} AND ({any_marker})"),
        None => any_marker,
    };
    let args = sim_args(udid, since_secs, Level::Default, Some(&predicate));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = runner.run("xcrun", &args, None, LOG_TIMEOUT)?.into_success("log show")?;
    Ok(out.stdout_str().lines().filter(|l| markers::classify(l).is_some()).map(str::to_owned).collect())
}

/// A compact log line's process id: `Name[pid:tid]`.
fn compact_pid(line: &str) -> Option<u32> {
    let open = line.find('[')?;
    line[open + 1..].split([':', ']']).next()?.parse().ok()
}

/// The marked words of a fatal line, as the reason of the crash its process
/// reported, when the report only names a signal.
fn give_reason(crashes: &mut [Crash], line: &str) {
    let Some(pid) = compact_pid(line) else { return };
    let Some((marker, _)) = markers::MARKERS.iter().find(|(m, _)| line.contains(m)) else { return };
    let words = &line[line.find(marker).unwrap_or(0)..];
    for crash in crashes.iter_mut().filter(|c| c.pid == Some(pid)) {
        crash.reason = words.trim().to_owned();
    }
}

/// The panel's watch on a simulator's reports: new `.ips` files since the
/// first look.
#[derive(Debug, Default)]
pub struct ReportScan {
    seen: Option<HashSet<PathBuf>>,
}

impl ReportScan {
    /// Crashes of `udid`'s apps reported since the last call; the first call
    /// only takes note of what is there.
    pub fn poll(&mut self, home: &Path, udid: &str) -> Vec<Crash> {
        let dirs = report_dirs(home, udid);
        let all = reports_since(&dirs, SystemTime::UNIX_EPOCH);
        let Some(seen) = &mut self.seen else {
            self.seen = Some(all.into_iter().collect());
            return Vec::new();
        };
        let mut crashes = Vec::new();
        for path in all {
            if seen.contains(&path) {
                continue;
            }
            match read_ips(&path) {
                Some(ips) => {
                    seen.insert(path);
                    if ips.is_of(udid) {
                        crashes.push(ips.crash);
                    }
                }
                // Still being written: looked at again next time, unless it
                // never parses.
                None if age(&path) > Duration::from_secs(60) => drop(seen.insert(path)),
                None => {}
            }
        }
        crashes
    }
}

fn age(path: &Path) -> Duration {
    std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok()).unwrap_or_default()
}

// ---- Android's crash buffer ----

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

/// A short, stable hash (FNV-1a) of the first frames, so one crash read twice
/// keeps its id.
fn frames_hash(frames: &[String]) -> u64 {
    frames.iter().take(3).flat_map(|f| f.bytes().chain([0])).fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// `pid 4321 (com.example.app)` at the end of a fatal signal line.
fn signal_process(msg: &str) -> Option<(u32, String)> {
    let at = msg.rfind("pid ")?;
    let (pid, name) = msg[at + 4..].split_once(' ')?;
    let name = name.trim().strip_prefix('(')?.strip_suffix(')')?;
    Some((pid.parse().ok()?, name.to_owned()))
}

/// The crashes in a `logcat -b crash -v threadtime` dump: Java/Kotlin
/// exceptions and native signals, oldest first.
pub fn parse_crash_buffer(text: &str) -> Vec<Crash> {
    let mut crashes: Vec<Crash> = Vec::new();
    // The crash being read, and how its lines are read.
    let mut open: Option<(usize, Reading)> = None;
    for line in text.lines().filter_map(parse_threadtime) {
        let msg = line.msg.trim_end();
        if line.tag == "AndroidRuntime" && msg.starts_with("FATAL EXCEPTION") {
            crashes.push(Crash { id: String::new(), time: line.time.into(), app: String::new(), exception: String::new(), reason: String::new(), frames: Vec::new(), pid: Some(line.pid) });
            open = Some((crashes.len() - 1, Reading::Java));
            continue;
        }
        if let Some(signal) = msg.split_once("Fatal signal ").map(|(_, s)| s) {
            let (pid, app) = signal_process(msg).unzip();
            let name = signal.split_once('(').and_then(|(_, s)| s.split_once(')')).map(|(s, _)| s.to_owned());
            crashes.push(Crash {
                id: String::new(),
                time: line.time.into(),
                app: app.unwrap_or_default(),
                exception: name.unwrap_or_else(|| "signal".into()),
                reason: format!("signal {}", signal.split(" in tid").next().unwrap_or(signal)),
                frames: Vec::new(),
                pid: pid.or(Some(line.pid)),
            });
            open = Some((crashes.len() - 1, Reading::Native));
            continue;
        }
        let Some((at, reading)) = open else { continue };
        let crash = &mut crashes[at];
        let text = msg.trim();
        if reading == Reading::Native {
            // `#00 pc 000000000004e1a8  /apex/…/libc.so (abort+164)`.
            if line.tag == "DEBUG" && text.starts_with('#') && text.contains(" pc ") && crash.frames.len() < FRAMES {
                crash.frames.push(text.split_whitespace().skip(3).collect::<Vec<_>>().join(" "));
            }
            continue;
        }
        if line.tag != "AndroidRuntime" || Some(line.pid) != crash.pid {
            open = None;
            continue;
        }
        if reading == Reading::JavaCause {
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
            open = Some((at, Reading::JavaCause));
        }
    }
    for crash in &mut crashes {
        crash.id = format!("{}@{}#{:016x}", crash.pid.unwrap_or(0), crash.time, frames_hash(&crash.frames));
    }
    crashes
}

/// How a crash's following lines are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    /// A Java/Kotlin exception: its message, then its `at` frames.
    Java,
    /// Its causes, which add nothing to the summary.
    JavaCause,
    /// A native signal: frames from debuggerd's backtrace.
    Native,
}

/// `app`'s crash: its package, or one of its processes (`com.x:remote`).
fn is_app(crash: &Crash, app: &str) -> bool {
    crash.app == app || crash.app.strip_prefix(app).is_some_and(|rest| rest.starts_with(':'))
}

/// `logcat -d` of `buffers` from the device's `start` (epoch seconds).
fn dump(adb: &Adb, serial: &str, buffers: &str, start: u64) -> crate::Result<String> {
    adb.logcat(serial, &["-d", "-b", buffers, "-v", "threadtime", "-T", &format!("{start}.000")], LOG_TIMEOUT)
}

/// An Android device's crashes and warnings over the last `since_secs`.
pub fn android_crashes(adb: &Adb, serial: &str, since_secs: u32, app: Option<&str>) -> DiagResult<CrashList> {
    let start = device_now(adb, serial)?.saturating_sub(u64::from(since_secs));
    let mut seen = HashSet::new();
    let crashes = parse_crash_buffer(&dump(adb, serial, "crash", start)?)
        .into_iter()
        .filter(|c| app.is_none_or(|a| is_app(c, a)) && seen.insert(c.id.clone()))
        .collect();
    let warnings = dump(adb, serial, "main,system", start)?
        .lines()
        .filter(|l| markers::classify(l) == Some(Kind::Warning) && app.is_none_or(|a| l.contains(a)))
        .map(str::to_owned)
        .collect();
    Ok(CrashList { crashes, warnings: newest(warnings) })
}

/// The panel's watch on an Android device's crash buffer: crashes not in
/// the buffer at the first look.
#[derive(Debug, Default)]
pub struct BufferScan {
    seen: Option<HashSet<String>>,
}

impl BufferScan {
    /// New crashes since the last call; the first call only takes note.
    pub fn poll(&mut self, adb: &Adb, serial: &str) -> crate::Result<Vec<Crash>> {
        let text = adb.logcat(serial, &["-d", "-b", "crash", "-v", "threadtime", "-t", "500"], Duration::from_secs(10))?;
        Ok(self.take(parse_crash_buffer(&text)))
    }

    fn take(&mut self, crashes: Vec<Crash>) -> Vec<Crash> {
        let Some(seen) = &mut self.seen else {
            self.seen = Some(crashes.into_iter().map(|c| c.id).collect());
            return Vec::new();
        };
        crashes.into_iter().filter(|c| seen.insert(c.id.clone())).collect()
    }
}

fn newest(mut lines: Vec<String>) -> Vec<String> {
    let skip = lines.len().saturating_sub(MAX_WARNINGS);
    lines.drain(..skip);
    lines
}

#[cfg(test)]
#[path = "crashes_tests.rs"]
mod tests;
