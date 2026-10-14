//! `sim rn network|profile` (v32): read-only looks into a React Native app
//! through its Metro dev server's debugger (React Native 0.83 or later).
//!
//! Re-exported from [`crate::simulator`]. Every type is new in v32, so its
//! shape is fixed from its first release (postcard: structs by field
//! position, enums by ordinal). Grow by minting a new type or appending a
//! variant at the end, never by adding a field.
//!
//! The host picks the app (the one running on the worktree's attached
//! device) and redacts what it collected before replying: secret headers,
//! every URL query value and known secret JSON keys come back as
//! `[redacted]`.

use serde::{Deserialize, Serialize};

/// What to look at, for how long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimRnCmdWire {
    /// The app's HTTP requests for `seconds` (at most 60).
    Network {
        seconds: u32,
        /// Only requests whose URL contains this.
        grep: Option<String>,
        /// Also headers and text bodies (each at most 4 KiB).
        bodies: bool,
        /// The app (its bundle id / package) when several are connected.
        app: Option<String>,
    },
    /// Where the app's JavaScript spent `seconds` (at most 60): the `top`
    /// functions by self time (at most 100).
    Profile { seconds: u32, top: u32, app: Option<String> },
}

/// What a look found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimRnWire {
    Network(SimRnNetworkWire),
    Profile(SimRnProfileWire),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRnNetworkWire {
    /// The app looked at.
    pub app: String,
    /// In the order they started.
    pub requests: Vec<SimRnRequestWire>,
    /// More requests (or bytes) than fit: the rest were dropped.
    pub truncated: bool,
    /// What the caller should know (an open React Native DevTools was
    /// disconnected, …).
    pub warnings: Vec<String>,
}

/// One request; what it did not get to (a reply, an end) is `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRnRequestWire {
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    /// From start to the end of its body (or its failure).
    pub ms: Option<u32>,
    /// Bytes received.
    pub size: Option<u64>,
    pub mime: Option<String>,
    /// Why it failed (or "canceled").
    pub failed: Option<String>,
    /// With `bodies` only; empty otherwise.
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRnProfileWire {
    pub app: String,
    /// How long the profile ran.
    pub duration_ms: f64,
    /// The time its samples cover (the sum of every function's self time).
    pub sampled_ms: f64,
    /// Most self time first.
    pub functions: Vec<SimRnFunctionWire>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRnFunctionWire {
    /// `(anonymous)` when it has none.
    pub name: String,
    /// Its script's URL (or file); empty for the runtime's own entries
    /// (`(garbage collector)`, `(idle)`).
    pub url: String,
    /// 1-based.
    pub line: u32,
    pub self_ms: f64,
}
