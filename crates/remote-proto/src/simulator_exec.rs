//! `sim exec` (v31): a raw `simctl` or `adb` command on the attached device,
//! for what the dedicated verbs do not cover.
//!
//! Re-exported from [`crate::simulator`]. Every type is new in v31, so its
//! shape is fixed from its first release (postcard: structs by field
//! position, enums by ordinal). Grow by minting a new type or appending a
//! variant at the end, never by adding a field.
//!
//! The host decides everything: whether exec is on at all (off unless the
//! user turned it on), which subcommands may run, and which device. The
//! caller never names the device: the host adds the attached one's id.

use serde::{Deserialize, Serialize};

/// `sim exec -- <args…>`: the arguments after `simctl` (without the device)
/// or after `adb` (without `-s`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimExecWire {
    pub argv: Vec<String>,
}

/// What the command printed, cut to 64 KiB together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimExecOutputWire {
    /// Its exit code; `None` when it was stopped (at the size cap or the
    /// time limit, which `stderr` then says).
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The output reached the cap: the rest was not read.
    pub truncated: bool,
}
