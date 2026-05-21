use serde::{Deserialize, Serialize};

use crate::error::ErrCode;

// Bumped whenever the wire schema changes in a non-additive way. The
// socket path also embeds a version (`relay-v<N>.sock`) so mismatched
// major builds can't even reach the handshake; this constant is the
// failsafe for the case where an old client somehow finds a newer
// daemon's socket (dev environments).
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub token: String,
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub server_protocol_version: u32,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyDescriptor {
    pub pty_id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

// Phase-07: per-PTY counters surfaced by `Request::Stats`. Currently
// observational only — no UI consumer yet (planned for the relay pane
// in the app's About dialog).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyStats {
    pub pty_id: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub alive_secs: u64,
}

// Hello + HelloAck travel inside Request/Response frames (not as a
// separate non-framed prelude) so the codec stays uniform — one read
// loop on both sides, one parse path. The daemon enforces that the
// FIRST request from a given client must be `Request::Hello`; later
// `Hello` frames are rejected with `ErrCode::AuthFailed`. That state
// machine lives in the daemon (phase 02), not in this protocol crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello(Hello),
    Spawn {
        cwd: String,
        cols: u16,
        rows: u16,
        shell: Option<String>,
        env: Vec<(String, String)>,
    },
    Attach {
        pty_id: String,
    },
    Write {
        pty_id: String,
        bytes: Vec<u8>,
    },
    Resize {
        pty_id: String,
        cols: u16,
        rows: u16,
    },
    Close {
        pty_id: String,
        grace_ms: u32,
    },
    ListPtys,
    Stats,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    HelloAck(HelloAck),
    SpawnOk { pty_id: String },
    AttachOk { replay: Vec<u8> },
    Ok,
    Pty(PtyDescriptor),
    PtyList(Vec<PtyDescriptor>),
    StatsOk(Vec<PtyStats>),
    Err { code: ErrCode, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notification {
    Output { pty_id: String, bytes: Vec<u8> },
    Exit { pty_id: String, code: Option<i32> },
}
