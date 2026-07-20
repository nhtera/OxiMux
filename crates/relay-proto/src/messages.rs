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
//
// v5: `Request::Spawn` carries `args` (the program's argv) so an agent
// launch can pass its flags directly — the daemon runs `program args…` as
// the PTY leaf, with no login-shell `exec` wrapper echoing a command line.
// Bincode isn't self-describing, so the added field is a wire break: the
// socket name bumps to `relay-v5.sock`, a fresh client spawns a fresh
// daemon, and any stale v4 daemon idles out on its own socket.
//
// v6: `Request::AgentStatus` lets a child process (an agent CLI hook,
// invoked via `oximux agent-status`) report structured status. The daemon
// frames the opaque payload as an OSC-9999 sequence and fans it out on the
// PTY's existing output channel, so the app's status scanner decodes it
// with no new app-side plumbing — and an agent hook can report status
// without a controlling terminal (hooks run detached, with no `/dev/tty`).
// New variant ⇒ wire break ⇒ socket bumps to `relay-v6.sock`.
//
// v7: `Notification::Gapped` tells a subscriber the daemon discarded output
// for it, so it knows to re-attach and resync from the replay ring. The
// version bump is load-bearing rather than ceremonial: the handshake compares
// versions for EQUALITY, so without it a v7 daemon and a v6 client would
// connect happily and then break the moment a gap occurred — the v6 client
// cannot decode variant 3, and postcard enums are positional. Bumping turns
// that into a clean refusal at connect. Socket bumps to `relay-v7.sock`.
//
// Also v7 (same unreleased break): `Request::Replay` / `Response::ReplayOk`
// re-fetch the ring WITHOUT minting an attachment, so recovering from a gap
// does not add a second attachment whose stale size would keep voting in the
// daemon's smallest-screen-wins `min`.
pub const PROTOCOL_VERSION: u32 = 7;

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
        // Argv for the spawned program (excluding argv[0]). Empty for a
        // plain shell. When set, the daemon runs `shell args…` directly so
        // an agent's flags reach it without a wrapper command line.
        args: Vec<String>,
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
    /// Structured agent status for a PTY — sent by the `oximux agent-status`
    /// CLI, which an agent's hooks invoke (e.g. Claude Code `PreToolUse` /
    /// `Stop`). `payload` is an opaque JSON object string (e.g.
    /// `{"v":1,"state":"working","tool":"Bash"}`); the daemon does NOT parse
    /// it — it wraps it as `ESC]9999;<payload>BEL` and fans it out on the
    /// PTY's output channel, where the app's OSC scanner decodes it. This is
    /// how a hook reports status despite running with no controlling terminal.
    /// Appended last to keep existing bincode variant indices stable.
    AgentStatus {
        pty_id: String,
        payload: String,
    },
    /// Re-fetch a PTY's replay ring WITHOUT registering an attachment — the
    /// recovery path for a subscriber that received `Notification::Gapped`.
    ///
    /// Distinct from `Attach` on purpose. A re-`Attach` mints a second
    /// `attachment_id` for a client that already holds one, and the daemon
    /// drives the PTY at the element-wise `min` across attachments, so the
    /// stale entry would keep voting on the size until it was released —
    /// visibly resizing the live process to recover from a dropped frame.
    /// This asks only the question the caller actually has ("what is on the
    /// screen now?") and leaves attachment bookkeeping alone.
    /// Appended last to keep existing variant indices stable.
    Replay {
        pty_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    HelloAck(HelloAck),
    // `attachment_id` identifies the spawning session's auto-attachment
    // (Spawn auto-attaches the caller). The client stores it so its
    // `Resize`/`Detach` for this PTY address the right attachment.
    SpawnOk {
        pty_id: String,
        attachment_id: u64,
    },
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
    /// Answer to `Request::Replay`: the ring snapshot plus the dims it was
    /// drawn at. The caller MUST rebuild its emulator at these dims before
    /// replaying, for the same reason `AttachOk` carries them — absolute-
    /// position sequences only land correctly in a grid of the right size.
    ReplayOk {
        replay: Vec<u8>,
        cols: u16,
        rows: u16,
    },
    Err {
        code: ErrCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notification {
    Output {
        pty_id: String,
        bytes: Vec<u8>,
    },
    Exit {
        pty_id: String,
        code: Option<i32>,
    },
    /// Explicit attention raised via `Request::Notify`. The client maps
    /// this to a pane attention signal (ring + tab dot). `title`/`body`
    /// are carried for a future OS-banner surface; today the client only
    /// uses it to flag attention.
    Attention {
        pty_id: String,
        title: String,
        body: String,
    },
    /// This subscriber fell behind and the daemon discarded output for it.
    ///
    /// The bytes are not lost — the session's replay ring still holds them —
    /// but they will never arrive on the live stream, so a client that keeps
    /// rendering from here on is drawing a terminal with a hole in it. The
    /// recovery is to re-`Attach`, which replays the ring from scratch.
    ///
    /// Sent once per gap rather than per dropped message: a subscriber that is
    /// behind is behind, and repeating the signal would compete for the very
    /// queue space that is already exhausted.
    Gapped {
        pty_id: String,
    },
}
