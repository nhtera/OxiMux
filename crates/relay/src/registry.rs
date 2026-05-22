use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use dashmap::DashMap;
use oximux_relay_proto::{ErrCode, Notification, PtyDescriptor, PtyStats};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

use crate::ring_buffer::RingBuffer;

// Bounded per-subscriber channel. The session writer task drains it;
// if it falls behind, fan_out drops notifications (with a tracing warn)
// rather than growing memory unboundedly. The replay buffer is the
// source of truth — a slow client gets the missed bytes on its next
// Attach, and the live stream gaps until it catches up.
pub const SUBSCRIBER_QUEUE: usize = 1024;

// 1 MiB per PTY — the plan's "Locked decisions" section, larger than
// the reference UX's 100 KiB because TUIs (`cc`, `vim`, full-screen pagers) can
// repaint dense screens that need more headroom for byte-for-byte
// replay to look correct.
pub const REPLAY_BUFFER_BYTES: usize = 1024 * 1024;

const READ_CHUNK_BYTES: usize = 8 * 1024;

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
    pub env: Vec<(String, String)>,
}

struct Entry {
    pty_id: String,
    cwd: PathBuf,
    cols: Mutex<u16>,
    rows: Mutex<u16>,
    // Kept alive so we can call `resize()` on the master fd. The
    // reader has its own cloned fd.
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    ring: Arc<Mutex<RingBuffer>>,
    subscribers: Arc<Mutex<Vec<Sender<Notification>>>>,
    // Flipped by `reader_loop` immediately before the Exit fan-out.
    // The `close` grace poll watches this to skip the full SIGKILL
    // wait when the child has already reaped naturally.
    child_exited: Arc<AtomicBool>,
    // Best-effort PID of the spawned child for the SIGTERM step. None
    // on non-Unix or when portable-pty returns None.
    pid: Option<u32>,
    // Phase-07: per-PTY counters surfaced by `Request::Stats`.
    bytes_in: AtomicU64,
    bytes_out: Arc<AtomicU64>,
    started_at: Instant,
}

pub struct PtyRegistry {
    entries: DashMap<String, Arc<Entry>>,
}

impl Default for PtyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyRegistry {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    pub fn spawn(&self, args: SpawnArgs) -> Result<String, RegistryError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: args.rows,
                cols: args.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let shell = args
            .shell
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/zsh".into());
        let mut command = CommandBuilder::new(&shell);
        command.cwd(&args.cwd);
        for (k, v) in &args.env {
            command.env(k, v);
        }
        let child = pair.slave.spawn_command(command).context("spawn shell")?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        // The slave fd is duplicated into the child by spawn_command;
        // we no longer need our copy. Dropping it lets the kernel
        // deliver EOF to the master read side once the child exits.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let pty_id = Uuid::new_v4().to_string();
        let ring = Arc::new(Mutex::new(RingBuffer::new(REPLAY_BUFFER_BYTES)));
        let subscribers: Arc<Mutex<Vec<Sender<Notification>>>> = Arc::new(Mutex::new(Vec::new()));
        let child_exited = Arc::new(AtomicBool::new(false));

        // The reader thread owns the cloned read fd and the Child
        // handle (so it can wait for the exit code). It pushes bytes
        // into the ring and fans them to every live subscriber.
        let bytes_out = Arc::new(AtomicU64::new(0));
        let pty_id_for_reader = pty_id.clone();
        let ring_for_reader = Arc::clone(&ring);
        let subs_for_reader = Arc::clone(&subscribers);
        let exited_for_reader = Arc::clone(&child_exited);
        let bytes_out_for_reader = Arc::clone(&bytes_out);
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
                    bytes_out_for_reader,
                )
            })
            .context("spawn reader thread")?;

        let entry = Arc::new(Entry {
            pty_id: pty_id.clone(),
            cwd: args.cwd,
            cols: Mutex::new(args.cols),
            rows: Mutex::new(args.rows),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            ring,
            subscribers,
            child_exited,
            pid,
            bytes_in: AtomicU64::new(0),
            bytes_out,
            started_at: Instant::now(),
        });
        self.entries.insert(pty_id.clone(), entry);
        Ok(pty_id)
    }

    pub fn attach(
        &self,
        pty_id: &str,
        sub: Sender<Notification>,
    ) -> Result<Vec<u8>, RegistryError> {
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
        subs.push(sub);
        drop(subs);
        drop(ring);
        Ok(replay)
    }

    pub fn write(&self, pty_id: &str, bytes: &[u8]) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        let mut w = entry.writer.lock().expect("writer poisoned");
        w.write_all(bytes).context("pty write")?;
        w.flush().context("pty flush")?;
        entry
            .bytes_in
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn resize(&self, pty_id: &str, cols: u16, rows: u16) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(pty_id)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;
        entry
            .master
            .lock()
            .expect("master poisoned")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        *entry.cols.lock().expect("cols poisoned") = cols;
        *entry.rows.lock().expect("rows poisoned") = rows;
        Ok(())
    }

    pub async fn close(&self, pty_id: &str, grace: Duration) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .remove(pty_id)
            .map(|(_, v)| v)
            .ok_or_else(|| RegistryError::NotFound(pty_id.into()))?;

        // SIGTERM to the process group, then poll the reader-set
        // `child_exited` flag until either the child reaped or the
        // grace window expires. On expiry, SIGKILL via portable-pty's
        // ChildKiller (idempotent if the child is already gone).
        send_sigterm(entry.pid);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if entry.child_exited.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = entry.killer.lock().expect("killer poisoned").kill();
        Ok(())
    }

    pub fn list(&self) -> Vec<PtyDescriptor> {
        self.entries
            .iter()
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

    pub fn live_count(&self) -> usize {
        self.entries.len()
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

fn send_sigterm(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let Ok(pid_i32) = i32::try_from(pid) else {
            tracing::warn!(pid, "pid > i32::MAX, refusing SIGTERM");
            return;
        };
        // Negative pid → process group. portable-pty's spawned child
        // setsid()s before exec, so pgid == child pid.
        match kill(Pid::from_raw(-pid_i32), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(e) => tracing::warn!(?e, pid, "SIGTERM failed"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn reader_loop(
    pty_id: String,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ring: Arc<Mutex<RingBuffer>>,
    subscribers: Arc<Mutex<Vec<Sender<Notification>>>>,
    child_exited: Arc<AtomicBool>,
    bytes_out: Arc<AtomicU64>,
) {
    let mut buf = [0u8; READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &buf[..n];
                bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                // Lock-order discipline: ring before subscribers, same
                // order `attach` uses, to keep snapshot+push atomic.
                let mut rb = ring.lock().expect("ring poisoned");
                rb.push(bytes);
                drop(rb);
                fan_out(
                    &subscribers,
                    Notification::Output {
                        pty_id: pty_id.clone(),
                        bytes: bytes.to_vec(),
                    },
                );
            }
            Err(e) => {
                tracing::debug!(?e, pty_id, "reader EOF/err");
                break;
            }
        }
    }
    let code = child.wait().ok().map(|s| s.exit_code() as i32);
    // Release-ordered so the corresponding Acquire load in `close`
    // observes the flag flip without sequencing the fan_out below.
    child_exited.store(true, Ordering::Release);
    fan_out(
        &subscribers,
        Notification::Exit {
            pty_id: pty_id.clone(),
            code,
        },
    );
}

fn fan_out(subscribers: &Arc<Mutex<Vec<Sender<Notification>>>>, notif: Notification) {
    let mut subs = subscribers.lock().expect("subs poisoned");
    // Drop subscribers whose receiver has been dropped. For "full"
    // we keep the subscriber but discard the message — back-pressure
    // policy from the plan: the replay buffer is the source of truth
    // for slow clients.
    subs.retain(|tx| match tx.try_send(notif.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!("subscriber queue full; dropping notification");
            true
        }
        Err(TrySendError::Closed(_)) => false,
    });
}
