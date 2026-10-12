//! One-shot device logs: a simulator's `log show`, an Android device's
//! `logcat -d`, cut to the last lines (and bytes) asked for.
//!
//! The pure parts — the predicate, the arguments, the tail — are built here
//! and unit-tested; [`sim_logs`] and [`android_logs`] run them through the
//! [`Runner`].

use std::time::Duration;

use crate::android::adb::Adb;
use crate::runner::Runner;
use crate::simctl::{self, SimUdid};
use super::{DiagError, DiagResult};
use crate::state::{Reject, check_app, is_package};
use crate::{Result, SimError};

pub const DEFAULT_LINES: u32 = 200;
pub const MAX_LINES: u32 = 2000;
/// The most a reply carries; the newest lines are kept.
pub const MAX_BYTES: usize = 64 * 1024;
/// A log read that takes longer is stopped (narrow `--since`).
pub const LOG_TIMEOUT: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_secs(10);

/// The longest window a simulator's whole log (no `--app`, no `--grep`) is
/// read over; with info or debug messages, shorter.
const WHOLE_SIM_SECS: u32 = 600;
const WHOLE_SIM_DETAILED_SECS: u32 = 300;

/// Which messages, from least to most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Errors and faults only.
    Error,
    /// What the platform shows by default.
    #[default]
    Default,
    /// And informational messages.
    Info,
    /// And debug messages.
    Debug,
}

/// What to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogQuery {
    /// How far back, at most [`super::MAX_SINCE_SECS`].
    pub since_secs: u32,
    /// One app's messages (a bundle id or package name).
    pub app: Option<String>,
    /// Messages containing this, ignoring case.
    pub grep: Option<String>,
    pub level: Level,
    /// The last this many lines, at most [`MAX_LINES`].
    pub lines: u32,
}

/// The lines read, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogLines {
    pub lines: Vec<String>,
    /// Older lines were left out (by `lines` or [`MAX_BYTES`]).
    pub truncated: bool,
}

/// Why a query cannot run.
pub fn check(query: &LogQuery) -> std::result::Result<(), String> {
    if query.since_secs == 0 || query.since_secs > super::MAX_SINCE_SECS {
        return Err(format!("--since is between 1s and {}m", super::MAX_SINCE_SECS / 60));
    }
    if query.lines == 0 || query.lines > MAX_LINES {
        return Err(format!("--lines is between 1 and {MAX_LINES}"));
    }
    if let Some(app) = &query.app {
        check_app(app).map_err(|r| match r {
            Reject::BadInput(why) | Reject::Unsupported(why) => why,
        })?;
    }
    if query.grep.as_deref().is_some_and(|g| g.is_empty() || g.len() > 256 || g.chars().any(char::is_control)) {
        return Err("--grep is 1 to 256 characters on one line".into());
    }
    Ok(())
}

/// `s` inside a `log` predicate's double quotes.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// `log show`'s `--predicate`: the app's process, errors only, the text.
/// `None` when nothing narrows it.
pub fn predicate(process: Option<&str>, level: Level, grep: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(process) = process {
        parts.push(format!("process == {}", quoted(process)));
    }
    if level == Level::Error {
        parts.push("(messageType == error OR messageType == fault)".to_owned());
    }
    if let Some(grep) = grep {
        parts.push(format!("eventMessage CONTAINS[c] {}", quoted(grep)));
    }
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// `xcrun`'s arguments for a simulator's log.
pub fn sim_args(udid: &SimUdid, since_secs: u32, level: Level, predicate: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["simctl", "spawn", udid.as_str(), "log", "show", "--last"].map(str::to_owned).into();
    args.push(format!("{since_secs}s"));
    args.extend(["--style", "compact"].map(str::to_owned));
    match level {
        Level::Info => args.push("--info".into()),
        Level::Debug => args.extend(["--info", "--debug"].map(str::to_owned)),
        Level::Error | Level::Default => {}
    }
    if let Some(predicate) = predicate {
        args.extend(["--predicate".to_owned(), predicate.to_owned()]);
    }
    args
}

/// The executable an installed app runs as (what `log` calls its process).
pub fn sim_executable(runner: &dyn Runner, udid: &SimUdid, app: &str) -> DiagResult<String> {
    let bundle = match simctl::get_app_container(runner, udid, app, Some("app"), SHORT) {
        Ok(bundle) => bundle,
        Err(SimError::CommandFailed { .. }) => return Err(DiagError::NotFound(format!("{app} is not installed on this simulator"))),
        Err(e) => return Err(e.into()),
    };
    let plist = bundle.join("Info.plist");
    let plist = plist.to_string_lossy();
    let out = runner.run("plutil", &["-extract", "CFBundleExecutable", "raw", "-o", "-", &plist], None, SHORT)?.into_success("plutil")?;
    let exe = out.stdout_str().trim().to_owned();
    if exe.is_empty() {
        return Err(SimError::Parse { what: "CFBundleExecutable".into(), detail: format!("{app} names no executable") }.into());
    }
    Ok(exe)
}

/// A simulator's log for `query`.
pub fn sim_logs(runner: &dyn Runner, udid: &SimUdid, query: &LogQuery) -> DiagResult<LogLines> {
    // `log show` hands back everything before the tail is cut: the whole
    // device's chatter over a long window is too much to hold.
    let whole = if query.level >= Level::Info { WHOLE_SIM_DETAILED_SECS } else { WHOLE_SIM_SECS };
    if query.app.is_none() && query.grep.is_none() && query.since_secs > whole {
        return Err(DiagError::BadInput(format!(
            "a simulator's whole log is read {}m at a time at this level: add --app or --grep, or narrow --since",
            whole / 60
        )));
    }
    let process = query.app.as_deref().map(|app| sim_executable(runner, udid, app)).transpose()?;
    let predicate = predicate(process.as_deref(), query.level, query.grep.as_deref());
    let args = sim_args(udid, query.since_secs, query.level, predicate.as_deref());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut tail = Tail::new(query.lines as usize);
    let out = runner.run_lines("xcrun", &args, LOG_TIMEOUT, &mut |line| {
        // `log show` opens with what it filtered on and a column header.
        if !line.starts_with("Filtering the log data") && !line.starts_with("Timestamp ") {
            tail.push(line);
        }
    })?;
    simctl::require_success("log show", Some(udid.as_str()), out)?;
    Ok(tail.finish())
}

/// `logcat`'s priority filter.
fn priority(level: Level) -> &'static str {
    match level {
        Level::Error => "*:E",
        Level::Default | Level::Info => "*:I",
        Level::Debug => "*:V",
    }
}

/// `logcat`'s arguments: from `start` (the device's epoch seconds), one app's
/// uid or pid.
pub fn logcat_args(start: u64, only: Option<Only>, level: Level) -> Vec<String> {
    let mut args = vec!["-d".to_owned(), "-v".into(), "threadtime".into(), "-T".into(), format!("{start}.000")];
    match only {
        Some(Only::Uid(uid)) => args.push(format!("--uid={uid}")),
        Some(Only::Pid(pid)) => args.push(format!("--pid={pid}")),
        None => {}
    }
    args.push(priority(level).into());
    args
}

/// Android's first app uid: below it, uids are the system's, shared.
pub(super) const FIRST_APP_UID: u32 = 10_000;

/// Whose lines `logcat` keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Only {
    /// Every process the app ran as (it survives a crash and a restart).
    Uid(u32),
    /// The one running now (an Android older than `--uid`).
    Pid(u32),
}

/// The device's clock, in epoch seconds (log times are the device's).
pub fn device_now(adb: &Adb, serial: &str) -> Result<u64> {
    let out = adb.shell(serial, &["date", "+%s"], SHORT)?;
    out.trim().parse().map_err(|_| SimError::Parse { what: "date +%s".into(), detail: out })
}

/// `package:com.example.app uid:10123` → `Some(Some(10123))`; the package
/// listed without a uid (an older `pm`) → `Some(None)`; not listed → `None`.
pub fn parse_uid(out: &str, package: &str) -> Option<Option<u32>> {
    out.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let name = words.next()?.strip_prefix("package:")?;
        (name == package).then(|| words.find_map(|w| w.strip_prefix("uid:")?.parse().ok()))
    })
}

/// The app's uid (`None` inside: installed, uid unknown); `None` when the
/// package is not installed.
pub fn app_uid(adb: &Adb, serial: &str, package: &str) -> Result<Option<Option<u32>>> {
    Ok(parse_uid(&adb.shell(serial, &["pm", "list", "packages", "-U", package], SHORT)?, package))
}

/// The app's running pid (`pidof -s`), `None` when it is not running.
pub fn app_pid(adb: &Adb, serial: &str, package: &str) -> Result<Option<u32>> {
    Ok(adb.shell(serial, &["pidof", "-s", package], SHORT)?.trim().parse().ok())
}

/// An Android device's log for `query`.
pub fn android_logs(adb: &Adb, serial: &str, query: &LogQuery) -> DiagResult<LogLines> {
    let start = device_now(adb, serial)?.saturating_sub(u64::from(query.since_secs));
    let grep = query.grep.as_deref().map(str::to_lowercase);
    let read = |only| -> Result<LogLines> {
        let args = logcat_args(start, only, query.level);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut tail = Tail::new(query.lines as usize);
        adb.logcat_lines(serial, &args, LOG_TIMEOUT, &mut |line| {
            if !line.starts_with("--------- beginning of") && grep.as_deref().is_none_or(|g| line.to_lowercase().contains(g)) {
                tail.push(line);
            }
        })?;
        Ok(tail.finish())
    };
    Ok(match query.app.as_deref() {
        None => read(None)?,
        Some(app) => {
            if !is_package(app) {
                return Err(DiagError::BadInput(format!("`{app}` is not an Android package name")));
            }
            let uid = app_uid(adb, serial, app)?.ok_or_else(|| DiagError::NotFound(format!("{app} is not installed on this device")))?;
            // A system app shares its uid with the system (Settings runs as
            // uid 1000, as system_server does): only its own process is its.
            let by_uid = match uid.filter(|uid| *uid >= FIRST_APP_UID) {
                Some(uid) => match read(Some(Only::Uid(uid))) {
                    Ok(text) => Some(text),
                    // An Android without `--uid` refuses the option; a
                    // timeout is an answer, not a reason to read again.
                    Err(SimError::CommandFailed { .. }) => None,
                    Err(e) => return Err(e.into()),
                },
                None => None,
            };
            match by_uid {
                Some(text) => text,
                // Or an Android without `--uid`: the running process only.
                None => match app_pid(adb, serial, app)? {
                    Some(pid) => read(Some(Only::Pid(pid)))?,
                    None => LogLines::default(),
                },
            }
        }
    })
}

/// The newest lines of a stream: at most `max` of them, within
/// [`MAX_BYTES`]. Older ones are dropped as newer arrive, so a long output
/// costs no more memory than what is kept.
pub struct Tail {
    max: usize,
    lines: std::collections::VecDeque<String>,
    bytes: usize,
    dropped: bool,
}

impl Tail {
    pub fn new(max: usize) -> Self {
        Self { max, lines: Default::default(), bytes: 0, dropped: false }
    }

    pub fn push(&mut self, line: &str) {
        self.lines.push_back(line.to_owned());
        self.bytes += line.len() + 1;
        while self.lines.len() > self.max || self.bytes > MAX_BYTES {
            let Some(old) = self.lines.pop_front() else { break };
            self.bytes -= old.len() + 1;
            self.dropped = true;
        }
    }

    pub fn finish(self) -> LogLines {
        LogLines { lines: self.lines.into(), truncated: self.dropped }
    }
}

/// [`Tail`] over lines already in hand.
pub fn tail<'a>(lines: impl Iterator<Item = &'a str>, max: usize) -> LogLines {
    let mut tail = Tail::new(max);
    lines.for_each(|l| tail.push(l));
    tail.finish()
}

/// `30s`, `5m`, `1h` (or bare seconds) as seconds.
pub fn parse_since(text: &str) -> Option<u32> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last()? {
        (at, 's') => (&text[..at], 1),
        (at, 'm') => (&text[..at], 60),
        (at, 'h') => (&text[..at], 3600),
        _ => (text, 1),
    };
    number.parse::<u32>().ok()?.checked_mul(unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceId;
    use crate::runner::{CmdOutput, ScriptedRunner};

    fn udid() -> SimUdid {
        SimUdid::new(&DeviceId("U".into())).unwrap()
    }

    fn query() -> LogQuery {
        LogQuery { since_secs: 30, app: None, grep: None, level: Level::Default, lines: DEFAULT_LINES }
    }

    #[test]
    fn the_predicate_is_built_from_escaped_parts() {
        assert_eq!(predicate(None, Level::Default, None), None);
        assert_eq!(
            predicate(Some("Crashy"), Level::Error, Some(r#"say "hi" \o/"#)).unwrap(),
            r#"process == "Crashy" AND (messageType == error OR messageType == fault) AND eventMessage CONTAINS[c] "say \"hi\" \\o/""#
        );
    }

    #[test]
    fn a_simulator_log_asks_for_the_window_and_the_level() {
        assert_eq!(sim_args(&udid(), 30, Level::Default, None), ["simctl", "spawn", "U", "log", "show", "--last", "30s", "--style", "compact"]);
        let debug = sim_args(&udid(), 300, Level::Debug, Some("process == \"A\""));
        assert_eq!(&debug[6..], ["300s", "--style", "compact", "--info", "--debug", "--predicate", "process == \"A\""]);
    }

    #[test]
    fn an_app_is_read_by_its_executable_and_the_header_is_dropped() {
        // Joined as the code joins it (`\` on Windows).
        let plist = std::path::Path::new("/data/Crashy.app").join("Info.plist");
        let runner = ScriptedRunner::default()
            .expect("xcrun simctl get_app_container U dev.oximux.crashy app", CmdOutput::ok("/data/Crashy.app\n"))
            .expect(&format!("plutil -extract CFBundleExecutable raw -o - {}", plist.display()), CmdOutput::ok("Crashy\n"))
            .expect(
                "xcrun simctl spawn U log show --last 60s --style compact --predicate process == \"Crashy\" AND (messageType == error OR messageType == fault)",
                CmdOutput::ok("Filtering the log data using \"process == \\\"Crashy\\\"\"\nTimestamp               Ty Process[PID:TID]\n2026-10-12 15:54:29.797 E  Crashy[61726:59a99a] boom\n"),
            );
        let q = LogQuery { since_secs: 60, app: Some("dev.oximux.crashy".into()), level: Level::Error, ..query() };
        let got = sim_logs(&runner, &udid(), &q).unwrap();
        assert_eq!(got.lines, ["2026-10-12 15:54:29.797 E  Crashy[61726:59a99a] boom"]);
        assert!(!got.truncated);
    }

    #[test]
    fn logcat_reads_from_the_devices_clock_for_one_uid() {
        assert_eq!(logcat_args(1_760_000_000, Some(Only::Uid(10123)), Level::Error), ["-d", "-v", "threadtime", "-T", "1760000000.000", "--uid=10123", "*:E"]);
        assert_eq!(logcat_args(5, None, Level::Debug).last().unwrap(), "*:V");
        let pm = "package:com.example.app.debug uid:10124\npackage:com.example.app uid:10123\n";
        assert_eq!(parse_uid(pm, "com.example.app"), Some(Some(10123)));
        assert_eq!(parse_uid(pm, "com.other"), None);
        assert_eq!(parse_uid("package:com.old\n", "com.old"), Some(None), "installed, uid unknown");
    }

    #[test]
    fn android_logs_filter_by_uid_and_text() {
        let runner = ScriptedRunner::default()
            .expect("adb -s S shell date +%s", CmdOutput::ok("1760000100\n"))
            .expect("adb -s S shell pm list packages -U com.example.app", CmdOutput::ok("package:com.example.app uid:10123\n"))
            .expect(
                "adb -s S logcat -d -v threadtime -T 1760000040.000 --uid=10123 *:I",
                CmdOutput::ok("--------- beginning of main\n10-12 15:54:00.000 1 1 I App: Hello\n10-12 15:54:01.000 1 1 E App: BOOM here\n"),
            );
        let adb = Adb::new(&runner, std::path::Path::new("adb"));
        let q = LogQuery { since_secs: 60, app: Some("com.example.app".into()), grep: Some("boom".into()), ..query() };
        assert_eq!(android_logs(&adb, "S", &q).unwrap().lines, ["10-12 15:54:01.000 1 1 E App: BOOM here"]);
    }

    #[test]
    fn a_system_app_is_read_by_its_process_not_its_shared_uid() {
        let runner = ScriptedRunner::default()
            .expect("adb -s S shell date +%s", CmdOutput::ok("1760000100\n"))
            .expect("adb -s S shell pm list packages -U com.android.settings", CmdOutput::ok("package:com.android.settings uid:1000\n"))
            .expect("adb -s S shell pidof -s com.android.settings", CmdOutput::ok("4242\n"))
            .expect("adb -s S logcat -d -v threadtime -T 1760000070.000 --pid=4242 *:I", CmdOutput::ok("10-12 15:54:00.000 4242 4242 I Settings: open\n"));
        let adb = Adb::new(&runner, std::path::Path::new("adb"));
        let q = LogQuery { app: Some("com.android.settings".into()), ..query() };
        assert_eq!(android_logs(&adb, "S", &q).unwrap().lines.len(), 1);
    }

    #[test]
    fn the_tail_keeps_the_newest_lines_within_the_caps() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let got = tail(lines.iter().map(String::as_str), 3);
        assert_eq!(got.lines, ["line 7", "line 8", "line 9"]);
        assert!(got.truncated);
        let big = "x".repeat(40 * 1024);
        let got = tail([big.as_str(), big.as_str(), "last"].into_iter(), 10);
        assert_eq!(got.lines.len(), 2, "64 KiB keeps the newest that fit");
        assert_eq!(got.lines[1], "last");
    }

    #[test]
    fn queries_are_checked_before_anything_runs() {
        assert!(check(&query()).is_ok());
        assert!(check(&LogQuery { since_secs: 7200, ..query() }).is_err());
        assert!(check(&LogQuery { lines: 5000, ..query() }).is_err());
        assert!(check(&LogQuery { app: Some("x;rm -rf /".into()), ..query() }).is_err());
        assert!(check(&LogQuery { grep: Some("a\nb".into()), ..query() }).is_err());
    }

    #[test]
    fn a_simulators_whole_log_is_read_over_a_short_window() {
        let runner = ScriptedRunner::default();
        let long = LogQuery { since_secs: 3600, ..query() };
        assert!(matches!(sim_logs(&runner, &udid(), &long), Err(DiagError::BadInput(_))));
        let debug = LogQuery { since_secs: 600, level: Level::Debug, ..query() };
        assert!(matches!(sim_logs(&runner, &udid(), &debug), Err(DiagError::BadInput(_))));
        assert!(runner.calls().is_empty(), "nothing ran");
    }

    #[test]
    fn a_window_reads_as_seconds() {
        assert_eq!(parse_since("30s"), Some(30));
        assert_eq!(parse_since("5m"), Some(300));
        assert_eq!(parse_since("1h"), Some(3600));
        assert_eq!(parse_since("45"), Some(45));
        assert_eq!(parse_since("m"), None);
        assert_eq!(parse_since("ten"), None);
    }
}
