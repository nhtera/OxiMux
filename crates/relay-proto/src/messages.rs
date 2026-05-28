use serde::{Deserialize, Serialize};

use crate::error::ErrCode;

// Bumped whenever the wire schema changes in a non-additive way. The
// socket path also embeds a version (`relay-v<N>.sock`) so mismatched
// major builds can't even reach the handshake; this constant is the
// failsafe for the case where an old client somehow finds a newer
// daemon's socket (dev environments).
//
// v4: multi-client attach — `attachment_id` added to `AttachOk`/`SpawnOk`
// (so each attachment is individually addressable) and to `Resize` (the
// daemon recomputes the effective PTY size as the element-wise `min`
// across attachments, "smallest screen wins"); `Request::Detach` lets one
// attachment drop out without killing the PTY.
pub const PROTOCOL_VERSION: u32 = 4;

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
        // Which attachment is requesting this size. The daemon stores it
        // per-attachment and drives the PTY at the element-wise `min`
        // across all live attachments ("smallest screen wins"). Obtained
        // from `AttachOk`/`SpawnOk`.
        attachment_id: u64,
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
    /// Explicit attention request for a PTY — sent by the `oximux notify`
    /// CLI (which agent hooks / scripts invoke). The daemon fans out a
    /// `Notification::Attention` to that PTY's subscribers so the owning
    /// pane raises its attention signal. Appended last to keep existing
    /// bincode variant indices stable.
    Notify {
        pty_id: String,
        title: String,
        body: String,
    },
    /// Drop one attachment from a PTY WITHOUT killing it. The daemon
    /// removes the attachment's size from the `min` computation (so the
    /// PTY can grow back to the remaining attachments) and stops fanning
    /// notifications to it. The PTY stays alive for any other
    /// attachments; with none left it retains its last size and is reaped
    /// only by the idle GC. Distinct from `Close`, which kills the PTY.
    /// Appended last to keep existing bincode variant indices stable.
    Detach {
        pty_id: String,
        attachment_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    HelloAck(HelloAck),
    // `attachment_id` identifies the spawning session's auto-attachment
    // (Spawn auto-attaches the caller). The client stores it so its
    // `Resize`/`Detach` for this PTY address the right attachment.
    SpawnOk { pty_id: String, attachment_id: u64 },
    // `cols`/`rows` are the PTY's current grid dimensions on the daemon.
    // The client MUST build its local emulator at exactly these dims
    // before replaying `replay`, otherwise raw bytes captured for a
    // wide grid land in the wrong cells (absolute-position CSI clipping)
    // and a later resize reflows them into scrambled output. With the
    // dims, replay reconstructs the screen byte-for-byte, and the live
    // process repaints on the next resize via SIGWINCH.
    AttachOk {
        replay: Vec<u8>,
        cols: u16,
        rows: u16,
        // Per-attachment handle minted by the daemon. The client stores
        // it and sends it back on `Resize`/`Detach` so the daemon knows
        // which attachment's requested size to update.
        attachment_id: u64,
    },
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
    /// Explicit attention raised via `Request::Notify`. The client maps
    /// this to a pane attention signal (ring + tab dot). `title`/`body`
    /// are carried for a future OS-banner surface; today the client only
    /// uses it to flag attention.
    Attention {
        pty_id: String,
        title: String,
        body: String,
    },
}
