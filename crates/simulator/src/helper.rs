//! Spawning `oximux-sim-helper` and completing its handshake.
//!
//! The helper is a private-framework process, so it gets as little of our
//! world as possible:
//! - **A cleared environment** plus an allowlist ([`ENV_ALLOWLIST`]). It
//!   needs `HOME`/`PATH`/`TMPDIR` for `xcrun`, and `DEVELOPER_DIR` when the
//!   user selects Xcode that way — nothing else of ours (tokens, agent
//!   session markers) has any business in it.
//! - **stdin/stdout pipes only.** Rust creates both pipe ends close-on-exec,
//!   so no other child we spawn later inherits the helper's stdin write end;
//!   that is what makes "exits on stdin EOF" a real parent-death guarantee.
//! - **stderr to a log file** (rotated at 1 MB), never to our stderr.
//!
//! [`Handshake::run`] then reads messages until `hello` (protocol check) and
//! `ready`, or a `fatal`, each bounded by a timeout.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use oximux_no_window::NoWindow;

use crate::protocol::{self, Event, FatalReason, MIN_PROTOCOL_VERSION, Outbound, PROTOCOL_VERSION, StreamFormat};
use crate::{DeviceId, Orientation, Result, SimError};

/// The only variables the helper inherits.
pub const ENV_ALLOWLIST: &[&str] = &["HOME", "PATH", "TMPDIR", "LANG", "USER", "LOGNAME", "DEVELOPER_DIR"];

/// How long the helper may take to say `hello`, and then `ready`.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Log files rotate past this size (one previous generation is kept).
const LOG_ROTATE_BYTES: u64 = 1 << 20;

/// Stream settings at spawn; all can change later via `configure`.
#[derive(Clone, Debug)]
pub struct HelperOptions {
    /// Output scale, 0 < s ≤ 1 (0.5 = "Half", the default).
    pub scale: f64,
    pub fps: f64,
    pub quality: f64,
    pub orientation: Orientation,
    /// Asked of every helper; a protocol-1 helper ignores the flag and
    /// streams JPEG.
    pub format: StreamFormat,
    /// Where the helper's stderr goes; `None` discards it.
    pub log_path: Option<PathBuf>,
    /// Record the helper here while it runs, so the next launch can reap it
    /// if this process dies without cleaning up (see `child_ledger`).
    pub ledger: Option<std::sync::Arc<crate::child_ledger::Ledger>>,
}

impl Default for HelperOptions {
    fn default() -> Self {
        Self {
            scale: 0.5,
            fps: 30.0,
            quality: 0.7,
            orientation: Orientation::Portrait,
            format: StreamFormat::Jpeg,
            log_path: None,
            ledger: None,
        }
    }
}

impl HelperOptions {
    /// The streaming invocation for `udid`.
    pub fn args(&self, udid: &DeviceId) -> Vec<String> {
        vec![
            "--udid".into(),
            udid.0.clone(),
            "--scale".into(),
            self.scale.to_string(),
            "--fps".into(),
            self.fps.to_string(),
            "--quality".into(),
            self.quality.to_string(),
            "--orientation".into(),
            self.orientation.as_u8().to_string(),
            "--format".into(),
            self.format.as_str().into(),
        ]
    }
}

/// A freshly spawned helper, before its streams are handed to a session.
pub struct Spawned {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// Spawn `path args…` with the allowlisted environment and piped stdio.
pub fn spawn(path: &Path, args: &[String], log_path: Option<&Path>) -> Result<Spawned> {
    let stderr = match log_path {
        Some(log) => Stdio::from(open_log(log)?),
        None => Stdio::null(),
    };
    let mut cmd = Command::new(path);
    cmd.args(args).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr).no_window();
    for key in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| SimError::HelperNotFound(format!("{}: {e}", path.display())))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(SimError::HelperFailed("helper spawned without stdio pipes".into()));
    };
    Ok(Spawned { child, stdin, stdout })
}

/// Open the helper's stderr log for appending, rotating it first when it has
/// grown past [`LOG_ROTATE_BYTES`].
fn open_log(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

/// What a completed handshake learned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The protocol version the helper speaks (within ours).
    pub proto: u32,
    pub version: String,
    pub xcode: Option<String>,
}

/// Drives the opening of a helper's output: `hello` first, then (for a
/// streaming helper) `ready`, with a fatal or silent helper turned into the
/// matching [`SimError`].
pub struct Handshake;

impl Handshake {
    /// Consume messages from `rx` until the handshake completes. Messages
    /// other than the ones the handshake needs are returned in order, so the
    /// caller can replay them (a streaming helper sends nothing else before
    /// `ready`, but a conformance helper does).
    pub fn run(
        rx: &mpsc::Receiver<Result<Outbound>>,
        wait_for_ready: bool,
        timeout: Duration,
    ) -> Result<(Hello, Vec<Outbound>)> {
        let hello = match Self::next(rx, timeout, "hello")? {
            Outbound::Event(Event::Hello { proto, version, xcode }) => {
                if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&proto) {
                    return Err(SimError::HelperIncompatible { expected: PROTOCOL_VERSION, got: proto });
                }
                Hello { proto, version, xcode }
            }
            other => return Err(SimError::Protocol(format!("expected hello first, got {other:?}"))),
        };
        let mut rest = Vec::new();
        if !wait_for_ready {
            return Ok((hello, rest));
        }
        loop {
            match Self::next(rx, timeout, "ready")? {
                Outbound::Event(Event::Ready { .. }) => return Ok((hello, rest)),
                Outbound::Event(Event::Fatal { reason, message }) => return Err(fatal_error(reason, message)),
                other => rest.push(other),
            }
        }
    }

    fn next(rx: &mpsc::Receiver<Result<Outbound>>, timeout: Duration, what: &str) -> Result<Outbound> {
        match rx.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(SimError::Timeout {
                what: format!("simulator helper `{what}`"),
                secs: timeout.as_secs(),
            }),
            // The reader saw EOF before the handshake finished.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(SimError::HelperExited { code: None }),
        }
    }
}

/// The user-facing error for a helper `fatal` event.
pub fn fatal_error(reason: FatalReason, message: String) -> SimError {
    match reason {
        FatalReason::FrameworkLoadFailed => SimError::FrameworkLoadFailed(message),
        FatalReason::DeviceNotFound => SimError::DeviceNotFound(message),
        FatalReason::DeviceNotBooted => SimError::DeviceNotBooted,
        FatalReason::BadArgs | FatalReason::CaptureFailed | FatalReason::Other(_) => SimError::HelperFailed(message),
    }
}

/// Read messages from the helper's stdout on the current thread, forwarding
/// each to `tx` until EOF, a protocol error, or the receiver going away.
pub fn pump(mut stdout: ChildStdout, tx: mpsc::Sender<Result<Outbound>>) {
    loop {
        let message = protocol::read_message(&mut stdout);
        let stop = !matches!(message, Ok(Some(_)));
        let forwarded = match message {
            Ok(Some(m)) => tx.send(Ok(m)),
            Ok(None) => break,
            Err(e) => tx.send(Err(e)),
        };
        if stop || forwarded.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Frame;

    fn hello(proto: u32) -> Outbound {
        Outbound::Event(Event::Hello { proto, version: "0.2.0".into(), xcode: None })
    }

    fn feed(messages: Vec<Result<Outbound>>) -> mpsc::Receiver<Result<Outbound>> {
        let (tx, rx) = mpsc::channel();
        for m in messages {
            tx.send(m).unwrap();
        }
        rx
    }

    #[test]
    fn handshake_needs_hello_then_ready() {
        let rx = feed(vec![
            Ok(hello(PROTOCOL_VERSION)),
            Ok(Outbound::Event(Event::Ready { udid: "U".into(), pid: 1, orientation: None })),
        ]);
        let (h, rest) = Handshake::run(&rx, true, Duration::from_millis(50)).unwrap();
        assert_eq!(h.version, "0.2.0");
        assert_eq!(h.proto, PROTOCOL_VERSION);
        assert!(rest.is_empty());
    }

    #[test]
    fn handshake_accepts_a_protocol_1_helper() {
        let rx = feed(vec![
            Ok(hello(MIN_PROTOCOL_VERSION)),
            Ok(Outbound::Event(Event::Ready { udid: "U".into(), pid: 1, orientation: None })),
        ]);
        assert_eq!(Handshake::run(&rx, true, Duration::from_millis(50)).unwrap().0.proto, 1);
        let rx = feed(vec![Ok(hello(0))]);
        assert!(matches!(Handshake::run(&rx, true, Duration::from_millis(50)), Err(SimError::HelperIncompatible { got: 0, .. })));
    }

    #[test]
    fn handshake_refuses_another_protocol_version() {
        let rx = feed(vec![Ok(hello(PROTOCOL_VERSION + 1))]);
        let err = Handshake::run(&rx, true, Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, SimError::HelperIncompatible { got, .. } if got == PROTOCOL_VERSION + 1));
    }

    #[test]
    fn handshake_requires_hello_first() {
        let rx = feed(vec![Ok(Outbound::Frame(Frame { width: 1, height: 1, jpeg: vec![] }))]);
        assert!(matches!(Handshake::run(&rx, true, Duration::from_millis(50)), Err(SimError::Protocol(_))));
    }

    #[test]
    fn handshake_maps_fatal_reasons() {
        let rx = feed(vec![
            Ok(hello(PROTOCOL_VERSION)),
            Ok(Outbound::Event(Event::Fatal { reason: FatalReason::FrameworkLoadFailed, message: "m".into() })),
        ]);
        assert!(matches!(Handshake::run(&rx, true, Duration::from_millis(50)), Err(SimError::FrameworkLoadFailed(_))));
    }

    #[test]
    fn handshake_times_out_and_reports_early_exit() {
        let (_tx, rx) = mpsc::channel::<Result<Outbound>>();
        assert!(matches!(Handshake::run(&rx, true, Duration::from_millis(20)), Err(SimError::Timeout { .. })));
        let rx = feed(vec![Ok(hello(PROTOCOL_VERSION))]); // sender dropped after hello
        assert!(matches!(Handshake::run(&rx, true, Duration::from_millis(50)), Err(SimError::HelperExited { .. })));
    }

    #[test]
    fn args_carry_every_option() {
        let opts = HelperOptions { orientation: Orientation::LandscapeLeft, format: StreamFormat::Avcc, ..Default::default() };
        let args = opts.args(&DeviceId("U".into()));
        assert_eq!(
            args,
            ["--udid", "U", "--scale", "0.5", "--fps", "30", "--quality", "0.7", "--orientation", "3", "--format", "avcc"]
        );
    }

    #[test]
    fn log_rotates_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("helper.log");
        std::fs::write(&log, vec![b'x'; (LOG_ROTATE_BYTES + 1) as usize]).unwrap();
        drop(open_log(&log).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        assert!(dir.path().join("helper.log.1").exists());
    }
}
