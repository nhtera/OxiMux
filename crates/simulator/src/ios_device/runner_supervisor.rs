//! Keeping an iPhone's control runner up while OxiMux drives the phone.
//!
//! A launch is `xcodebuild test-without-building -xctestrun <build>` for the
//! phone, as its own process group, recorded in the child ledger, with the
//! SHA-256 of a fresh token in `TEST_RUNNER_OXIMUX_TOKEN_SHA256` (xcodebuild
//! hands it to the runner and XCTest writes it into the result bundle, so
//! only the digest goes there; the token stays in memory, for the requests'
//! `Authorization` header). The runner prints `OXIMUX_RUNNER_LISTENING
//! port=N` once it listens on the phone's loopback (the first such line
//! wins: the system log repeats it with a prefix); commands then go to that
//! port through usbmux.
//!
//! Recovery is bounded: a runner that refuses the token, wedges, stops
//! listening or ends is relaunched — with a new token — at most once per
//! [`RecoveryClass`] in [`RECOVERY_WINDOW`] — a runner that ended between
//! commands included. After that the call fails with `GaveUp`, and the hub
//! turns control off and shows the error until the user retries (a new
//! supervisor, a new budget). A runner idle for [`IDLE_STOP`] is shut
//! down, and launched again by the next command.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::runner_build::own_process_group;
use super::runner_client::{self, Reply, RunnerClient, RunnerError};
use super::usbmux::Usbmux;
use crate::child_ledger::{self, Entry, Ledger};

/// How long a launch may take to listen (XCTest installs and starts the
/// runner on the phone first; a phone Xcode is still preparing for
/// development takes longer).
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
/// A runner with no command for this long is shut down.
pub const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// At most one automatic relaunch per failure class in this window.
pub const RECOVERY_WINDOW: Duration = Duration::from_secs(10 * 60);
const LISTENING: &str = "OXIMUX_RUNNER_LISTENING port=";
const FAILED: &str = "OXIMUX_RUNNER_FAILED";
/// What a failed launch keeps of xcodebuild's output.
const TAIL: usize = 200;

/// How to launch one phone's runner.
#[derive(Clone, Debug)]
pub struct RunnerSpec {
    /// The phone's hardware UDID (no `iosdev:` prefix).
    pub udid: String,
    /// `xcrun --find xcodebuild`, resolved.
    pub xcodebuild: PathBuf,
    /// The current build's `.xctestrun`.
    pub xctestrun: PathBuf,
    /// The build's derived data (the run's logs go there too).
    pub derived: PathBuf,
    pub ledger: Option<Arc<Ledger>>,
    pub transport: Transport,
}

/// How the runner's port is reached.
#[derive(Clone, Debug)]
pub enum Transport {
    /// On the phone's loopback, through usbmux.
    Usbmux(Usbmux),
    /// On this Mac's loopback (a runner in the Simulator listens there).
    Loopback,
}

impl Transport {
    fn connector(&self, udid: &str, port: u16) -> runner_client::Connector {
        match self {
            Self::Usbmux(mux) => runner_client::usbmux_connector(mux.clone(), udid.to_owned(), port),
            Self::Loopback => Arc::new(move || match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => Ok(Box::new(stream) as Box<dyn runner_client::RunnerStream>),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err(RunnerError::Refused),
                Err(e) => Err(RunnerError::Unreachable(e.to_string())),
            }),
        }
    }
}

impl RunnerSpec {
    fn argv(&self) -> Vec<String> {
        vec![
            self.xcodebuild.display().to_string(),
            "test-without-building".into(),
            "-xctestrun".into(),
            self.xctestrun.display().to_string(),
            "-destination".into(),
            format!("id={}", self.udid),
            "-derivedDataPath".into(),
            self.derived.display().to_string(),
        ]
    }
}

/// Why a runner could not be used, for the checklist.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// The phone has not trusted the developer certificate yet.
    #[error("On the iPhone, open Settings → General → VPN & Device Management and trust the developer, then try again")]
    NotTrusted,
    #[error("Turn on Developer Mode on the iPhone (Settings → Privacy & Security → Developer Mode)")]
    DeveloperModeOff,
    #[error("Unlock the iPhone, then try again")]
    Locked,
    /// The provisioning profile ran out: the build is redone.
    #[error("The runner's provisioning profile has expired")]
    ProfileExpired,
    #[error("Another test runner is using this iPhone")]
    Busy,
    /// Xcode is still preparing the phone for development (first use).
    #[error("Xcode is still preparing the iPhone for development. Keep it unlocked and connected, then try again")]
    Preparing,
    #[error("the iPhone is not connected over USB")]
    NotConnected,
    #[error("the iPhone's control runner did not start: {0}")]
    LaunchFailed(String),
    /// The runner failed the same way again within the recovery window.
    #[error("{0} (the control runner was already restarted for this)")]
    GaveUp(String),
    /// The command itself failed (`code` is the runner's), with what to do
    /// when the runner says.
    #[error("{message}{}", hint.as_deref().map(|h| format!(" ({h})")).unwrap_or_default())]
    Command { code: String, message: String, hint: Option<String> },
    #[error("{0}")]
    Runner(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<RunnerError> for ControlError {
    fn from(error: RunnerError) -> Self {
        match error {
            RunnerError::NotConnected => Self::NotConnected,
            RunnerError::Command { code, message, hint } => Self::Command { code, message, hint },
            other => Self::Runner(other.to_string()),
        }
    }
}

/// A launch's failure, from what xcodebuild said.
pub fn classify_launch_failure(output: &[String]) -> ControlError {
    let text = output.join("\n");
    let lower = text.to_lowercase();
    if lower.contains("developer mode") {
        ControlError::DeveloperModeOff
    } else if lower.contains("has not been explicitly trusted") || lower.contains("untrusted developer") || lower.contains("could not be verified") {
        ControlError::NotTrusted
    } else if lower.contains("profile has expired") || lower.contains("provisioning profile") && lower.contains("expired") {
        ControlError::ProfileExpired
    } else if lower.contains("is locked") || lower.contains("passcode protected") || lower.contains("unlock") {
        ControlError::Locked
    } else if lower.contains("preparing") || lower.contains("device is busy") {
        ControlError::Preparing
    } else if lower.contains("already running") || lower.contains("another test") {
        ControlError::Busy
    } else {
        ControlError::LaunchFailed(super::runner_build::failure_summary(output))
    }
}

/// The port in a line of the runner's output, when it says it listens.
pub fn listening_port(line: &str) -> Option<u16> {
    let rest = &line[line.find(LISTENING)? + LISTENING.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|port| *port != 0)
}

/// The ways a running runner fails that a relaunch may cure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecoveryClass {
    /// It refused the token.
    Unauthorized,
    /// A command overran (`RUNNER_WEDGED`, or no answer in time).
    Wedged,
    /// Nothing listens on its port.
    Refused,
    /// Its xcodebuild ended.
    Exited,
}

impl RecoveryClass {
    fn of(error: &RunnerError, exited: bool) -> Option<Self> {
        if exited {
            return Some(Self::Exited);
        }
        match error {
            RunnerError::Unauthorized => Some(Self::Unauthorized),
            RunnerError::Refused => Some(Self::Refused),
            RunnerError::Timeout(_) => Some(Self::Wedged),
            RunnerError::Command { code, .. } if code == "RUNNER_WEDGED" => Some(Self::Wedged),
            _ => None,
        }
    }

}

/// Whether a command failed before the runner could act on it (no stream,
/// or the token turned away first), so it may go to a relaunched runner.
fn never_ran(error: &RunnerError) -> bool {
    matches!(error, RunnerError::Refused | RunnerError::Unauthorized)
}

/// When each class was last recovered from.
#[derive(Debug, Default)]
pub struct Recovery {
    last: HashMap<RecoveryClass, Instant>,
}

impl Recovery {
    /// Whether `class` may be recovered from `now` (and, if so, note it).
    pub fn allow(&mut self, class: RecoveryClass, now: Instant) -> bool {
        match self.last.get(&class) {
            Some(at) if now.duration_since(*at) < RECOVERY_WINDOW => false,
            _ => {
                self.last.insert(class, now);
                true
            }
        }
    }
}

/// One launched runner.
struct Running {
    child: Arc<Mutex<Child>>,
    pid: u32,
    client: RunnerClient,
    exited: Arc<AtomicBool>,
}

impl Running {
    fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }
}

/// Starts runners for one phone and keeps one up while it is used.
pub struct RunnerSupervisor {
    inner: Arc<Inner>,
}

struct Inner {
    spec: RunnerSpec,
    running: Mutex<Option<Running>>,
    /// One command at a time per phone (the runner turns a second away).
    in_flight: Mutex<()>,
    recovery: Mutex<Recovery>,
    last_used: Mutex<Instant>,
    /// Set by [`RunnerSupervisor::stop`]: a launch under way gives up.
    stopping: AtomicBool,
    idle_stop: Duration,
}

impl RunnerSupervisor {
    pub fn new(spec: RunnerSpec) -> Self {
        Self::with_idle_stop(spec, IDLE_STOP)
    }

    fn with_idle_stop(spec: RunnerSpec, idle_stop: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                spec,
                running: Mutex::new(None),
                in_flight: Mutex::new(()),
                recovery: Mutex::new(Recovery::default()),
                last_used: Mutex::new(Instant::now()),
                stopping: AtomicBool::new(false),
                idle_stop,
            }),
        }
    }

    pub fn udid(&self) -> &str {
        &self.inner.spec.udid
    }

    /// Whether a runner is up (launched and not ended).
    pub fn is_running(&self) -> bool {
        lock(&self.inner.running).as_ref().is_some_and(|r| !r.has_exited())
    }

    /// Launches the runner now (the panel's "Enable control"), unless one is
    /// up already.
    pub fn start(&self) -> Result<(), ControlError> {
        self.start_unless(&AtomicBool::new(false))
    }

    /// [`start`](Self::start), given up (`Cancelled`) once `cancel` is set.
    pub fn start_unless(&self, cancel: &AtomicBool) -> Result<(), ControlError> {
        let _one = lock(&self.inner.in_flight);
        self.inner.stopping.store(false, Ordering::Relaxed);
        let mut running = lock(&self.inner.running);
        if running.as_ref().is_some_and(|r| !r.has_exited()) {
            return Ok(());
        }
        let launched = launch(&self.inner.spec, &self.inner.stopping, Some(cancel))?;
        let pid = launched.pid;
        *running = Some(launched);
        drop(running);
        watch_idle(Arc::downgrade(&self.inner), pid);
        Ok(())
    }

    /// Runs `command`, launching the runner first when none is up, and
    /// recovering (within bounds) when it fails.
    pub fn call(&self, command: &str, fields: Value) -> Result<Reply, ControlError> {
        let _one = lock(&self.inner.in_flight);
        self.inner.stopping.store(false, Ordering::Relaxed);
        *lock(&self.inner.last_used) = Instant::now();
        let mut relaunched = false;
        loop {
            let (client, exited) = self.client()?;
            let error = match client.call(command, fields.clone()) {
                Ok(reply) => {
                    *lock(&self.inner.last_used) = Instant::now();
                    return Ok(reply);
                }
                Err(error) => error,
            };
            // Turned off or quit while it ran: the failure is that stop's,
            // and nothing is launched again behind it.
            if self.inner.stopping.load(Ordering::Relaxed) {
                return Err(ControlError::Cancelled);
            }
            let ended = exited.load(Ordering::Relaxed);
            let Some(class) = RecoveryClass::of(&error, ended) else { return Err(error.into()) };
            tracing::info!(udid = %self.inner.spec.udid, ?class, "the iPhone control runner failed: {error}");
            self.shutdown_running(false);
            if relaunched || !lock(&self.inner.recovery).allow(class, Instant::now()) {
                return Err(ControlError::GaveUp(error.to_string()));
            }
            relaunched = true;
            // Only a command that never reached the runner goes to the new
            // one; one that may have run is not done twice — the new runner
            // is up for the next.
            if !never_ran(&error) {
                self.client()?;
                return Err(error.into());
            }
        }
    }

    /// Shuts the runner down (detach, control turned off), after the
    /// command in flight.
    pub fn stop(&self) {
        self.inner.stopping.store(true, Ordering::Relaxed);
        let _one = lock(&self.inner.in_flight);
        self.shutdown_running(true);
    }

    /// Ends the runner now (quit, or a command that must not be waited
    /// for): its group is killed at once — on quit no thread survives to
    /// escalate a `SIGTERM`, which `xcodebuild` ignores — and the command in
    /// flight fails; the reaping happens off this thread.
    pub fn abort(&self) {
        self.inner.stopping.store(true, Ordering::Relaxed);
        // A launch under way sees `stopping` within a tenth of a second.
        let Some(running) = lock(&self.inner.running).take() else { return };
        if !running.has_exited() {
            child_ledger::kill_group(running.pid);
        }
        let spec = self.inner.spec.clone();
        std::thread::spawn(move || end_process(&spec, &running.child, running.pid, &running.exited));
    }

    /// The running runner's client, launching one when none is up.
    fn client(&self) -> Result<(RunnerClient, Arc<AtomicBool>), ControlError> {
        let mut running = lock(&self.inner.running);
        if let Some(r) = running.as_ref() {
            if !r.has_exited() {
                return Ok((r.client.clone(), r.exited.clone()));
            }
            let r = running.take().expect("checked");
            finish(&self.inner.spec, r, false);
            // It ended on its own (a stop takes it out of `running` first):
            // launching again is a recovery, bounded like any other.
            if !lock(&self.inner.recovery).allow(RecoveryClass::Exited, Instant::now()) {
                return Err(ControlError::GaveUp("the iPhone's control runner ended".into()));
            }
        }
        let launched = launch(&self.inner.spec, &self.inner.stopping, None)?;
        let handles = (launched.client.clone(), launched.exited.clone());
        let pid = launched.pid;
        *running = Some(launched);
        drop(running);
        watch_idle(Arc::downgrade(&self.inner), pid);
        Ok(handles)
    }

    fn shutdown_running(&self, polite: bool) {
        if let Some(r) = lock(&self.inner.running).take() {
            finish(&self.inner.spec, r, polite);
        }
    }
}

impl Drop for RunnerSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Ends a runner: `shutdown` first when asked to be polite (the run ends
/// cleanly), then its process group, and its ledger entry.
fn finish(spec: &RunnerSpec, running: Running, polite: bool) {
    if polite && !running.has_exited() {
        let _ = running.client.call("shutdown", Value::Null);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.has_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    end_process(spec, &running.child, running.pid, &running.exited);
}

/// Stops a runner's xcodebuild (and its group) unless it has ended, reaps
/// it, and drops its ledger entry.
fn end_process(spec: &RunnerSpec, child: &Mutex<Child>, pid: u32, exited: &AtomicBool) {
    if !exited.load(Ordering::Relaxed) {
        child_ledger::stop_group(pid, Duration::from_secs(5));
    }
    let _ = lock(child).wait();
    exited.store(true, Ordering::Relaxed);
    if let Some(ledger) = &spec.ledger {
        let _ = ledger.remove(pid);
    }
}

/// Each launch leaves a result bundle (~100 KB): only the last few stay.
const KEPT_RESULTS: usize = 3;

/// Launches the runner and waits for it to listen.
fn launch(spec: &RunnerSpec, stopping: &AtomicBool, cancel: Option<&AtomicBool>) -> Result<Running, ControlError> {
    prune_results(&spec.derived.join("Logs/Test"), KEPT_RESULTS);
    let token = runner_client::new_token();
    let argv = spec.argv();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env("TEST_RUNNER_OXIMUX_TOKEN_SHA256", runner_client::token_digest(&token))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn().map_err(|e| ControlError::LaunchFailed(e.to_string()))?;
    let pid = child.id();
    if let Some(ledger) = &spec.ledger
        && let Err(e) = ledger.record(Entry::xcodebuild(pid, argv.clone(), Some(spec.udid.clone())))
    {
        tracing::warn!("could not record the iPhone runner {pid} in the child ledger: {e}");
    }
    let output = Output::default();
    output.read(child.stdout.take());
    output.read(child.stderr.take());
    let child = Arc::new(Mutex::new(child));
    let exited = Arc::new(AtomicBool::new(false));
    watch_exit(child.clone(), exited.clone());
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let failure = loop {
        if let Some(port) = output.port() {
            let connector = spec.transport.connector(&spec.udid, port);
            let client = RunnerClient::new(connector, &token);
            return Ok(Running { child, pid, client, exited });
        }
        if output.failed() || exited.load(Ordering::Relaxed) {
            // Whatever it printed on the way out.
            std::thread::sleep(Duration::from_millis(300));
            break classify_launch_failure(&output.tail());
        }
        if stopping.load(Ordering::Relaxed) || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break ControlError::Cancelled;
        }
        if Instant::now() >= deadline {
            break ControlError::LaunchFailed(format!("it did not start listening within {}s", LAUNCH_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if matches!(failure, ControlError::Cancelled) && !exited.load(Ordering::Relaxed) {
        // Quit, Cancel or Turn off mid-launch: on quit nothing is left to
        // escalate the SIGTERM `end_process` starts with, and xcodebuild
        // ignores it.
        child_ledger::kill_group(pid);
    }
    end_process(spec, &child, pid, &exited);
    Err(failure)
}

/// Removes all but the newest `keep` `.xcresult` bundles in `dir` (their
/// names start with the run's date and time).
fn prune_results(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut results: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "xcresult")).collect();
    results.sort();
    for old in results.iter().rev().skip(keep) {
        let _ = std::fs::remove_dir_all(old);
    }
}

/// What xcodebuild printed: the listening port, a failure, the last lines.
#[derive(Clone, Default)]
struct Output {
    inner: Arc<Mutex<OutputState>>,
}

#[derive(Default)]
struct OutputState {
    port: Option<u16>,
    failed: bool,
    tail: VecDeque<String>,
}

impl Output {
    /// Reads `pipe` a line at a time until it closes.
    fn read(&self, pipe: Option<impl Read + Send + 'static>) {
        let output = self.clone();
        std::thread::spawn(move || {
            let Some(pipe) = pipe else { return };
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { return };
                output.push(line);
            }
        });
    }

    fn push(&self, line: String) {
        let mut state = lock(&self.inner);
        if state.port.is_none() {
            state.port = listening_port(&line);
        }
        state.failed |= line.contains(FAILED) || line.contains("** TEST EXECUTE FAILED **");
        state.tail.push_back(line);
        if state.tail.len() > TAIL {
            state.tail.pop_front();
        }
    }

    fn port(&self) -> Option<u16> {
        lock(&self.inner).port
    }

    fn failed(&self) -> bool {
        lock(&self.inner).failed
    }

    fn tail(&self) -> Vec<String> {
        lock(&self.inner).tail.iter().cloned().collect()
    }
}

/// Notes when xcodebuild has ended (whatever it left holding its pipes).
fn watch_exit(child: Arc<Mutex<Child>>, exited: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        loop {
            if exited.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(Some(_)) | Err(_) = lock(&child).try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        exited.store(true, Ordering::Relaxed);
    });
}

/// Shuts the runner `pid` down once it has been idle for `idle_stop`; ends
/// with that runner (or the supervisor).
fn watch_idle(inner: Weak<Inner>, pid: u32) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(inner) = inner.upgrade() else { return };
            let Ok(_one) = inner.in_flight.try_lock() else { continue };
            let mut running = lock(&inner.running);
            // Gone, or another launch's: not this watch's any more.
            let Some(r) = running.as_ref().filter(|r| r.pid == pid) else { return };
            if r.has_exited() {
                return;
            }
            if lock(&inner.last_used).elapsed() >= inner.idle_stop {
                let r = running.take().expect("checked");
                drop(running);
                tracing::info!(udid = %inner.spec.udid, "stopping the idle iPhone control runner");
                finish(&inner.spec, r, true);
                return;
            }
        }
    });
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_is_the_first_listening_line_even_with_a_prefix() {
        assert_eq!(listening_port("OXIMUX_RUNNER_LISTENING port=50168"), Some(50168));
        assert_eq!(listening_port("2026-10-08 02:40:01.123 OximuxRunnerUITests-Runner[812:1]: OXIMUX_RUNNER_LISTENING port=50168\r"), Some(50168));
        assert_eq!(listening_port("OXIMUX_RUNNER_LISTENING port="), None);
        assert_eq!(listening_port("OXIMUX_RUNNER_LISTENING port=0"), None);
        assert_eq!(listening_port("OXIMUX_RUNNER_LISTENING port=99999"), None);
        assert_eq!(listening_port("Testing started"), None);
        let output = Output::default();
        output.push("Testing started".into());
        output.push("OXIMUX_RUNNER_LISTENING port=50168".into());
        output.push("[log] OXIMUX_RUNNER_LISTENING port=51111".into());
        assert_eq!(output.port(), Some(50168));
    }

    #[test]
    fn launch_failures_name_what_to_do_on_the_phone() {
        let says = |line: &str| classify_launch_failure(&[line.to_owned()]);
        assert_eq!(
            says("Unable to launch dev.oximux.runner.tX.uitests.xctrunner because it has an invalid code signature, inadequate entitlements or its profile has not been explicitly trusted by the user."),
            ControlError::NotTrusted
        );
        assert_eq!(says("error: Developer Mode disabled. To use iPhone for development, enable Developer Mode in Settings."), ControlError::DeveloperModeOff);
        assert_eq!(says("The provisioning profile has expired."), ControlError::ProfileExpired);
        assert_eq!(says("Unlock Jo’s iPhone to Continue"), ControlError::Locked);
        assert_eq!(says("A test runner is already running on this device"), ControlError::Busy);
        // Xcode's first-connect wait is not another runner.
        assert_eq!(says("Device is busy (Preparing Jo’s iPhone): waiting for the device"), ControlError::Preparing);
        let ControlError::LaunchFailed(why) = says("error: something else entirely") else { panic!() };
        assert_eq!(why, "error: something else entirely");
    }

    #[test]
    fn each_failure_class_is_recovered_from_once_per_window() {
        let mut recovery = Recovery::default();
        let start = Instant::now();
        assert!(recovery.allow(RecoveryClass::Refused, start));
        assert!(!recovery.allow(RecoveryClass::Refused, start + Duration::from_secs(60)));
        // Another class has its own allowance.
        assert!(recovery.allow(RecoveryClass::Wedged, start + Duration::from_secs(60)));
        assert!(recovery.allow(RecoveryClass::Refused, start + RECOVERY_WINDOW));
    }

    #[test]
    fn failures_map_to_their_recovery_class() {
        assert_eq!(RecoveryClass::of(&RunnerError::Unauthorized, false), Some(RecoveryClass::Unauthorized));
        assert_eq!(RecoveryClass::of(&RunnerError::Refused, false), Some(RecoveryClass::Refused));
        assert_eq!(RecoveryClass::of(&RunnerError::Timeout(45), false), Some(RecoveryClass::Wedged));
        let wedged = RunnerError::Command { code: "RUNNER_WEDGED".into(), message: String::new(), hint: None };
        assert_eq!(RecoveryClass::of(&wedged, false), Some(RecoveryClass::Wedged));
        assert_eq!(RecoveryClass::of(&RunnerError::Lost("x".into()), true), Some(RecoveryClass::Exited));
        // The command's own failure, and an unplugged phone: no relaunch.
        let failed = RunnerError::Command { code: "XCTEST_FAILED".into(), message: String::new(), hint: None };
        assert_eq!(RecoveryClass::of(&failed, false), None);
        assert_eq!(RecoveryClass::of(&RunnerError::NotConnected, false), None);
        assert!(never_ran(&RunnerError::Refused) && never_ran(&RunnerError::Unauthorized));
        assert!(!never_ran(&RunnerError::Lost("x".into())) && !never_ran(&RunnerError::Timeout(45)));
    }

    /// A stand-in `xcodebuild` (a shell script): prints what `script` says.
    fn fake_xcodebuild(dir: &std::path::Path, script: &str) -> RunnerSpec {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("xcodebuild");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        RunnerSpec {
            udid: "00008130-000A1B2C3D4E5F60".into(),
            xcodebuild: path,
            xctestrun: dir.join("x.xctestrun"),
            derived: dir.join("derived"),
            ledger: Some(Arc::new(Ledger::open(dir.join("children.json")).unwrap())),
            transport: Transport::Usbmux(Usbmux::at(&dir.join("no-usbmuxd"))),
        }
    }

    #[test]
    fn a_launch_that_fails_says_why_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let spec = fake_xcodebuild(dir.path(), "echo 'Testing started'; echo 'error: Developer Mode disabled' >&2; exit 65");
        let supervisor = RunnerSupervisor::new(spec.clone());
        assert_eq!(supervisor.start(), Err(ControlError::DeveloperModeOff));
        assert!(!supervisor.is_running());
        assert!(spec.ledger.unwrap().entries().unwrap().is_empty());
    }

    #[test]
    fn a_launch_gets_only_the_tokens_digest_in_its_environment_and_the_port_it_prints() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let spec = fake_xcodebuild(
            dir.path(),
            &format!("echo \"$@\" > {0}; [ ${{#TEST_RUNNER_OXIMUX_TOKEN_SHA256}} -eq 64 ] && [ -z \"$TEST_RUNNER_OXIMUX_TOKEN\" ] && echo ok >> {0}; echo 'OXIMUX_RUNNER_LISTENING port=50755'; exec sleep 30", seen.display()),
        );
        let ledger = spec.ledger.clone().unwrap();
        let supervisor = RunnerSupervisor::new(spec);
        supervisor.start().unwrap();
        assert!(supervisor.is_running());
        let seen = std::fs::read_to_string(&seen).unwrap();
        assert!(seen.starts_with("test-without-building -xctestrun ") && seen.contains("-destination id=00008130-000A1B2C3D4E5F60"), "{seen}");
        assert!(seen.ends_with("ok\n"), "no token: {seen}");
        assert_eq!(ledger.entries().unwrap().len(), 1);
        // The port is reached through usbmux; this Mac's test daemon is
        // missing, so the call fails — without relaunch (not a runner fault).
        assert!(matches!(supervisor.call("viewport", Value::Null), Err(ControlError::Runner(_))));
        supervisor.stop();
        assert!(!supervisor.is_running());
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn a_launch_that_never_listens_is_stopped_by_stop() {
        let dir = tempfile::tempdir().unwrap();
        let spec = fake_xcodebuild(dir.path(), "exec sleep 30");
        let supervisor = Arc::new(RunnerSupervisor::new(spec));
        let starting = {
            let supervisor = supervisor.clone();
            std::thread::spawn(move || supervisor.start())
        };
        std::thread::sleep(Duration::from_millis(300));
        supervisor.inner.stopping.store(true, Ordering::Relaxed);
        assert_eq!(starting.join().unwrap(), Err(ControlError::Cancelled));
    }

    #[test]
    fn an_idle_runner_is_shut_down() {
        let dir = tempfile::tempdir().unwrap();
        let spec = fake_xcodebuild(dir.path(), "echo 'OXIMUX_RUNNER_LISTENING port=50755'; exec sleep 30");
        let supervisor = RunnerSupervisor::with_idle_stop(spec, Duration::from_millis(500));
        supervisor.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while supervisor.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!supervisor.is_running());
    }

    /// A port nothing listens on.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A runner on this Mac's loopback that answers every command `ok`.
    fn answering_runner() -> u16 {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"ok":true,"data":{"tapped":true}}"#;
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        port
    }

    /// `xcodebuild` stand-in whose Nth launch listens on `ports[N]` (the
    /// last one after that), counting launches in `launches`.
    fn launching(dir: &std::path::Path, ports: &[u16]) -> RunnerSpec {
        let launches = dir.join("launches");
        let cases: String = ports.iter().enumerate().map(|(i, p)| format!("{}) p={p};; ", i + 1)).collect();
        let last = ports.last().unwrap();
        let script = format!(
            "echo x >> {0}; n=$(wc -l < {0} | tr -d ' '); case $n in {cases}*) p={last};; esac; echo \"OXIMUX_RUNNER_LISTENING port=$p\"; exec sleep 30",
            launches.display()
        );
        RunnerSpec { transport: Transport::Loopback, ..fake_xcodebuild(dir, &script) }
    }

    fn launches(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("launches")).map_or(0, |s| s.lines().count())
    }

    #[test]
    fn a_runner_that_stopped_listening_is_relaunched_once_and_the_tap_goes_through() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = RunnerSupervisor::new(launching(dir.path(), &[closed_port(), answering_runner()]));
        let reply = supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})).unwrap();
        assert_eq!(reply.data["tapped"], true);
        assert_eq!(launches(dir.path()), 2);
        supervisor.stop();
    }

    #[test]
    fn the_same_failure_again_within_the_window_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = RunnerSupervisor::new(launching(dir.path(), &[closed_port()]));
        // Refused, relaunched, refused again: no third launch for this call.
        let Err(ControlError::GaveUp(_)) = supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})) else { panic!() };
        assert_eq!(launches(dir.path()), 2);
        assert!(!supervisor.is_running());
        // The next command launches, and is refused without a relaunch.
        let Err(ControlError::GaveUp(_)) = supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})) else { panic!() };
        assert_eq!(launches(dir.path()), 3);
    }

    /// A runner that ends between commands (killed, crashed) is relaunched
    /// by the next one once; ending again within the window is given up on.
    #[test]
    fn a_runner_that_keeps_ending_between_commands_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let spec = launching(dir.path(), &[answering_runner()]);
        let ledger = spec.ledger.clone().unwrap();
        let supervisor = RunnerSupervisor::new(spec);
        let kill = |supervisor: &RunnerSupervisor| {
            let pid = ledger.entries().unwrap()[0].pid;
            child_ledger::kill_group(pid);
            let deadline = Instant::now() + Duration::from_secs(10);
            while supervisor.is_running() {
                assert!(Instant::now() < deadline, "the killed runner never ended");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})).unwrap();
        kill(&supervisor);
        supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})).unwrap();
        assert_eq!(launches(dir.path()), 2);
        kill(&supervisor);
        let Err(ControlError::GaveUp(_)) = supervisor.call("tap", serde_json::json!({"x": 1, "y": 2})) else { panic!() };
        assert_eq!(launches(dir.path()), 2, "no third launch within the window");
    }

    #[test]
    fn only_the_newest_result_bundles_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for stamp in ["2026.10.08_02-36-46", "2026.10.08_02-37-00", "2026.10.09_09-00-00", "2026.10.10_10-00-00"] {
            std::fs::create_dir_all(dir.path().join(format!("Test-OximuxRunner-{stamp}-+0700.xcresult"))).unwrap();
        }
        std::fs::write(dir.path().join("LogStoreManifest.plist"), b"").unwrap();
        prune_results(dir.path(), 2);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["LogStoreManifest.plist", "Test-OximuxRunner-2026.10.09_09-00-00-+0700.xcresult", "Test-OximuxRunner-2026.10.10_10-00-00-+0700.xcresult"]);
    }

    #[test]
    fn abort_ends_the_runner_without_waiting_for_the_command_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        // A runner that takes every request and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        // Like xcodebuild, the stand-in ignores SIGTERM: only a SIGKILL ends
        // it before `end_process`'s 5 s escalation (which a quit never sees).
        let script = format!("trap '' TERM; echo \"OXIMUX_RUNNER_LISTENING port={port}\"; exec sleep 30");
        let supervisor = Arc::new(RunnerSupervisor::new(RunnerSpec { transport: Transport::Loopback, ..fake_xcodebuild(dir.path(), &script) }));
        supervisor.start().unwrap();
        let busy = {
            let supervisor = supervisor.clone();
            std::thread::spawn(move || supervisor.call("type", serde_json::json!({"text": "x".repeat(3000)})))
        };
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        supervisor.abort();
        assert!(started.elapsed() < Duration::from_millis(500), "abort waited {:?}", started.elapsed());
        assert!(!supervisor.is_running());
        // The xcodebuild stand-in is gone well before a SIGTERM's grace.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !supervisor.inner.spec.ledger.as_ref().unwrap().entries().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(supervisor.inner.spec.ledger.as_ref().unwrap().entries().unwrap().is_empty(), "still running: SIGTERM only?");
        drop(busy);
    }

    #[test]
    fn a_command_cut_by_an_abort_launches_nothing_again() {
        let dir = tempfile::tempdir().unwrap();
        // A runner that holds every request until it is gone, then drops
        // each connection (the client's one resend included).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = Arc::new(Mutex::new(Vec::new()));
        let gone = Arc::new(AtomicBool::new(false));
        {
            let (held, gone) = (held.clone(), gone.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    if !gone.load(Ordering::Relaxed) {
                        lock(&held).push(stream);
                    }
                }
            });
        }
        let supervisor = Arc::new(RunnerSupervisor::new(launching(dir.path(), &[port])));
        supervisor.start().unwrap();
        let call = {
            let supervisor = supervisor.clone();
            std::thread::spawn(move || supervisor.call("type", serde_json::json!({"text": "x"})))
        };
        std::thread::sleep(Duration::from_millis(300));
        supervisor.abort();
        // The connection drops, as the killed runner's would.
        gone.store(true, Ordering::Relaxed);
        lock(&held).clear();
        assert_eq!(call.join().unwrap(), Err(ControlError::Cancelled));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(launches(dir.path()), 1, "relaunched after the abort");
        assert!(!supervisor.is_running());
    }

    #[test]
    fn a_start_given_up_by_its_caller_ends_its_launch() {
        let dir = tempfile::tempdir().unwrap();
        // Ignoring SIGTERM, as xcodebuild does: a cancel must not wait out
        // the 5 s escalation (on quit, nothing would be left to do it).
        let spec = fake_xcodebuild(dir.path(), "trap '' TERM; exec sleep 30");
        let ledger = spec.ledger.clone().unwrap();
        let supervisor = RunnerSupervisor::new(spec);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        assert_eq!(supervisor.start_unless(&cancel), Err(ControlError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2), "the cancel waited {:?}", started.elapsed());
        assert!(ledger.entries().unwrap().is_empty());
    }
}
