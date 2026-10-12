//! Crash reports, one summary each: a simulator's `.ips` files (written by
//! macOS's crash reporter, about 20 s after the app dies) and Android's crash
//! buffer (`logcat -b crash`).
//!
//! [`ReportScan`] and [`super::crash_buffer::BufferScan`] are the panel's
//! watchers: their first look is a baseline, so a crash from before the
//! panel opened never shows. Android's half is in [`super::crash_buffer`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::logs::{LOG_TIMEOUT, Level, predicate, sim_args, sim_executable};
use super::markers::{self, Kind};
use super::{Crash, DiagResult, FRAMES};
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
    /// When the app died (the report is written later).
    pub captured: Option<SystemTime>,
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
    let captured = str_of(&body, "captureTime").as_deref().and_then(capture_time);
    Some(Ips { proc_path: str_of(&body, "procPath").unwrap_or_default(), bundle_id, captured, crash })
}

/// `2026-10-12 15:54:29.8027 +0700` as a time.
pub fn capture_time(text: &str) -> Option<SystemTime> {
    let mut words = text.split_whitespace();
    let (date, clock, zone) = (words.next()?, words.next()?, words.next()?);
    let mut ymd = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let mut hms = clock.split(':');
    let (h, min) = (hms.next()?.parse::<i64>().ok()?, hms.next()?.parse::<i64>().ok()?);
    let sec = hms.next()?.parse::<f64>().ok()?;
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let zone: i64 = zone.get(1..)?.parse().ok()?;
    let offset = sign * ((zone / 100) * 3600 + (zone % 100) * 60);
    // Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
    // `days_from_civil`).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + min * 60 - offset;
    let at = Duration::from_secs_f64(secs as f64 + sec);
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + at)
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
                    Some(Kind::Fatal) => {
                        give_reason(&mut crashes, &line);
                        unreported(&mut crashes, &line);
                    }
                    None => {}
                }
            }
        }
        Err(e) => warnings.push(format!("(the log could not be read for warnings: {e})")),
    }
    let (crashes, left_out) = keep_newest(crashes);
    Ok(CrashList { crashes, warnings: newest(warnings, left_out) })
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
    // The report's own words win (an uncaught exception's reason).
    for crash in crashes.iter_mut().filter(|c| c.pid == Some(pid) && markers::classify(&c.reason).is_none()) {
        crash.reason = words.trim().to_owned();
    }
}

/// A fatal line whose process has no report yet (macOS writes it a few
/// seconds to ~20 s after the crash): listed as a crash without frames,
/// saying so, rather than not at all.
fn unreported(crashes: &mut Vec<Crash>, line: &str) {
    let Some(pid) = compact_pid(line) else { return };
    if crashes.iter().any(|c| c.pid == Some(pid)) {
        return;
    }
    let Some((marker, _)) = markers::MARKERS.iter().find(|(m, _)| line.contains(m)) else { return };
    // `2026-10-12 15:54:29.797 Df Crashy[61726:59a99a] …`.
    let time = line.get(..23).unwrap_or_default().to_owned();
    let process = line.split_whitespace().nth(3).and_then(|w| w.split('[').next()).unwrap_or_default().to_owned();
    crashes.push(Crash {
        id: format!("log:{pid}"),
        time,
        app: process,
        exception: "crashed (its report is still being written: run `oximux sim crashes` again in ~20 s for the frames)".into(),
        reason: line[line.find(marker).unwrap_or(0)..].trim().to_owned(),
        frames: Vec::new(),
        pid: Some(pid),
    });
}

/// The panel's watch on a simulator's reports: new `.ips` files since the
/// first look.
#[derive(Debug, Default)]
pub struct ReportScan {
    seen: Option<HashSet<PathBuf>>,
    /// When the first look was: a report written after it for a crash
    /// before it is not news.
    since: Option<SystemTime>,
}

impl ReportScan {
    /// Crashes of `udid`'s apps reported since the last call; the first call
    /// only takes note of what is there.
    pub fn poll(&mut self, home: &Path, udid: &str) -> Vec<Crash> {
        let dirs = report_dirs(home, udid);
        let all = reports_since(&dirs, SystemTime::UNIX_EPOCH);
        let Some(seen) = &mut self.seen else {
            self.seen = Some(all.into_iter().collect());
            self.since = Some(SystemTime::now());
            return Vec::new();
        };
        let since = self.since.unwrap_or(SystemTime::UNIX_EPOCH);
        let mut crashes = Vec::new();
        for path in all {
            if seen.contains(&path) {
                continue;
            }
            match read_ips(&path) {
                Some(ips) => {
                    seen.insert(path);
                    if ips.is_of(udid) && ips.captured.is_none_or(|at| at >= since) {
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

/// The most crashes a list carries (the newest): a reply stays well inside
/// the transport's limit whatever the device logged.
const MAX_CRASHES: usize = 20;

/// The newest [`MAX_CRASHES`] of `crashes` (oldest first), and how many
/// older ones were left out.
pub(super) fn keep_newest(mut crashes: Vec<Crash>) -> (Vec<Crash>, usize) {
    let skip = crashes.len().saturating_sub(MAX_CRASHES);
    crashes.drain(..skip);
    (crashes, skip)
}

/// The newest warnings, then a note when crashes were left out.
pub(super) fn newest(mut lines: Vec<String>, crashes_left_out: usize) -> Vec<String> {
    let skip = lines.len().saturating_sub(MAX_WARNINGS);
    lines.drain(..skip);
    if crashes_left_out > 0 {
        lines.push(format!("({crashes_left_out} older crashes left out: narrow --since or add --app)"));
    }
    lines
}

#[cfg(test)]
#[path = "crashes_tests.rs"]
mod tests;
