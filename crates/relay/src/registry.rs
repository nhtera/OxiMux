use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use dashmap::DashMap;
use oximux_relay_proto::{
    ErrCode, Notification, PtyDescriptor, PtyStats, UNROUTED_ATTACHMENT,
};
use oximux_shell_env::{clear_inherited_colour_suppression, seed_utf8_locale};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

use crate::checkpoint::CheckpointStore;
use crate::program_status_query::ProgramStatusQueryDetector;
use crate::pty_writer::PtyWriter;
use crate::ring_buffer::RingBuffer;

// Bounded per-subscriber channel. The session writer task drains it;
// if it falls behind, fan_out drops notifications rather than growing
// memory unboundedly. The replay buffer is the source of truth — a slow
// client gets the missed bytes on its next Attach, and the live stream
// gaps until it catches up. The client is TOLD it gapped
// (`Notification::Gapped`) so that next Attach actually happens.
pub const SUBSCRIBER_QUEUE: usize = 1024;

/// One attached client's outbound queue, plus whether it has an unannounced
/// gap. The flag lives beside the sender rather than inside the channel
/// because a full queue — the only way a gap occurs — has no room to carry
/// the notice at the moment it is needed.
struct Subscriber {
    /// The attachment this sender belongs to, so `detach` can drop it by name.
    ///
    /// Without it the only thing that ever removed a subscriber was `fan_out`
    /// noticing a closed receiver — which requires the PTY to produce output.
    /// A client that leaves while its shell sits quietly at a prompt would
    /// otherwise leave its sender in here indefinitely.
    attachment_id: u64,
    tx: Sender<Notification>,
    gapped: bool,
}

// 1 MiB per PTY — the plan's "Locked decisions" section, larger than
// the ~100 KiB common in lighter multiplexers because TUIs (`cc`, `vim`,
// full-screen pagers) can repaint dense screens that need more headroom
// for byte-for-byte replay to look correct.
pub const REPLAY_BUFFER_BYTES: usize = 1024 * 1024;

const READ_CHUNK_BYTES: usize = 8 * 1024;

// After an effective PTY resize, re-assert the same size a couple of
// times shortly after (a resize-confirm nudge). Some TUIs miss the first
// SIGWINCH if it lands mid-redraw; a cheap re-send nudges them to
// repaint at the new geometry. Each resend is superseded by a newer
// resize via `Entry::resize_seq`, so a stale resend can't clobber a
// fresher size.
const RESIZE_RESEND_DELAYS_MS: [u64; 2] = [40, 120];

// Error returned by registry operations. Keep small and convert into
// `oximux_relay_proto::ErrCode` in the server layer.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("pty not found: {0}")]
    NotFound(String),
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

impl RegistryError {
    pub fn err_code(&self) -> ErrCode {
        match self {
            RegistryError::NotFound(_) => ErrCode::PtyNotFound,
            RegistryError::Internal(_) => ErrCode::Internal,
        }
    }
}

pub struct SpawnArgs {
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub shell: Option<String>,
    /// Argv for the spawned program (excluding argv[0]). Empty for a plain
    /// shell; set when an agent launch passes its flags directly.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// History to seed the replay ring with before the child's output (see
    /// `Request::Spawn::prefill`).
    pub prefill: Vec<u8>,
}

struct Entry {
    pty_id: String,
    cwd: PathBuf,
    // EFFECTIVE grid size currently applied to the master fd — the
    // element-wise `min` across `attachments`. `attach`/`list` return
    // these so a new attacher builds its emulator at the live size.
    cols: Mutex<u16>,
    rows: Mutex<u16>,
    // Per-attachment requested sizes. The effective PTY size is the
    // element-wise `min` over these ("smallest screen wins"): the PTY
    // can't be wider/taller than the smallest viewer or that viewer
    // would clip. Empty after every attachment detaches — the PTY then
    // retains its last effective size until a new attach arrives.
    attachments: Mutex<HashMap<u64, (u16, u16)>>,
    // Bumped on every EFFECTIVE resize. A deferred "resend confirm" task
    // captures the value at arm time and only re-applies its size while
    // the seq is unchanged, so a newer resize supersedes a stale resend.
    resize_seq: AtomicU64,
    // Kept alive so we can call `resize()` on the master fd. The
    // reader has its own cloned fd. `Arc` so the resend-confirm task can
    // share the handle without holding the registry entry.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    // Input goes through the writer thread so the reader never blocks on it.
    writer: PtyWriter,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    ring: Arc<Mutex<RingBuffer>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    // Flipped by `reader_loop` immediately before the Exit fan-out.
    // The `close` grace poll watches this to skip the full SIGKILL
    // wait when the child has already reaped naturally.
    child_exited: Arc<AtomicBool>,
    // The child's exit status, stored by `reader_loop` just before it sets
    // `child_exited`. `EXIT_CODE_NONE` (a signal/detach, no status) or a real
    // 0..=255. Read only when `child_exited` is set, so its pre-exit value is
    // never observed. Lets `attach` replay an `Exit` to a client that
    // reconnects AFTER the child already died (the daemon outlives the app, so
    // a re-launched app would otherwise adopt a dead session as a frozen pane).
    exit_code: Arc<AtomicI32>,
    // Best-effort PID of the spawned child for the SIGTERM step. None
    // on non-Unix or when portable-pty returns None.
    pid: Option<u32>,
    // Windows stand-in for the process group: holds the shell and everything it
    // spawned, so `close` can end the tree rather than just the shell. Kept
    // alive for the entry's lifetime — the job's kill-on-close limit means
    // dropping this reaps the tree, which is what makes a daemon crash clean up
    // after itself.
    #[cfg(windows)]
    job: Option<Arc<oximux_job_object::JobObject>>,
    // Phase-07: per-PTY counters surfaced by `Request::Stats`.
    bytes_in: AtomicU64,
    bytes_out: Arc<AtomicU64>,
    started_at: Instant,
    // `bytes_out` value at the last disk checkpoint. The checkpoint tick
    // compares against the live counter and skips PTYs with no new
    // output, so idle shells cost zero disk writes.
    checkpointed_bytes_out: AtomicU64,
}

pub struct PtyRegistry {
    entries: DashMap<String, Arc<Entry>>,
    // Sessions a close has taken out of `entries` but not yet ended (see
    // `begin_close`). Not listed, but still alive: a restart must end them,
    // and the daemon is not idle while any remain.
    closing: Arc<DashMap<String, Arc<Entry>>>,
    // Process-wide monotonic source of attachment ids. Unique across all
    // PTYs (simpler than a per-entry counter; the id space is u64).
    next_attachment_id: AtomicU64,
    // Disk-checkpoint sink. None disables checkpointing (unit tests,
    // explicit opt-out). All store calls are best-effort: a failing
    // disk must never take down a live PTY.
    checkpoints: Option<Arc<CheckpointStore>>,
    // Set once, by `terminate_all`, for the rest of the daemon's life. Every
    // reader thread holds a clone: while it is set, a child that ends keeps its
    // checkpoint and raises no `Exit`. The sessions are ending because the
    // daemon is being restarted, and the app brings them back from those
    // checkpoints — an `Exit` would instead close tabs and fail agents.
    restarting: Arc<AtomicBool>,
    /// Held shared by a spawn from its `restarting` check until its session is
    /// in `entries`, and taken exclusively when the daemon stops taking new
    /// sessions. So a stop never looks at the sessions while a spawn that
    /// already passed its check is still forking: it either sees that session
    /// or the spawn sees the flag.
    spawn_gate: std::sync::RwLock<()>,
}

impl Default for PtyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyRegistry {
    pub fn new() -> Self {
        Self::with_checkpoints(None)
    }

    pub fn with_checkpoints(checkpoints: Option<Arc<CheckpointStore>>) -> Self {
        Self {
            entries: DashMap::new(),
            closing: Arc::new(DashMap::new()),
            next_attachment_id: AtomicU64::new(1),
            checkpoints,
            restarting: Arc::new(AtomicBool::new(false)),
            spawn_gate: std::sync::RwLock::new(()),
        }
    }

    pub fn spawn(&self, args: SpawnArgs) -> Result<String, RegistryError> {
        // A session started now would miss `terminate_all`'s snapshot: never
        // signalled, silent when it ends, and outliving the daemon if it
        // ignores the hangup. Checked under the gate, held to the insert.
        let _gate = self.spawn_gate.read().unwrap_or_else(|p| p.into_inner());
        if self.restarting.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("the daemon is restarting; not starting new sessions").into());
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: args.rows,
                cols: args.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        // Resolved here, not client-side, and that ordering is the point: a
        // paired phone asking for "a terminal" has no idea what shells the
        // host has, so the host is the only end that can answer.
        let shell = args.shell.unwrap_or_else(oximux_shell_env::default_shell);
        // Mint the PTY id up front so it can be injected into the child's
        // environment as OXIMUX_PTY_ID — the `oximux notify` CLI reads it to
        // tell the daemon which pane to raise attention on.
        let pty_id = Uuid::new_v4().to_string();
        let mut command = CommandBuilder::new(&shell);
        // Program argv (excluding argv[0]). Empty for a plain shell; set
        // when an agent launch is spawned directly with its flags.
        for a in &args.args {
            command.arg(a);
        }
        command.cwd(&args.cwd);
        // Terminal identity defaults. The daemon is spawned detached, so it
        // has no TERM in its own environment; without this the shell child
        // inherits none and curses/TUI apps degrade ("clear: TERM
        // environment variable not set", broken vim/pager/Claude Code
        // rendering). The emulator speaks xterm-256color with truecolor.
        // Set BEFORE the caller loop so an explicit caller TERM still wins.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // Host-terminal identity: tools and AI agents detect the emulator
        // via TERM_PROGRAM to toggle features (clickable links, keybinds).
        command.env("TERM_PROGRAM", "oximux");
        // Pane handle for `oximux notify` (set before the caller loop so an
        // explicit override still wins, though callers shouldn't set it).
        command.env("OXIMUX_PTY_ID", &pty_id);
        seed_utf8_locale(&mut command);
        // The daemon is the worst-affected spawn site, and the reason this call
        // exists. It is started detached by the app and outlives it, so it
        // carries whatever environment the app was launched with — forever.
        // Launch OxiMux once from a coding agent's shell and every pane in
        // every session from then on renders monochrome, with nothing in any
        // log to say why. Before the caller loop, so an explicit
        // `args.env` entry still wins.
        clear_inherited_colour_suppression(&mut command);
        for (k, v) in &args.env {
            command.env(k, v);
        }
        let child = pair.slave.spawn_command(command).context("spawn shell")?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        // Windows has no process group to signal, and `ChildKiller::kill` ends
        // exactly the shell — the compiler, dev server or test runner it was
        // running keeps going with nothing left to account for it. A job object
        // is what gives the tree a single handle to end. Adopted immediately
        // after spawn, so the only escapee would be something the shell forked
        // before its first instruction ran.
        #[cfg(windows)]
        let job = pid.and_then(|pid| match oximux_job_object::JobObject::adopt_pid(pid) {
            Ok(job) => Some(Arc::new(job)),
            Err(e) => {
                // Not fatal: the PTY still works and the direct child is still
                // killable. What is lost is the guarantee about its descendants,
                // which is worth a warning rather than a failed spawn.
                tracing::warn!(?e, pid, "could not put PTY child in a job object");
                None
            }
        });
        // The slave fd is duplicated into the child by spawn_command;
        // we no longer need our copy. Dropping it lets the kernel
        // deliver EOF to the master read side once the child exits.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = PtyWriter::spawn(&pty_id, pair.master.take_writer().context("take writer")?)?;

        let seeded_history = !args.prefill.is_empty();
        let mut seeded = RingBuffer::new(REPLAY_BUFFER_BYTES);
        // Before the reader thread exists, so no child output can land ahead
        // of the restored history.
        seeded.push(&args.prefill);
        let ring = Arc::new(Mutex::new(seeded));
        let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::new(Mutex::new(Vec::new()));
        let child_exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(AtomicI32::new(EXIT_CODE_NONE));

        // The reader thread owns the cloned read fd and the Child
        // handle (so it can wait for the exit code). It pushes bytes
        // into the ring and fans them to every live subscriber.
        let bytes_out = Arc::new(AtomicU64::new(0));
        let pty_id_for_reader = pty_id.clone();
        let ring_for_reader = Arc::clone(&ring);
        let subs_for_reader = Arc::clone(&subscribers);
        let exited_for_reader = Arc::clone(&child_exited);
        let exit_code_for_reader = Arc::clone(&exit_code);
        let bytes_out_for_reader = Arc::clone(&bytes_out);
        let checkpoints_for_reader = self.checkpoints.clone();
        let restarting_for_reader = Arc::clone(&self.restarting);
        let writer_for_reader = writer.clone();
        std::thread::Builder::new()
            .name(format!("relay-pty-{pty_id}"))
            .spawn(move || {
                reader_loop(
                    pty_id_for_reader,
                    reader,
                    child,
                    ring_for_reader,
                    subs_for_reader,
                    exited_for_reader,
                    exit_code_for_reader,
                    bytes_out_for_reader,
                    checkpoints_for_reader,
                    restarting_for_reader,
                    writer_for_reader,
                )
            })
            .context("spawn reader thread")?;

        // Seed the on-disk checkpoint dir up front so even a crash
        // before the first scrollback tick leaves an identifiable
        // session behind. The child pid rides along so the app can
        // resolve the shell's live cwd kernel-side (split inheritance)
        // without a wire round-trip. Best-effort: disk trouble must
        // not block the spawn.
        if let Some(store) = &self.checkpoints
            && let Err(e) = store.open(&pty_id, &args.cwd, args.cols, args.rows, pid)
        {
            tracing::warn!(?e, pty_id, "checkpoint open failed");
        }

        let entry = Arc::new(Entry {
            pty_id: pty_id.clone(),
            cwd: args.cwd,
            cols: Mutex::new(args.cols),
            rows: Mutex::new(args.rows),
            // No attachments yet — the spawning session auto-attaches via
            // the server's `attach` call immediately after this returns.
            attachments: Mutex::new(HashMap::new()),
            resize_seq: AtomicU64::new(0),
            master: Arc::new(Mutex::new(pair.master)),
            writer,
            killer: Mutex::new(killer),
            ring,
            subscribers,
            child_exited,
            exit_code,
            pid,
            #[cfg(windows)]
            job,
            bytes_in: AtomicU64::new(0),
            bytes_out,
            started_at: Instant::now(),
            // Seeded history is in the ring but not in `bytes_out`: start it
            // unwritten, or a shell that prints nothing never checkpoints it
            // and the next restart loses it.
            checkpointed_bytes_out: AtomicU64::new(if seeded_history { u64::MAX } else { 0 }),
        });
        self.entries.insert(pty_id.clone(), entry);
        Ok(pty_id)
    }

    /// Snapshot the replay ring and current dims WITHOUT touching subscribers
    /// or attachments — the resync path for a client told it `Gapped`.
    ///
    /// Deliberately not `attach`: that client is already attached and already
    /// holds an `attachment_id`. Attaching again would add a second entry to
    /// the smallest-screen-wins `min`, so recovering from a dropped frame
    /// would resize the live process. This answers only "what is on screen
    /// now?".
    pub fn replay(&self, pty_id: &str) -> Result<(Vec<u8>, u16, u16), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        // Ring first, matching the lock order `attach` and `fan_out` use, so a
        // snapshot taken here can never interleave with a push mid-write.
        let ring = entry.ring.lock().expect("ring poisoned");
        let replay = ring.snapshot();
        drop(ring);
        let cols = *entry.cols.lock().expect("cols poisoned");
        let rows = *entry.rows.lock().expect("rows poisoned");
        Ok((replay, cols, rows))
    }

    /// Attach a subscriber and return `(replay, cols, rows, attachment_id)`
    /// — the buffered raw output, the PTY's CURRENT (effective) grid
    /// dimensions, and a fresh per-attachment handle. The client must
    /// build its local emulator at exactly `(cols, rows)` before
    /// replaying, so absolute-position bytes land in the right cells and
    /// a later resize lets the live process repaint cleanly instead of
    /// reflowing scrambled content.
    ///
    /// The new attachment is registered at the CURRENT effective size, so
    /// the `min` (and therefore the PTY) is unchanged by the attach
    /// itself — it can only ever shrink once this attachment sends a
    /// smaller `resize`.
    pub fn attach(
        &self,
        pty_id: &str,
        sub: Sender<Notification>,
    ) -> Result<(Vec<u8>, u16, u16, u64), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        // Hold BOTH locks across snapshot + push so the operation is
        // atomic vs. `fan_out` (which takes the same two locks: ring
        // first while in `reader_loop`, then subscribers). Without
        // this, bytes that land in the ring between snapshot and
        // push would be in the next attacher's replay but missing
        // from this attacher's live stream — a silent gap.
        let ring = entry.ring.lock().expect("ring poisoned");
        let mut subs = entry.subscribers.lock().expect("subs poisoned");
        let replay = ring.snapshot();
        // If the child already died (the daemon outlives the app, so a
        // re-launched app can attach to a dead session), replay the terminal
        // `Exit` to THIS new subscriber after its scrollback — otherwise it
        // adopts the frozen ring as a live, input-less pane. Sent before the
        // push so `reader_loop`'s own one-shot fan-out (if it is still in
        // flight) can't also reach this sender; a benign duplicate is harmless
        // (the client treats Exit idempotently).
        // Allocated before the push so the subscriber carries the same id as
        // the attachment it belongs to; `detach` removes them as a pair. Taken
        // before the replayed `Exit` below rather than after, so that one can be
        // addressed to this attachment like every other notification — a client
        // routes on the id, and an unaddressed `Exit` would reach no one.
        let attachment_id = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        if entry.child_exited.load(Ordering::Acquire) {
            let raw = entry.exit_code.load(Ordering::Acquire);
            let code = (raw != EXIT_CODE_NONE).then_some(raw);
            // `try_send` (not async `send`) to match `fan_out`; the channel was
            // just created by the attaching client so it is empty.
            let _ = sub.try_send(Notification::Exit {
                pty_id: pty_id.to_owned(),
                attachment_id,
                code,
            });
        }
        subs.push(Subscriber {
            attachment_id,
            tx: sub,
            gapped: false,
        });
        drop(subs);
        drop(ring);
        let cols = *entry.cols.lock().expect("cols poisoned");
        let rows = *entry.rows.lock().expect("rows poisoned");
        entry
            .attachments
            .lock()
            .expect("attachments poisoned")
            .insert(attachment_id, (cols, rows));
        Ok((replay, cols, rows, attachment_id))
    }

    pub fn write(&self, pty_id: &str, bytes: &[u8]) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        entry.writer.write(bytes)?;
        entry
            .bytes_in
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Fan out an explicit attention notification to every subscriber of
    /// `pty_id` — driven by `Request::Notify` (the `oximux notify` CLI). The
    /// owning client maps the `Attention` notification to a pane attention
    /// signal (ring + tab dot).
    pub fn notify(&self, pty_id: &str, title: String, body: String) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        fan_out(
            &entry.subscribers,
            Notification::Attention {
                pty_id: pty_id.to_owned(),
                title,
                body,
            },
        );
        Ok(())
    }

    /// Inject a structured agent-status packet into `pty_id`'s output stream —
    /// driven by `Request::AgentStatus` (the `oximux agent-status` CLI an agent
    /// hook invokes). `payload` is an opaque JSON object string; we wrap it as
    /// an OSC-9999 sequence and fan it out as live `Output` to subscribers, so
    /// the app's existing OSC scanner decodes it into agent status. The daemon
    /// stays a dumb mux: it only adds the OSC envelope, never parses `payload`.
    ///
    /// Live-only on purpose (not written to the replay ring): status is
    /// ephemeral, and a hook re-fires it after any re-attach. The envelope is
    /// swallowed by the terminal emulator, so nothing renders on screen.
    pub fn agent_status(&self, pty_id: &str, payload: &str) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        // OSC 9999 ; <payload> BEL  (ESC = 0x1B, BEL = 0x07).
        let mut bytes = Vec::with_capacity(payload.len() + 7);
        bytes.extend_from_slice(b"\x1b]9999;");
        bytes.extend_from_slice(payload.as_bytes());
        bytes.push(0x07);
        fan_out(
            &entry.subscribers,
            Notification::Output {
                pty_id: pty_id.to_owned(),
                attachment_id: UNROUTED_ATTACHMENT,
                bytes,
            },
        );
        Ok(())
    }

    /// Record `attachment_id`'s requested size and re-apply the effective
    /// (`min`) PTY size. "Smallest screen wins": the PTY is driven at the
    /// element-wise minimum across all attachments so no viewer clips.
    /// `master.resize` is only called when the effective size actually
    /// changes, keeping the hot resize path cheap.
    pub fn resize(
        &self,
        pty_id: &str,
        attachment_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?
            .clone();
        entry
            .attachments
            .lock()
            .expect("attachments poisoned")
            .insert(attachment_id, (cols, rows));
        apply_effective_size(&entry)
    }

    /// Drop one attachment without killing the PTY. Recomputes the
    /// effective size so the PTY can grow back to the remaining
    /// attachments; with none left it retains its last size (the idle GC
    /// reaps a fully-detached PTY after the timeout). No-op if the
    /// attachment was already gone.
    pub fn detach(&self, pty_id: &str, attachment_id: u64) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?
            .clone();
        let removed = entry
            .attachments
            .lock()
            .expect("attachments poisoned")
            .remove(&attachment_id)
            .is_some();
        // Drop this attachment's notification sender too, not just its share of
        // the size accounting.
        //
        // The session that owned it cannot finish until every clone of its
        // `notif_tx` is gone: its pump task ends when the channel closes, its
        // writer ends when the pump drops the last outbound sender, and
        // `session_loop` awaits both before returning. Leaving the sender here
        // therefore does not merely leak a channel — it strands the whole
        // session task, so the daemon never counts the client as gone and the
        // last-client checkpoint flush never runs. `fan_out` used to be the only
        // thing that reaped these, which made the cleanup conditional on the PTY
        // happening to produce more output after the client had already left.
        entry
            .subscribers
            .lock()
            .expect("subs poisoned")
            .retain(|s| s.attachment_id != attachment_id);
        if removed {
            apply_effective_size(&entry)?;
        }
        Ok(())
    }

    pub async fn close(&self, pty_id: &str, grace: Duration) -> Result<(), RegistryError> {
        self.begin_close(pty_id)?.finish(grace).await;
        Ok(())
    }

    /// The part of a close that happens at once: the session leaves the
    /// list (so no later request sees it), moves to `closing` until it has
    /// ended, and is sent SIGTERM. The rest — the grace, the escalation, the
    /// checkpoint — is [`Closing::finish`], which the caller may run behind
    /// other requests.
    pub fn begin_close(&self, pty_id: &str) -> Result<Closing, RegistryError> {
        let entry = self
            .entries
            .remove(pty_id)
            .map(|(_, v)| v)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        self.closing.insert(entry.pty_id.clone(), Arc::clone(&entry));
        // SIGTERM to the process group; `finish` then polls the reader-set
        // `child_exited` flag until either the child reaped or the grace
        // window expires, and escalates on expiry (see `escalate`).
        send_sigterm(entry.pid);
        Ok(Closing {
            entry,
            checkpoints: self.checkpoints.clone(),
            closing: Arc::clone(&self.closing),
        })
    }

    /// Whether `pty_id` is being closed: out of the list, not yet ended.
    pub fn is_closing(&self, pty_id: &str) -> bool {
        self.closing.contains_key(pty_id)
    }

    /// Wait until a close already under way for `pty_id` has ended it.
    pub async fn wait_closed(&self, pty_id: &str) {
        while self.is_closing(pty_id) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// From now on, sessions that end keep their checkpoints and raise no
    /// `Exit`, and no new session starts. Idempotent; `terminate_all` begins
    /// with it.
    pub fn mark_restarting(&self) {
        self.restarting.store(true, Ordering::SeqCst);
        // Wait out any spawn that passed its check before the flag went up,
        // so every session it starts is in `entries` when this returns.
        drop(self.spawn_gate.write().unwrap_or_else(|p| p.into_inner()));
    }

    /// Stop taking new sessions — only if none is live, and in one step with
    /// every spawn, so a session started on another connection meanwhile is
    /// neither missed nor ended with the daemon. Whether it stopped.
    pub fn stop_if_idle(&self) -> bool {
        let _spawns = self.spawn_gate.write().unwrap_or_else(|p| p.into_inner());
        if self.live_count() > 0 {
            return false;
        }
        self.restarting.store(true, Ordering::SeqCst);
        true
    }

    /// End every session for a daemon restart, keeping what restoring them
    /// needs. Order matters:
    ///
    /// 1. The `restarting` flag goes up first, so no reader thread — however
    ///    fast its child dies — deletes a checkpoint or raises an `Exit`.
    /// 2. A checkpoint pass captures the freshest scrollback.
    /// 3. SIGTERM every process group, share one `grace` window across all of
    ///    them, then kill whatever is left (with its job tree on Windows).
    ///
    /// The explicit kill matters: children run in their own session, so a
    /// SIGHUP-ignoring agent would otherwise outlive the daemon — and be
    /// resumed a second time by the app.
    pub async fn terminate_all(self: &Arc<Self>, grace: Duration) {
        self.mark_restarting();
        let registry = Arc::clone(self);
        if let Err(err) = tokio::task::spawn_blocking(move || registry.checkpoint_all()).await {
            tracing::warn!(?err, "restart checkpoint pass did not complete");
        }
        // Sessions mid-close too: their close runs in a task this exit would
        // drop before it escalates.
        let entries: Vec<Arc<Entry>> = self
            .entries
            .iter()
            .chain(self.closing.iter())
            .map(|kv| Arc::clone(kv.value()))
            .filter(|e| !e.child_exited.load(Ordering::Acquire))
            .collect();
        // Windows has no graceful signal to send, so waiting out a grace
        // window there would only delay every restart.
        #[cfg(unix)]
        {
            for entry in &entries {
                send_sigterm(entry.pid);
            }
            wait_exited(&entries, grace).await;
        }
        #[cfg(not(unix))]
        let _ = grace;
        let escalated = escalate(&entries).await;
        #[cfg(windows)]
        for entry in &entries {
            if let Some(job) = &entry.job
                && let Err(e) = job.kill()
            {
                tracing::warn!(?e, pty_id = entry.pty_id, "job-object tree kill failed");
            }
        }
        tracing::info!(sessions = entries.len(), escalated, "ended every session for a daemon restart");
    }

    /// One disk-checkpoint pass over every live PTY: snapshot the replay
    /// ring and persist it, skipping PTYs whose `bytes_out` hasn't moved
    /// since the last pass. Driven by the server's periodic tick (and
    /// once more on graceful shutdown) from a blocking thread — each
    /// write is a small (≤ ring capacity) atomic file replace.
    pub fn checkpoint_all(&self) {
        let Some(store) = &self.checkpoints else {
            tracing::debug!("checkpoint pass with no store configured; nothing to do");
            return;
        };
        // Counted and logged because the interesting outcome here is a pass
        // that writes nothing. Every reason for that is legitimate on its own
        // — no PTYs, or none with new output — and indistinguishable from a
        // broken flush unless the pass says which it was.
        let (mut written, mut skipped, mut failed) = (0usize, 0usize, 0usize);
        for kv in self.entries.iter() {
            let e = kv.value();
            // An ended session's checkpoint is its reader's to keep or remove;
            // writing one here would resurrect a directory it just deleted.
            if e.child_exited.load(Ordering::Acquire) {
                skipped += 1;
                continue;
            }
            let seen = e.bytes_out.load(Ordering::Relaxed);
            if seen == e.checkpointed_bytes_out.load(Ordering::Relaxed) {
                skipped += 1;
                continue;
            }
            let bytes = e.ring.lock().expect("ring poisoned").snapshot();
            let cols = *e.cols.lock().expect("cols poisoned");
            let rows = *e.rows.lock().expect("rows poisoned");
            // Live cwd straight from the kernel (one proc_pidinfo per
            // ACTIVE pty per tick) — shells don't reliably announce cwd
            // changes in-band, and the cold-restore consumer wants the
            // directory the user was actually in when the daemon died.
            let live_cwd = e.pid.and_then(oximux_proc_cwd::cwd_of_pid);
            match store.write_scrollback(&e.pty_id, &bytes, cols, rows, live_cwd.as_deref()) {
                // Store the pre-snapshot counter: output that landed
                // mid-write is picked up by the next pass.
                Ok(()) => {
                    e.checkpointed_bytes_out.store(seen, Ordering::Relaxed);
                    written += 1;
                }
                Err(err) => {
                    failed += 1;
                    tracing::warn!(?err, pty_id = e.pty_id, "checkpoint write failed");
                }
            }
        }
        tracing::debug!(written, skipped, failed, "checkpoint pass complete");
    }

    /// PTYs available for warm re-attach. A PTY whose child has already
    /// exited is kept in the registry (its ring + checkpoint back replay and
    /// cold-restore), but it is NOT live: re-attaching to it shows the
    /// replayed scrollback while every keystroke writes to a PTY with no
    /// reader, so the pane looks alive yet silently swallows input. Excluding
    /// exited entries here makes the restore liveness gate honest, so a dead
    /// session falls through to a fresh respawn instead of attaching a corpse.
    pub fn list(&self) -> Vec<PtyDescriptor> {
        self.entries
            .iter()
            .filter(|kv| !kv.value().child_exited.load(Ordering::Acquire))
            .map(|kv| {
                let e = kv.value();
                PtyDescriptor {
                    pty_id: e.pty_id.clone(),
                    cwd: e.cwd.to_string_lossy().into_owned(),
                    cols: *e.cols.lock().expect("cols poisoned"),
                    rows: *e.rows.lock().expect("rows poisoned"),
                }
            })
            .collect()
    }

    /// Sessions not yet ended, closing ones included.
    pub fn live_count(&self) -> usize {
        self.entries.len() + self.closing.len()
    }

    pub fn stats(&self) -> Vec<PtyStats> {
        self.entries
            .iter()
            .map(|kv| {
                let e = kv.value();
                PtyStats {
                    pty_id: e.pty_id.clone(),
                    bytes_in: e.bytes_in.load(Ordering::Relaxed),
                    bytes_out: e.bytes_out.load(Ordering::Relaxed),
                    alive_secs: e.started_at.elapsed().as_secs(),
                }
            })
            .collect()
    }
}

/// Recompute the effective PTY size as the element-wise `min` over the
/// entry's attachments and apply it to the master fd — but only when it
/// differs from the size currently applied, so the common "nothing
/// changed" case is a cheap compare with no syscall. With no attachments
/// left the PTY retains its last size (early return). On an effective
/// change, arms a deferred "resend confirm".
///
/// Lock discipline: `attachments` is released before `cols`/`rows`/
/// `master` are taken. Concurrent resizes from different attachments
/// therefore serialize on the `cols`/`rows` mutexes and converge on the
/// true `min` (each writes its own value into the shared map before
/// calling this), so a transiently-stale read self-corrects on the next
/// call rather than deadlocking.
fn apply_effective_size(entry: &Arc<Entry>) -> Result<(), RegistryError> {
    let (min_cols, min_rows) = {
        let atts = entry.attachments.lock().expect("attachments poisoned");
        if atts.is_empty() {
            return Ok(());
        }
        let min_cols = atts.values().map(|(c, _)| *c).min().unwrap_or(1).max(1);
        let min_rows = atts.values().map(|(_, r)| *r).min().unwrap_or(1).max(1);
        (min_cols, min_rows)
    };

    let mut cur_cols = entry.cols.lock().expect("cols poisoned");
    let mut cur_rows = entry.rows.lock().expect("rows poisoned");
    if *cur_cols == min_cols && *cur_rows == min_rows {
        return Ok(());
    }
    entry
        .master
        .lock()
        .expect("master poisoned")
        .resize(PtySize {
            rows: min_rows,
            cols: min_cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("pty resize")?;
    *cur_cols = min_cols;
    *cur_rows = min_rows;
    drop(cur_cols);
    drop(cur_rows);

    let seq = entry.resize_seq.fetch_add(1, Ordering::AcqRel) + 1;
    arm_resize_resend(entry, min_cols, min_rows, seq);
    Ok(())
}

/// Spawn a detached task that re-applies `(cols, rows)` a couple of times
/// after short delays — a nudge for TUIs that miss the first SIGWINCH.
/// Each resend re-checks `resize_seq`: a newer effective resize bumps the
/// seq and supersedes any in-flight resend, so a stale resend can't
/// clobber a fresher size. No-op outside a tokio runtime (direct unit
/// tests), where the resend is unnecessary.
fn arm_resize_resend(entry: &Arc<Entry>, cols: u16, rows: u16, seq: u64) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let entry = Arc::clone(entry);
    handle.spawn(async move {
        for delay_ms in RESIZE_RESEND_DELAYS_MS {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if entry.resize_seq.load(Ordering::Acquire) != seq {
                return; // superseded by a newer resize
            }
            let _ = entry
                .master
                .lock()
                .expect("master poisoned")
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
        }
    });
}

/// After a SIGTERM grace: how long a hung-up child gets before SIGKILL.
#[cfg(unix)]
const HANGUP_GRACE: Duration = Duration::from_millis(500);

/// A session [`PtyRegistry::begin_close`] took out of the list, still to be
/// ended. Leaves `closing` when dropped — however its close ends, a panic or a
/// cancelled task included — so the daemon is never left counting a session
/// no one is ending.
pub struct Closing {
    entry: Arc<Entry>,
    checkpoints: Option<Arc<CheckpointStore>>,
    closing: Arc<DashMap<String, Arc<Entry>>>,
}

impl Closing {
    /// Wait out `grace`, then end whatever is left of the session.
    pub async fn finish(self, grace: Duration) {
        let entry = &self.entry;
        wait_exited(std::slice::from_ref(entry), grace).await;
        escalate(std::slice::from_ref(entry)).await;
        // `escalate` ends the shell; on Windows that leaves its
        // descendants running, so the job is what actually closes the session.
        // After the grace window rather than instead of it: a shell given the
        // chance to exit on its own lets its children finish writing.
        #[cfg(windows)]
        if let Some(job) = &entry.job
            && let Err(e) = job.kill()
        {
            tracing::warn!(?e, pty_id = %entry.pty_id, "job-object tree kill failed");
        }
        // Deliberate kill — nothing to cold-restore. The reader thread
        // also removes on its way out; remove is idempotent.
        if let Some(store) = &self.checkpoints {
            let _ = store.remove(&entry.pty_id);
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.closing.remove(&self.entry.pty_id);
    }
}

/// Wait until every entry's child has exited, or `within` elapses.
async fn wait_exited(entries: &[Arc<Entry>], within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline
        && entries.iter().any(|e| !e.child_exited.load(Ordering::Acquire))
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// End whatever survived the SIGTERM grace, returning how many that was.
///
/// Hang up first: `ChildKiller::kill` sends SIGHUP on unix, which is the
/// signal an interactive shell actually honours (it ignores SIGTERM) — it
/// exits cleanly, saving history and hanging up its background jobs, which sit
/// in process groups a group signal never reaches. Only what ignores the
/// hangup too gets a process-group SIGKILL, so no child can outlive its
/// session. On Windows the killer is TerminateProcess and the caller's job
/// kill ends the tree.
async fn escalate(entries: &[Arc<Entry>]) -> usize {
    let alive: Vec<Arc<Entry>> = entries
        .iter()
        .filter(|e| !e.child_exited.load(Ordering::Acquire))
        .cloned()
        .collect();
    for entry in &alive {
        let _ = entry.killer.lock().expect("killer poisoned").kill();
    }
    #[cfg(unix)]
    {
        wait_exited(&alive, HANGUP_GRACE).await;
        for entry in alive.iter().filter(|e| !e.child_exited.load(Ordering::Acquire)) {
            send_sigkill(entry.pid);
        }
    }
    alive.len()
}

fn send_sigterm(pid: Option<u32>) {
    #[cfg(unix)]
    signal_group(pid, nix::sys::signal::Signal::SIGTERM);
    #[cfg(not(unix))]
    let _ = pid;
}

/// The end of a grace window. Needed on top of `ChildKiller::kill`, which on
/// unix sends only SIGHUP: a child that ignores it (and SIGTERM) would outlive
/// its session, and a daemon restart would then resume a second copy of it.
#[cfg(unix)]
fn send_sigkill(pid: Option<u32>) {
    signal_group(pid, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, signal: nix::sys::signal::Signal) {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    let Some(pid) = pid else { return };
    let Ok(pid_i32) = i32::try_from(pid) else {
        tracing::warn!(pid, ?signal, "pid > i32::MAX, refusing to signal");
        return;
    };
    // Negative pid → process group. portable-pty's spawned child
    // setsid()s before exec, so pgid == child pid.
    match kill(Pid::from_raw(-pid_i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => tracing::warn!(?e, pid, ?signal, "signalling the process group failed"),
    }
}

/// Sentinel `exit_code` for "the child carried no status" — a signal or
/// detach where `Child::wait` returned `None`. Distinguishes a real `0`
/// (clean exit) from "unknown" so `attach` can replay the correct
/// `Notification::Exit { code }` (`None` here) to a late-reconnecting client.
const EXIT_CODE_NONE: i32 = i32::MIN;

/// How long [`reader_loop`] keeps draining output after the child has been
/// reaped, before it publishes `Exit`.
///
/// The daemon cannot key exit off the reader seeing EOF, because on Windows it
/// never does: a ConPTY's output pipe belongs to the pseudoconsole, which
/// outlives the child and is released only when the master is dropped at
/// `close`. A shell that exits on its own — `exit`, a crash, a finished agent —
/// produces no EOF at all, so an EOF-only loop would never fan out
/// `Notification::Exit`, never drop the checkpoint, and leave the entry looking
/// live to `list` forever.
///
/// The window covers the hand-off race: `child.wait()` can return while the last
/// bytes the child wrote are still in flight, and those belong in the ring ahead
/// of `Exit`. It only ever delays a session that has already ended.
///
/// A deadline from the moment of reaping rather than an idle timeout, for the same
/// reason as its twin in `oximux-pty`: a detached grandchild that inherits the pty
/// and keeps writing would hold an idle timeout open forever, and the session
/// would never report `Exit` — the original bug, reintroduced.
const POST_EXIT_DRAIN: Duration = Duration::from_millis(200);

/// Wake-up granularity for the drain loop while no output is arriving.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(20);

// The reader task owns the PTY's full I/O state (reader, child handle, ring,
// subscribers, exit flags); threading it as discrete args keeps the spawn site
// explicit rather than hiding it behind a bag struct.
#[allow(clippy::too_many_arguments)]
fn reader_loop(
    pty_id: String,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ring: Arc<Mutex<RingBuffer>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    child_exited: Arc<AtomicBool>,
    exit_code: Arc<AtomicI32>,
    bytes_out: Arc<AtomicU64>,
    checkpoints: Option<Arc<CheckpointStore>>,
    restarting: Arc<AtomicBool>,
    writer: PtyWriter,
) {
    // Reap on a thread of its own rather than after the read loop, and read on a
    // thread of its own rather than inline, so the loop below can end the session
    // on whichever comes first: EOF, or the child being reaped plus a short
    // drain. Waiting for EOF alone is what does not port — see `POST_EXIT_DRAIN`.
    let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel::<Option<i32>>(1);
    std::thread::spawn(move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let _ = exit_tx.send(code);
    });

    // Dropping `bytes_tx` is how the reader reports EOF, keeping the unix path's
    // "pty closed, session over" behaviour exactly as it was.
    let (bytes_tx, bytes_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let pty_id_for_reader = pty_id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    // Blocking send: these bytes are the scrollback, so applying
                    // backpressure to the pty is right and dropping them is not.
                    if bytes_tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(?e, pty_id = pty_id_for_reader, "reader EOF/err");
                    return;
                }
            }
        }
    });

    // Live output only: replayed history never reaches here, so it is never answered.
    let mut queries = ProgramStatusQueryDetector::new();
    let mut publish = |bytes: &[u8]| {
        queries.feed(bytes, Instant::now(), |ticket| writer.try_reply(ticket));
        bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        // Lock-order discipline: ring before subscribers, same
        // order `attach` uses, to keep snapshot+push atomic.
        let mut rb = ring.lock().expect("ring poisoned");
        rb.push(bytes);
        drop(rb);
        fan_out(
            &subscribers,
            Notification::Output {
                pty_id: pty_id.clone(),
                attachment_id: UNROUTED_ATTACHMENT,
                bytes: bytes.to_vec(),
            },
        );
    };

    let mut reaped: Option<Option<i32>> = None;
    let mut drain_deadline: Option<Instant> = None;
    // Whether a daemon restart ended this session, decided the moment the end
    // is first seen — not after the drain, when a restart that began since
    // would claim a session that had already ended on its own.
    let mut ended_by_restart: Option<bool> = None;
    loop {
        match bytes_rx.recv_timeout(DRAIN_POLL_INTERVAL) {
            Ok(chunk) => publish(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ended_by_restart.get_or_insert_with(|| restarting.load(Ordering::SeqCst));
                break;
            }
        }
        if reaped.is_none()
            && let Ok(code) = exit_rx.try_recv()
        {
            ended_by_restart.get_or_insert_with(|| restarting.load(Ordering::SeqCst));
            reaped = Some(code);
            drain_deadline = Some(Instant::now() + POST_EXIT_DRAIN);
        }
        if let Some(deadline) = drain_deadline
            && Instant::now() >= deadline
        {
            break;
        }
    }

    // Output the reader already handed over belongs in the ring ahead of `Exit`.
    while let Ok(chunk) = bytes_rx.try_recv() {
        publish(&chunk);
    }

    // On the EOF route the reaper has not been consulted yet, and its status is
    // worth blocking for: EOF means the pty is gone, so `wait` is about to
    // return. On the reaped route the code is already in hand.
    let code = match reaped {
        Some(code) => code,
        None => exit_rx.recv().unwrap_or(None),
    };
    // Stash the status BEFORE flipping `child_exited`, so any reader that
    // observes the flag (here or in `attach`) also sees the final code.
    exit_code.store(code.unwrap_or(EXIT_CODE_NONE), Ordering::Release);
    // Release-ordered so the corresponding Acquire load in `close`
    // observes the flag flip without sequencing the fan_out below.
    child_exited.store(true, Ordering::Release);
    // Ended by a daemon restart, not by itself: keep the checkpoint the app
    // restores from, and stay quiet — see `PtyRegistry::restarting`.
    if ended_by_restart.unwrap_or_else(|| restarting.load(Ordering::SeqCst)) {
        tracing::debug!(pty_id, "session ended by a daemon restart; keeping its checkpoint");
        return;
    }
    // Natural child exit is a clean end — drop the disk checkpoint so
    // an exited shell never cold-restores on the next launch. (The
    // `close` path removes too; remove is idempotent.)
    if let Some(store) = &checkpoints {
        let _ = store.remove(&pty_id);
    }
    fan_out(
        &subscribers,
        Notification::Exit {
            pty_id: pty_id.clone(),
            attachment_id: UNROUTED_ATTACHMENT,
            code,
        },
    );
}

fn fan_out(subscribers: &Arc<Mutex<Vec<Subscriber>>>, notif: Notification) {
    let mut subs = subscribers.lock().expect("subs poisoned");
    // Drop subscribers whose receiver has been dropped. For "full" we keep the
    // subscriber but discard the message — the replay ring is the source of
    // truth for a slow client. Discarding *silently* is what this used to do,
    // and it made that recovery unreachable: the client had no way to learn it
    // had missed anything, so it never re-attached and kept rendering a
    // terminal with a hole in it.
    subs.retain_mut(|sub| {
        // A pending gap is announced before any further output, so the client
        // re-attaches rather than compositing new bytes onto a stale grid.
        // This cannot be sent at the moment the gap opens: the queue being
        // full is precisely what caused it, so the signal waits for room.
        if sub.gapped {
            match sub.tx.try_send(Notification::Gapped {
                pty_id: pty_id_of(&notif).to_owned(),
                attachment_id: sub.attachment_id,
            }) {
                Ok(()) => sub.gapped = false,
                // Still backed up. Keep the flag and try again next time
                // rather than dropping the notice along with the bytes.
                Err(TrySendError::Full(_)) => return true,
                Err(TrySendError::Closed(_)) => return false,
            }
        }
        // Address each copy to the attachment it is going to. The daemon
        // already sent one per attachment; naming it is what lets a client
        // holding several on one PTY route them to the right subscriber
        // instead of showing every byte once per attachment it holds.
        match sub.tx.try_send(notif.clone().addressed_to(sub.attachment_id)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::warn!("subscriber queue full; dropping notification");
                sub.gapped = true;
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    });
}

/// The session a notification belongs to. Every variant carries one, so a gap
/// notice can be addressed to the same session as the output it displaced.
fn pty_id_of(notif: &Notification) -> &str {
    match notif {
        Notification::Output { pty_id, .. }
        | Notification::Exit { pty_id, .. }
        | Notification::Attention { pty_id, .. }
        | Notification::Gapped { pty_id, .. } => pty_id,
    }
}

#[cfg(test)]
mod detach_tests {
    use super::*;

    /// Detaching must drop the attachment's notification sender, not only its
    /// share of the size accounting.
    ///
    /// The session task holding the other end cannot finish while a clone of
    /// its sender is parked here: its pump ends when the channel closes, its
    /// writer ends when the pump drops the last outbound sender, and
    /// `session_loop` awaits both. So a sender left behind does not leak a
    /// channel, it strands the session — the daemon never counts that client as
    /// gone, and the last-client checkpoint flush never fires.
    ///
    /// This went unnoticed because `fan_out` also reaps closed receivers, which
    /// hid it anywhere the PTY kept talking after the client left. On a shell
    /// that goes quiet at a prompt, nothing ever reaped it.
    #[tokio::test]
    async fn detach_drops_the_subscribers_sender_without_waiting_for_output() {
        let reg = PtyRegistry::new();
        let pty_id = reg
            .spawn(SpawnArgs {
                cwd: oximux_shell_env::test_support::test_cwd(),
                cols: 80,
                rows: 24,
                shell: Some(oximux_shell_env::test_support::test_shell()),
                args: Vec::new(),
                env: Vec::new(),
                prefill: Vec::new(),
            })
            .expect("spawn");

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (_replay, _cols, _rows, attachment_id) = reg.attach(&pty_id, tx).expect("attach");
        {
            let entry = reg.entries.get(&pty_id).expect("entry");
            let subs = entry.subscribers.lock().expect("subs poisoned");
            assert_eq!(subs.len(), 1, "the attach should have registered one");
        }

        reg.detach(&pty_id, attachment_id).expect("detach");

        let entry = reg.entries.get(&pty_id).expect("entry");
        assert!(
            entry
                .subscribers
                .lock()
                .expect("subs poisoned")
                .is_empty(),
            "detach must remove the subscriber, with no output needed to reap it"
        );
    }
}

#[cfg(test)]
mod fan_out_tests {
    use super::*;

    fn output(n: u8) -> Notification {
        Notification::Output {
            attachment_id: 0,
            pty_id: "pty-1".into(),
            bytes: vec![n],
        }
    }

    fn gapped() -> Notification {
        Notification::Gapped {
            pty_id: "pty-1".into(),
            attachment_id: 0,
        }
    }

    /// Every copy must name the attachment it went to.
    ///
    /// The daemon has always sent one copy per attachment; before v9 they were
    /// indistinguishable on the wire, so a client holding two attachments to one
    /// PTY received two identical frames, handed both to every local subscriber,
    /// and rendered each byte twice.
    #[tokio::test]
    async fn fan_out_addresses_each_copy_to_its_own_attachment() {
        let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(4);
        let (tx_b, mut rx_b) = tokio::sync::mpsc::channel(4);
        let subs: Subs = Arc::new(Mutex::new(vec![
            Subscriber { attachment_id: 7, tx: tx_a, gapped: false },
            Subscriber { attachment_id: 9, tx: tx_b, gapped: false },
        ]));

        fan_out(&subs, output(1));

        assert_eq!(rx_a.recv().await.unwrap().attachment_id(), Some(7));
        assert_eq!(rx_b.recv().await.unwrap().attachment_id(), Some(9));
    }

    /// A gap belongs to the subscriber that fell behind, so its notice has to
    /// carry that subscriber's id — telling the others would send them
    /// re-attaching over a stream that never had a hole in it.
    #[tokio::test]
    async fn a_gap_notice_names_the_attachment_that_fell_behind() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let subs: Subs = Arc::new(Mutex::new(vec![Subscriber {
            attachment_id: 42,
            tx,
            gapped: false,
        }]));

        fan_out(&subs, output(1)); // fills the single slot
        fan_out(&subs, output(2)); // dropped — the gap opens
        assert_eq!(rx.recv().await.unwrap().attachment_id(), Some(42));
        fan_out(&subs, output(3)); // room again: the notice goes first

        let notice = rx.recv().await.unwrap();
        assert!(matches!(notice, Notification::Gapped { .. }), "got {notice:?}");
        assert_eq!(notice.attachment_id(), Some(42));
    }

    type Subs = Arc<Mutex<Vec<Subscriber>>>;

    fn subscribers(cap: usize) -> (Subs, tokio::sync::mpsc::Receiver<Notification>) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        (
            Arc::new(Mutex::new(vec![Subscriber {
                attachment_id: 0,
                tx,
                gapped: false,
            }])),
            rx,
        )
    }

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<Notification>) -> Vec<Notification> {
        let mut out = Vec::new();
        while let Ok(n) = rx.try_recv() {
            out.push(n);
        }
        out
    }

    /// A subscriber that falls behind is TOLD, once room frees up.
    ///
    /// This is the whole point of the flag: the queue being full is what causes
    /// the gap, so the notice cannot be delivered at the moment it happens. If
    /// it were simply dropped along with the bytes — which is what this used to
    /// do — the client would never learn to re-attach, and the replay ring that
    /// holds the missed output would be unreachable in practice.
    #[tokio::test]
    async fn a_subscriber_that_missed_output_is_told_once_there_is_room() {
        let (subs, mut rx) = subscribers(2);

        fan_out(&subs, output(1));
        fan_out(&subs, output(2)); // queue now full
        fan_out(&subs, output(3)); // dropped — the gap opens

        assert_eq!(drain(&mut rx), vec![output(1), output(2)]);

        fan_out(&subs, output(4));
        assert_eq!(
            drain(&mut rx),
            vec![gapped(), output(4)],
            "the gap is announced BEFORE the next output, so the client re-attaches \
             rather than compositing fresh bytes onto a stale grid",
        );
    }

    /// The notice is sent once per gap, not once per dropped message: a
    /// subscriber that is behind is behind, and repeating it would compete for
    /// the very queue space that is already exhausted.
    #[tokio::test]
    async fn the_gap_notice_is_not_repeated_once_the_subscriber_catches_up() {
        let (subs, mut rx) = subscribers(2);

        fan_out(&subs, output(1));
        fan_out(&subs, output(2));
        fan_out(&subs, output(3)); // dropped
        drain(&mut rx);

        // Asserted, not merely drained: without this the test would pass
        // vacuously if no notice were ever sent — the exact regression the
        // sibling test guards against.
        fan_out(&subs, output(4));
        assert_eq!(drain(&mut rx), vec![gapped(), output(4)], "the gap is announced once");

        fan_out(&subs, output(5));
        assert_eq!(
            drain(&mut rx),
            vec![output(5)],
            "a caught-up subscriber gets plain output, not a second gap notice",
        );
    }

    /// A subscriber still congested when the notice goes out gaps again — the
    /// notice occupies queue space like any other message, so the output it was
    /// announcing can itself be dropped.
    ///
    /// That is correct rather than unfortunate: a second gap really did occur,
    /// and the client is told about it. Pinned here because the alternative
    /// reading ("one gap notice per congestion episode") is the intuitive one
    /// and would make a future refactor suppress a notice that is real.
    #[tokio::test]
    async fn a_still_congested_subscriber_gaps_again() {
        let (subs, mut rx) = subscribers(1);

        fan_out(&subs, output(1)); // fills the single slot
        fan_out(&subs, output(2)); // dropped — gap opens
        assert_eq!(drain(&mut rx), vec![output(1)]);

        // One slot free: the notice takes it, so output(3) has nowhere to go.
        fan_out(&subs, output(3));
        assert_eq!(drain(&mut rx), vec![gapped()]);

        fan_out(&subs, output(4));
        assert_eq!(
            drain(&mut rx),
            vec![gapped()],
            "the dropped output(3) opened a fresh gap, which is announced in turn",
        );
    }

    /// A subscriber whose receiver is gone is dropped rather than retained
    /// forever — including while a gap is pending, which is its own early-return
    /// branch in the retain and would otherwise leak the entry.
    #[tokio::test]
    async fn a_closed_subscriber_is_dropped_even_with_a_gap_pending() {
        let (subs, mut rx) = subscribers(1);

        fan_out(&subs, output(1));
        fan_out(&subs, output(2)); // gap opens
        rx.close();
        drop(rx);

        fan_out(&subs, output(3));
        assert!(
            subs.lock().unwrap().is_empty(),
            "the dead subscriber is reaped on the gap-notice path too",
        );
    }
}

#[cfg(all(test, unix))]
mod close_tests {
    use super::*;

    fn stubborn(reg: &PtyRegistry) -> String {
        reg.spawn(SpawnArgs {
            cwd: oximux_shell_env::test_support::test_cwd(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            // Ignores SIGTERM, as an idle interactive shell does.
            args: vec!["-c".into(), "trap '' TERM; sleep 60".into()],
            env: Vec::new(),
            prefill: Vec::new(),
        })
        .expect("spawn")
    }

    // A restored pane's history is seeded into the ring ahead of the child's
    // output, so a later attach — and the checkpoint — replays it too.
    #[tokio::test]
    async fn a_seeded_session_replays_its_history_first() {
        let reg = PtyRegistry::new();
        let pty_id = reg
            .spawn(SpawnArgs {
                cwd: oximux_shell_env::test_support::test_cwd(),
                cols: 80,
                rows: 24,
                shell: Some("/bin/sh".into()),
                args: vec!["-c".into(), "echo fresh; sleep 60".into()],
                env: Vec::new(),
                prefill: b"old history\r\n".to_vec(),
            })
            .expect("spawn");
        let mut replay = Vec::new();
        for _ in 0..100 {
            replay = reg.replay(&pty_id).expect("replay").0;
            if String::from_utf8_lossy(&replay).contains("fresh") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let text = String::from_utf8_lossy(&replay);
        assert!(text.starts_with("old history\r\n"), "{text:?}");
        assert!(text.contains("fresh"), "{text:?}");
        let _ = reg.begin_close(&pty_id);
    }

    // A session mid-close is off the list at once, yet still counts as live —
    // so the daemon neither goes idle nor lets a restart forget it — until
    // its close has ended it.
    #[tokio::test]
    async fn a_closing_session_is_unlisted_but_live_until_it_ends() {
        let reg = Arc::new(PtyRegistry::new());
        let pty_id = stubborn(&reg);
        // Until the shell has run its `trap`, SIGTERM still ends it at once.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let closing = reg.begin_close(&pty_id).expect("begin");

        assert!(reg.list().is_empty(), "off the list at once");
        assert_eq!(reg.live_count(), 1, "still live");
        assert!(reg.is_closing(&pty_id));
        assert!(matches!(reg.begin_close(&pty_id), Err(RegistryError::NotFound(_))));

        let waiter = {
            let reg = Arc::clone(&reg);
            let pty_id = pty_id.clone();
            tokio::spawn(async move { reg.wait_closed(&pty_id).await })
        };
        closing.finish(Duration::from_millis(200)).await;
        waiter.await.expect("a second close waits for the first");

        assert_eq!(reg.live_count(), 0);
        assert!(!reg.is_closing(&pty_id));
    }

    // A restart that lands mid-close still ends that session.
    #[tokio::test]
    async fn a_restart_ends_a_session_mid_close() {
        let reg = Arc::new(PtyRegistry::new());
        let pty_id = stubborn(&reg);
        // Held, not finished: the close task still waiting out its grace.
        let closing = reg.begin_close(&pty_id).expect("begin");
        let entry = Arc::clone(&closing.entry);

        reg.terminate_all(Duration::from_millis(200)).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !entry.child_exited.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(entry.child_exited.load(Ordering::Acquire), "the session was ended");
        drop(closing);
        assert_eq!(reg.live_count(), 0, "a dropped close leaves nothing behind");
    }
}
