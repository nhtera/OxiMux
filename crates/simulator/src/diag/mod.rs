//! Device logs and crash reports for `oximux sim logs` / `crashes` and the
//! panel's crash row: bounded, one-shot reads, never a stream.
//!
//! - [`markers`]: the lines that mean an app crashed or nearly did.
//! - [`logs`]: a simulator's `log show` and an Android device's `logcat`,
//!   cut to the last lines and bytes asked for.
//! - [`crashes`]: a simulator's `.ips` reports and Android's crash buffer, as
//!   one summary per crash.
//!
//! Everything a device or the user types reaches a command as one argument
//! (`log show`'s predicate is built here, escaped; `adb logcat` quotes each
//! word for the device's shell), never through a shell of ours.

pub mod crash_buffer;
pub mod crashes;
pub mod logs;
pub mod markers;

/// The furthest back either verb reads.
pub const MAX_SINCE_SECS: u32 = 3600;

/// One crash, summarised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crash {
    /// Tells one crash from another across reads (an `.ips` incident id, or
    /// Android's pid, time and first frames).
    pub id: String,
    /// When it happened, as the device wrote it.
    pub time: String,
    /// The app (bundle id or package), else the process name.
    pub app: String,
    /// `EXC_BAD_ACCESS (SIGSEGV)`, `java.lang.IllegalStateException`, …
    pub exception: String,
    /// Why it ended: the exception's message, a fatal error's words, the
    /// signal.
    pub reason: String,
    /// The top of the crashing thread's stack, innermost first.
    pub frames: Vec<String>,
    /// The process, when known (a log line names it).
    pub pid: Option<u32>,
}

/// How many frames a summary keeps.
pub const FRAMES: usize = 8;

/// Why a read did not happen.
#[derive(Debug, thiserror::Error)]
pub enum DiagError {
    /// The request itself is unusable (a malformed app id, a window too long).
    #[error("{0}")]
    BadInput(String),
    /// The app is not on the device.
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Sim(#[from] crate::SimError),
}

pub type DiagResult<T> = std::result::Result<T, DiagError>;
