//! Building the iPhone control runner on this Mac, signed with the user's
//! team (the runner ships as sources: the fork's release tarball, bundled in
//! the app and pinned by its SHA-256 here).
//!
//! Under `~/Library/Application Support/OxiMux/ios-runner/`:
//! - `src-<sha>/`: the tarball, extracted afresh by every build;
//! - `derived/<udid>/`: that phone's build (`-derivedDataPath`), with
//!   `oximux.stamp` beside it.
//!
//! A build is current while its stamp still matches: a hash of the sources'
//! SHA, `xcodebuild -version`, the team, the phone, the provisioning
//! profile's expiry and the `.xctestrun` the build wrote — and while that
//! profile has not expired (a free team's lasts a week). The stamp is written
//! last, atomically, so a build cut short is never taken for a finished one.
//!
//! One build at a time per (team, phone) in this process, and one at a time
//! on this Mac (an `fd-lock` on `.lock`): a second caller waits, then finds
//! the first one's build current.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use fd_lock::RwLock as FdLock;
use sha2::{Digest, Sha256};

use super::team::valid_team_id;
use crate::child_ledger::{self, Entry, Ledger};
use crate::runner::Runner;
use crate::{Result, SimError};

/// The runner release OxiMux drives (`ios-runner-v<VERSION>` in the fork).
pub const RUNNER_VERSION: &str = "0.1.0";
/// Its source tarball's SHA-256 (`oximux-ios-runner-src-<VERSION>.tar.gz`).
pub const RUNNER_SHA256: &str = "f72fed55b3f4b00c30f700d6030196afd13ad72b1d24bb72df835126a1ed6667";
/// The tarball's file name, as bundled.
pub fn tarball_name() -> String {
    format!("oximux-ios-runner-src-{RUNNER_VERSION}.tar.gz")
}

/// A profile this close to its expiry is rebuilt rather than launched.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60 * 60);
/// What a failed build reports: its last lines naming an error, or its tail.
const FAILURE_LINES: usize = 6;

/// Where the runner's sources and builds live.
#[derive(Clone, Debug)]
pub struct RunnerHome {
    root: PathBuf,
}

impl RunnerHome {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn source(&self) -> PathBuf {
        self.root.join(format!("src-{RUNNER_SHA256}"))
    }

    pub fn derived(&self, udid: &str) -> PathBuf {
        self.root.join("derived").join(udid)
    }

    fn stamp(&self, udid: &str) -> PathBuf {
        self.derived(udid).join("oximux.stamp")
    }
}

/// A finished, current build: what `test-without-building` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    pub xctestrun: PathBuf,
    /// When its provisioning profile runs out.
    pub expires: Option<SystemTime>,
}

/// One build's inputs.
pub struct BuildRequest<'a> {
    pub home: &'a RunnerHome,
    /// The bundled source tarball.
    pub tarball: &'a Path,
    /// The signing team the user chose.
    pub team: &'a str,
    /// The phone's hardware UDID (no `iosdev:` prefix).
    pub udid: &'a str,
    /// `xcrun --find xcodebuild`, resolved.
    pub xcodebuild: &'a Path,
    /// `xcodebuild -version`, as printed.
    pub xcode_version: &'a str,
    pub ledger: Option<&'a Ledger>,
    pub cancel: &'a AtomicBool,
}

/// The current build for `request`, building it first when there is none —
/// one build at a time per team and phone, and per Mac. `progress` gets the
/// build's output, a line at a time.
pub fn ensure(runner: &dyn Runner, request: &BuildRequest, progress: &mut dyn FnMut(&str)) -> Result<Built> {
    check_inputs(request)?;
    let flight = flight(request.team, request.udid);
    let _flight = flight.lock().unwrap_or_else(|e| e.into_inner());
    fs::create_dir_all(request.home.root())?;
    let lock = fs::OpenOptions::new().create(true).write(true).truncate(false).open(request.home.root().join(".lock"))?;
    let mut lock = FdLock::new(lock);
    let _lock = lock.write()?;
    if let Some(built) = current(runner, request) {
        return Ok(built);
    }
    build(runner, request, progress)
}

/// The build for `request` when its stamp still matches and its profile has
/// not run out.
pub fn current(runner: &dyn Runner, request: &BuildRequest) -> Option<Built> {
    let stored = fs::read_to_string(request.home.stamp(request.udid)).ok()?;
    let (built, stamp) = inspect(runner, request).ok()?;
    let fresh = built.expires.is_some_and(|at| at > SystemTime::now() + EXPIRY_MARGIN);
    (stored.trim() == stamp && fresh).then_some(built)
}

/// Throws away the stamp, so the next [`ensure`] builds (a launch refused
/// for an expired or revoked profile).
pub fn invalidate(home: &RunnerHome, udid: &str) {
    let _ = fs::remove_file(home.stamp(udid));
}

fn check_inputs(request: &BuildRequest) -> Result<()> {
    if !valid_team_id(request.team) {
        return Err(SimError::Unsupported(format!("`{}` is not a signing team id", request.team)));
    }
    if request.udid.is_empty() || !request.udid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(SimError::DeviceNotFound(request.udid.to_owned()));
    }
    Ok(())
}

/// The in-process build locks, by (team, phone).
type Flights = Mutex<HashMap<(String, String), Arc<Mutex<()>>>>;

/// One lock per (team, phone) in this process.
fn flight(team: &str, udid: &str) -> Arc<Mutex<()>> {
    static FLIGHTS: OnceLock<Flights> = OnceLock::new();
    let mut flights = FLIGHTS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    flights.entry((team.to_owned(), udid.to_owned())).or_default().clone()
}

fn build(runner: &dyn Runner, request: &BuildRequest, progress: &mut dyn FnMut(&str)) -> Result<Built> {
    let home = request.home;
    invalidate(home, request.udid);
    let source = extract(runner, home, request.tarball)?;
    let derived = home.derived(request.udid);
    fs::create_dir_all(&derived)?;
    let argv = build_argv(request.xcodebuild, &source, request.team, request.udid, &derived);
    run_build(&argv, &source, request, progress)?;
    let (built, stamp) = inspect(runner, request)?;
    write_atomic(&home.stamp(request.udid), stamp.as_bytes())?;
    Ok(built)
}

/// `xcodebuild build-for-testing`, as the runner's PROTOCOL.md has it:
/// `OXIMUX_BUNDLE_PREFIX` names both targets' ids (never
/// `PRODUCT_BUNDLE_IDENTIFIER`, which would give them one).
fn build_argv(xcodebuild: &Path, source: &Path, team: &str, udid: &str, derived: &Path) -> Vec<String> {
    vec![
        xcodebuild.display().to_string(),
        "build-for-testing".into(),
        "-project".into(),
        source.join("OximuxRunner.xcodeproj").display().to_string(),
        "-scheme".into(),
        "OximuxRunner".into(),
        "-destination".into(),
        format!("id={udid}"),
        "-derivedDataPath".into(),
        derived.display().to_string(),
        "-allowProvisioningUpdates".into(),
        format!("DEVELOPMENT_TEAM={team}"),
        format!("OXIMUX_BUNDLE_PREFIX=dev.oximux.runner.t{team}"),
    ]
}

/// The pinned tarball, checked, extracted into a fresh `src-<sha>/`.
fn extract(runner: &dyn Runner, home: &RunnerHome, tarball: &Path) -> Result<PathBuf> {
    let sha = sha256_file(tarball).map_err(|e| SimError::HelperNotFound(format!("{}: {e}", tarball.display())))?;
    if sha != RUNNER_SHA256 {
        return Err(SimError::Unsupported(format!(
            "the bundled iPhone runner sources do not match this OxiMux (SHA-256 {sha}); reinstall OxiMux"
        )));
    }
    let staging = home.root().join(format!(".extract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let staged = staging.display().to_string();
    let tar = tarball.display().to_string();
    let out = runner.run("/usr/bin/tar", &["-xzf", &tar, "-C", &staged], None, Duration::from_secs(60))?;
    out.into_success("tar")?;
    let unpacked = staging.join(format!("oximux-ios-runner-src-{RUNNER_VERSION}"));
    if !unpacked.join("OximuxRunner.xcodeproj").is_dir() {
        let _ = fs::remove_dir_all(&staging);
        return Err(SimError::Protocol("the iPhone runner tarball has no OximuxRunner.xcodeproj".into()));
    }
    let source = home.source();
    let _ = fs::remove_dir_all(&source);
    fs::rename(&unpacked, &source)?;
    let _ = fs::remove_dir_all(&staging);
    Ok(source)
}

/// Runs the build as its own process group, recorded in the ledger, its
/// output to `progress`; stopped (with its group) on `cancel`.
fn run_build(argv: &[String], source: &Path, request: &BuildRequest, progress: &mut dyn FnMut(&str)) -> Result<()> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(source).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn()?;
    let pid = child.id();
    if let Some(ledger) = request.ledger
        && let Err(e) = ledger.record(Entry::xcodebuild(pid, argv.to_vec(), Some(request.udid.to_owned())))
    {
        tracing::warn!("could not record the runner build {pid} in the child ledger: {e}");
    }
    let (tx, rx) = mpsc::channel::<String>();
    let readers = [lines_to(child.stdout.take(), tx.clone()), lines_to(child.stderr.take(), tx)];
    let mut tail: Vec<String> = Vec::new();
    let mut cancelled = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                progress(&line);
                tail.push(line);
                if tail.len() > 200 {
                    tail.drain(..100);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !cancelled && request.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            child_ledger::stop_group(pid, Duration::from_secs(5));
        }
    }
    let status = child.wait()?;
    for reader in readers {
        let _ = reader.join();
    }
    if let Some(ledger) = request.ledger {
        let _ = ledger.remove(pid);
    }
    if cancelled {
        return Err(SimError::Cancelled);
    }
    if !status.success() {
        return Err(SimError::CommandFailed {
            program: "xcodebuild build-for-testing".into(),
            code: status.code(),
            stderr: failure_summary(&tail),
        });
    }
    Ok(())
}

/// What a failed build said: its `error:` lines (the last few), else its
/// last lines.
pub fn failure_summary(lines: &[String]) -> String {
    let errors: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| l.contains("error:")).collect();
    let picked: Vec<&str> = if errors.is_empty() {
        lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect()
    } else {
        errors
    };
    let mut seen = Vec::new();
    for line in picked.iter().rev() {
        if !seen.contains(line) {
            seen.push(*line);
        }
        if seen.len() == FAILURE_LINES {
            break;
        }
    }
    seen.reverse();
    seen.join("\n")
}

fn lines_to(pipe: Option<impl Read + Send + 'static>, tx: mpsc::Sender<String>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(pipe) = pipe else { return };
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    })
}

/// `setsid` in the child: the build (and the processes it starts) is one
/// group, stopped together.
pub(crate) fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe; nothing else runs between fork
    // and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The build's `.xctestrun` and profile expiry, and the stamp they make.
fn inspect(runner: &dyn Runner, request: &BuildRequest) -> Result<(Built, String)> {
    let products = request.home.derived(request.udid).join("Build/Products");
    let xctestrun = only_file(&products, |name| name.starts_with("OximuxRunner_") && name.ends_with(".xctestrun"))?;
    let mut expires: Option<SystemTime> = None;
    for app in fs::read_dir(products.join("Debug-iphoneos"))?.flatten() {
        let profile = app.path().join("embedded.mobileprovision");
        if !profile.is_file() {
            continue;
        }
        let at = profile_expiry(runner, &profile)?;
        expires = Some(expires.map_or(at, |e| e.min(at)));
    }
    let xctestrun_sha = sha256_file(&xctestrun)?;
    let stamp = stamp(request, expires, &xctestrun_sha);
    Ok((Built { xctestrun, expires }, stamp))
}

fn stamp(request: &BuildRequest, expires: Option<SystemTime>, xctestrun_sha: &str) -> String {
    let expiry = expires.and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let mut hash = Sha256::new();
    for part in [RUNNER_SHA256, request.xcode_version.trim(), request.team, request.udid, &expiry.to_string(), xctestrun_sha] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex(&hash.finalize())
}

/// When a provisioning profile runs out (`security cms -D` decodes it).
fn profile_expiry(runner: &dyn Runner, profile: &Path) -> Result<SystemTime> {
    let path = profile.display().to_string();
    let out = runner.run("/usr/bin/security", &["cms", "-D", "-i", &path], None, Duration::from_secs(20))?;
    let out = out.into_success("security cms")?;
    expiry_of(&out.stdout)
}

fn expiry_of(profile_plist: &[u8]) -> Result<SystemTime> {
    let parse = |detail: String| SimError::Parse { what: "provisioning profile".into(), detail };
    let value: plist::Value = plist::from_bytes(profile_plist).map_err(|e| parse(e.to_string()))?;
    let date = value
        .as_dictionary()
        .and_then(|d| d.get("ExpirationDate"))
        .and_then(plist::Value::as_date)
        .ok_or_else(|| parse("no ExpirationDate".into()))?;
    Ok(date.into())
}

fn only_file(dir: &Path, wanted: impl Fn(&str) -> bool) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> =
        fs::read_dir(dir)?.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(&wanted)).collect();
    found.sort();
    // A newer SDK's build sits beside an older one's: the newest name wins.
    found.pop().ok_or_else(|| SimError::Protocol(format!("no .xctestrun in {}", dir.display())))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex(&hash.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};

    /// Decodes every profile as one expiring at `expires`, and counts calls.
    struct Profiles {
        expires: String,
        calls: Mutex<usize>,
    }

    impl Runner for Profiles {
        fn run(&self, program: &str, args: &[&str], _: Option<&[u8]>, _: Duration) -> Result<CmdOutput> {
            assert_eq!((program, &args[..3]), ("/usr/bin/security", &["cms", "-D", "-i"][..]));
            *self.calls.lock().unwrap() += 1;
            Ok(CmdOutput::ok(profile(&self.expires)))
        }
    }

    fn security(expires: &str) -> Profiles {
        Profiles { expires: expires.into(), calls: Mutex::new(0) }
    }

    const UDID: &str = "00008130-000A1B2C3D4E5F60";

    fn profile(expires: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Name</key><string>iOS Team Provisioning Profile: *</string><key>ExpirationDate</key><date>{expires}</date></dict></plist>"#
        )
    }

    /// A finished build's products, as xcodebuild leaves them.
    fn fake_build(home: &RunnerHome, xctestrun: &str) {
        let products = home.derived(UDID).join("Build/Products");
        for app in ["OximuxRunner.app", "OximuxRunnerUITests-Runner.app"] {
            let dir = products.join("Debug-iphoneos").join(app);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("embedded.mobileprovision"), b"signed").unwrap();
        }
        fs::write(products.join("OximuxRunner_iphoneos26.2-arm64.xctestrun"), xctestrun).unwrap();
    }

    fn request<'a>(home: &'a RunnerHome, team: &'a str, udid: &'a str, cancel: &'a AtomicBool) -> BuildRequest<'a> {
        BuildRequest {
            home,
            tarball: Path::new("/nonexistent.tar.gz"),
            team,
            udid,
            xcodebuild: Path::new("/usr/bin/false"),
            xcode_version: "Xcode 26.2\nBuild version 17C52",
            ledger: None,
            cancel,
        }
    }

    fn stamp_for(home: &RunnerHome, team: &str, udid: &str, expires: &str) -> String {
        let cancel = AtomicBool::new(false);
        inspect(&security(expires), &request(home, team, udid, &cancel)).unwrap().1
    }

    #[test]
    fn the_stamp_changes_with_phone_team_expiry_and_xctestrun() {
        let dir = tempfile::tempdir().unwrap();
        let home = RunnerHome::new(dir.path());
        fake_build(&home, "<plist/>");
        let base = stamp_for(&home, "TEAM000001", UDID, "2030-01-01T00:00:00Z");
        assert_eq!(base, stamp_for(&home, "TEAM000001", UDID, "2030-01-01T00:00:00Z"));
        assert_ne!(base, stamp_for(&home, "TEAM000002", UDID, "2030-01-01T00:00:00Z"));
        assert_ne!(base, stamp_for(&home, "TEAM000001", UDID, "2030-01-08T00:00:00Z"));
        fake_build(&home, "<plist>edited</plist>");
        assert_ne!(base, stamp_for(&home, "TEAM000001", UDID, "2030-01-01T00:00:00Z"));
    }

    #[test]
    fn a_build_is_current_only_with_its_stamp_and_an_unexpired_profile() {
        let dir = tempfile::tempdir().unwrap();
        let home = RunnerHome::new(dir.path());
        let cancel = AtomicBool::new(false);
        let req = request(&home, "TEAM000001", UDID, &cancel);
        fake_build(&home, "<plist/>");
        // No stamp: not current (a build that was cut short).
        assert_eq!(current(&security("2030-01-01T00:00:00Z"), &req), None);
        fs::write(home.stamp(UDID), stamp_for(&home, "TEAM000001", UDID, "2030-01-01T00:00:00Z")).unwrap();
        let built = current(&security("2030-01-01T00:00:00Z"), &req).unwrap();
        assert!(built.xctestrun.ends_with("OximuxRunner_iphoneos26.2-arm64.xctestrun"));
        // Expired (the stamp written while it was not, as a week passed).
        fs::write(home.stamp(UDID), stamp_for(&home, "TEAM000001", UDID, "2020-01-01T00:00:00Z")).unwrap();
        assert_eq!(current(&security("2020-01-01T00:00:00Z"), &req), None);
        invalidate(&home, UDID);
        assert!(!home.stamp(UDID).exists());
    }

    #[test]
    fn a_bad_team_or_udid_never_reaches_xcodebuild() {
        let dir = tempfile::tempdir().unwrap();
        let home = RunnerHome::new(dir.path());
        let cancel = AtomicBool::new(false);
        let runner = ScriptedRunner::new([]);
        let mut progress = |_: &str| {};
        assert!(ensure(&runner, &request(&home, "TEAM00000;", UDID, &cancel), &mut progress).is_err());
        assert!(ensure(&runner, &request(&home, "TEAM000001", "../../etc", &cancel), &mut progress).is_err());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_tarball_that_is_not_the_pinned_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = RunnerHome::new(dir.path().join("home"));
        let tarball = dir.path().join(tarball_name());
        fs::write(&tarball, b"not the release").unwrap();
        fs::create_dir_all(home.root()).unwrap();
        let Err(SimError::Unsupported(why)) = extract(&ScriptedRunner::new([]), &home, &tarball) else { panic!() };
        assert!(why.contains("do not match"), "{why}");
    }

    #[test]
    fn the_build_command_follows_the_runners_contract() {
        let argv = build_argv(Path::new("/X/xcodebuild"), Path::new("/h/src-x"), "TEAM000001", UDID, Path::new("/h/derived/u"));
        let line = argv.join(" ");
        assert!(line.starts_with("/X/xcodebuild build-for-testing -project /h/src-x/OximuxRunner.xcodeproj -scheme OximuxRunner"));
        assert!(line.contains(&format!("-destination id={UDID}")) && line.contains("-allowProvisioningUpdates"));
        assert!(line.ends_with("DEVELOPMENT_TEAM=TEAM000001 OXIMUX_BUNDLE_PREFIX=dev.oximux.runner.tTEAM000001"));
        assert!(!line.contains("PRODUCT_BUNDLE_IDENTIFIER"));
    }

    #[test]
    fn a_failed_build_reports_its_errors() {
        let lines: Vec<String> = [
            "CompileSwift normal arm64",
            "/x/Foo.swift:3: error: cannot find 'x' in scope",
            "note: something",
            "error: No Account for Team \"TEAM000001\". Add a new account in Accounts settings.",
            "** TEST BUILD FAILED **",
        ]
        .map(String::from)
        .to_vec();
        let summary = failure_summary(&lines);
        assert_eq!(summary.lines().count(), 2);
        assert!(summary.ends_with("Add a new account in Accounts settings."));
        assert_eq!(failure_summary(&["a".into(), "".into(), "b".into()]), "a\nb");
    }

    #[test]
    fn profile_expiry_is_read_from_the_decoded_profile() {
        let at = expiry_of(profile("2027-09-30T19:17:50Z").as_bytes()).unwrap();
        assert_eq!(at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1_822_331_870);
        assert!(expiry_of(b"<plist version=\"1.0\"><dict/></plist>").is_err());
    }

    #[test]
    fn builds_of_one_team_and_phone_share_a_flight() {
        assert!(Arc::ptr_eq(&flight("TEAM000001", UDID), &flight("TEAM000001", UDID)));
        assert!(!Arc::ptr_eq(&flight("TEAM000001", UDID), &flight("TEAM000002", UDID)));
    }

    /// A second caller waits for the first's lock on this Mac, then finds
    /// the build it made current instead of building again.
    #[test]
    fn a_concurrent_build_waits_on_the_lock_then_reuses_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let home = RunnerHome::new(dir.path());
        fake_build(&home, "<plist/>");
        let file = fs::OpenOptions::new().create(true).write(true).truncate(false).open(home.root().join(".lock")).unwrap();
        let mut held = FdLock::new(file);
        let guard = held.write().unwrap();
        let (tx, rx) = mpsc::channel();
        let waiter = {
            let home = home.clone();
            std::thread::spawn(move || {
                let cancel = AtomicBool::new(false);
                let runner = security("2030-01-01T00:00:00Z");
                let built = ensure(&runner, &request(&home, "TEAM000001", UDID, &cancel), &mut |_| {});
                tx.send(()).unwrap();
                built
            })
        };
        // Still waiting while the other build holds the lock.
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        // The other build finishes: it leaves its stamp, then lets go.
        fs::write(home.stamp(UDID), stamp_for(&home, "TEAM000001", UDID, "2030-01-01T00:00:00Z")).unwrap();
        drop(guard);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Current: no build ran (the request's xcodebuild is /usr/bin/false
        // and its tarball does not exist).
        assert!(waiter.join().unwrap().unwrap().xctestrun.is_file());
    }
}
