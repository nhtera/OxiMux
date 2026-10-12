//! Diagnostics verbs' wire types (v29): what `sim logs` and `sim crashes`
//! send inside [`SimCmdWire::Logs`](crate::simulator::SimCmdWire::Logs) /
//! [`SimCmdWire::Crashes`](crate::simulator::SimCmdWire::Crashes), and what
//! comes back.
//!
//! Re-exported from [`crate::simulator`]. Every type is new in v29, so its
//! shape is fixed from its first release (postcard: structs by field
//! position, enums by ordinal). Grow by minting a new type or appending a
//! variant at the end, never by adding a field.

use serde::{Deserialize, Serialize};

/// `sim logs`: one bounded read, never a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimLogsWire {
    /// How far back, in seconds (at most an hour).
    pub since_secs: u32,
    /// One app's messages (a bundle id or package name). Required on a real
    /// Android phone.
    pub app: Option<String>,
    /// Messages containing this, ignoring case.
    pub grep: Option<String>,
    pub level: SimLogLevelWire,
    /// The last this many lines (at most 2000; the reply is also cut to
    /// 64 KiB, newest kept).
    pub lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimLogLevelWire {
    /// What the platform shows by default.
    Default,
    /// Errors and faults only.
    Error,
    Info,
    Debug,
}

/// `sim crashes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimCrashesWire {
    /// How far back, in seconds (at most an hour).
    pub since_secs: u32,
    pub app: Option<String>,
}

/// One crash, summarised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimCrashWire {
    /// Tells one crash from another across reads.
    pub id: String,
    /// When, as the device wrote it.
    pub time: String,
    pub app: String,
    pub exception: String,
    pub reason: String,
    /// The top of the crashing thread's stack, innermost first.
    pub frames: Vec<String>,
}

/// Reply to [`SimCmdWire::Crashes`](crate::simulator::SimCmdWire::Crashes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimCrashListWire {
    pub crashes: Vec<SimCrashWire>,
    /// Lines that warn of trouble short of a crash (a hang, an unhandled
    /// rejection), newest last.
    pub warnings: Vec<String>,
}
