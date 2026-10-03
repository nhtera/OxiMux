//! The indexer thread: owns the writer connection, runs a pass on a timer
//! (sooner while a backlog remains), and publishes progress counters.
//!
//! Each pass lists every transcript (a stat per file — cheap next to a
//! read), decides per file from its watermark, and reads newest first under a
//! time budget so a first index of a large history spreads over many passes
//! instead of pinning a core. Deleted transcripts are retired on the periodic
//! full sweep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use super::ingest::{self, ReadEnd};
use super::watermark::{self, ReadPlan};
use super::{IndexSources, db, writer};

/// Idle cadence between passes.
const PASS_INTERVAL: Duration = Duration::from_secs(20);
/// Pause between passes while files are still waiting to be read.
const BACKLOG_PAUSE: Duration = Duration::from_secs(2);
/// Read time per pass.
const PASS_BUDGET: Duration = Duration::from_secs(5);
/// How often deleted transcripts are looked for.
const FULL_SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);
/// Deleted transcripts retired per sweep.
const RETIRE_PER_SWEEP: usize = 512;
/// Minimum gap between counter refreshes mid-pass.
const STATUS_EVERY: Duration = Duration::from_secs(1);

/// What the index holds and what the indexer is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStatus {
    /// Sessions with at least one indexed turn.
    pub sessions: u64,
    pub messages: u64,
    /// A pass is reading files right now.
    pub indexing: bool,
    /// Files read so far in the current pass, of `files_total` that needed it.
    pub files_done: u64,
    pub files_total: u64,
    /// Database + WAL bytes on disk.
    pub db_bytes: u64,
    /// The last pass or open failed; the indexer retries on its cadence.
    pub error: Option<String>,
}

enum Msg {
    Kick,
    Stop,
}

/// Starts the indexer thread.
pub struct SessionIndexer;

impl SessionIndexer {
    /// Spawn the indexer. Never blocks the caller: opening (or rebuilding) the
    /// database happens on the new thread, which first waits for `previous`
    /// (a just-stopped indexer) to exit, so there is never more than one
    /// writer.
    pub fn start(sources: IndexSources, db_path: PathBuf, previous: Option<IndexerHandle>) -> IndexerHandle {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(IndexStatus::default()));
        let thread = {
            let (stop, status) = (stop.clone(), status.clone());
            std::thread::Builder::new()
                .name("session-search-indexer".into())
                .spawn(move || {
                    if let Some(p) = previous {
                        p.stop();
                    }
                    lower_thread_priority();
                    let mut worker = Worker::new(sources, db_path, stop, status);
                    loop {
                        let backlog = worker.pass();
                        if worker.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match rx.recv_timeout(if backlog { BACKLOG_PAUSE } else { PASS_INTERVAL }) {
                            Ok(Msg::Kick) | Err(RecvTimeoutError::Timeout) => {}
                            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .ok()
        };
        IndexerHandle { tx, stop, status, thread }
    }
}

/// Owner of a running indexer. Dropping it signals the thread to stop
/// without waiting; [`IndexerHandle::stop`] also joins.
pub struct IndexerHandle {
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<IndexStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl IndexerHandle {
    pub fn status(&self) -> IndexStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Run a pass now instead of waiting for the timer.
    pub fn kick(&self) {
        let _ = self.tx.send(Msg::Kick);
    }

    /// Stop the thread and wait for it. The thread checks the stop flag every
    /// few hundred lines or few MB read and between files, so this returns
    /// after at most one bounded batch commit. Never call it on the UI thread:
    /// hand the handle to the next [`SessionIndexer::start`] or to a
    /// background task instead.
    pub fn stop(mut self) {
        self.signal_stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Ask the thread to stop without waiting for it.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Stop);
    }
}

impl Drop for IndexerHandle {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

/// The thread's state; also driven directly (one pass at a time) by tests.
pub(super) struct Worker {
    sources: IndexSources,
    db_path: PathBuf,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) status: Arc<Mutex<IndexStatus>>,
    conn: Option<Connection>,
    pub(super) last_sweep: Option<Instant>,
}

impl Worker {
    pub(super) fn new(
        sources: IndexSources,
        db_path: PathBuf,
        stop: Arc<AtomicBool>,
        status: Arc<Mutex<IndexStatus>>,
    ) -> Self {
        Self { sources, db_path, stop, status, conn: None, last_sweep: None }
    }

    /// One pass; `true` when files are still waiting (budget ran out).
    pub(super) fn pass(&mut self) -> bool {
        if self.conn.is_none() {
            match db::open_writer(&self.db_path) {
                Ok(c) => self.conn = Some(c),
                Err(e) => {
                    self.set(|s| s.error = Some(format!("{e:#}")));
                    return false;
                }
            }
        }
        let result = self.run_pass();
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        self.refresh_counts(false);
        self.set(|s| {
            s.indexing = false;
            s.error = error;
        });
        result.unwrap_or(false)
    }

    fn run_pass(&mut self) -> Result<bool> {
        let Some(conn) = self.conn.as_mut() else { return Ok(false) };
        let found = ingest::discover(&self.sources);
        if found.complete && self.last_sweep.is_none_or(|t| t.elapsed() >= FULL_SWEEP_EVERY) {
            let present: HashSet<String> = found.files.iter().map(|f| f.key()).collect();
            writer::retire_missing(conn, &present, RETIRE_PER_SWEEP)?;
            self.last_sweep = Some(Instant::now());
        }
        let mut pending = Vec::new();
        for f in found.files {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let prev = writer::load_watermark(conn, &f.key())?;
            let plan = watermark::decide(prev.as_ref(), f.stat, |o| ingest::newline_before(&f.path, o));
            if !matches!(plan, ReadPlan::Skip | ReadPlan::HeldOut) {
                pending.push((f, plan, prev));
            }
        }
        if pending.is_empty() {
            return Ok(false);
        }
        let total = pending.len() as u64;
        self.set(|s| {
            s.indexing = true;
            s.files_done = 0;
            s.files_total = total;
        });
        let deadline = Instant::now() + PASS_BUDGET;
        let mut last_status = Instant::now();
        for (done, (f, plan, prev)) in pending.into_iter().enumerate() {
            if self.stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(true);
            }
            let Some(conn) = self.conn.as_mut() else { return Ok(false) };
            match ingest::index_file(conn, &f, plan, prev.as_ref(), &self.stop, deadline) {
                Ok(ReadEnd::Done) => {}
                Ok(ReadEnd::Paused) => return Ok(true),
                Err(e) => {
                    tracing::debug!(path = %f.path.display(), error = %e, "session index: file read failed");
                    let _ = writer::record_failure(conn, &f.key(), f.agent, f.stat.mtime_ms);
                }
            }
            self.set(|s| s.files_done = done as u64 + 1);
            if last_status.elapsed() >= STATUS_EVERY {
                self.refresh_counts(true);
                last_status = Instant::now();
            }
        }
        Ok(false)
    }

    fn refresh_counts(&self, indexing: bool) {
        let Some(conn) = self.conn.as_ref() else { return };
        let Ok((sessions, messages)) = counts(conn) else { return };
        let db_bytes = db::index_size_bytes(&self.db_path);
        self.set(|s| {
            s.sessions = sessions;
            s.messages = messages;
            s.db_bytes = db_bytes;
            s.indexing = indexing;
        });
    }

    fn set(&self, f: impl FnOnce(&mut IndexStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }
}

/// `(sessions with turns, indexed rows)`.
pub fn counts(conn: &Connection) -> Result<(u64, u64)> {
    let sessions: i64 = conn.query_row("SELECT count(*) FROM sessions WHERE message_count > 0", [], |r| r.get(0))?;
    let messages: i64 = conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?;
    Ok((sessions as u64, messages as u64))
}

/// Status of an index file without a running indexer (settings pane while
/// the feature is off, or before the thread's first pass lands).
pub fn read_status(db_path: &Path) -> IndexStatus {
    let mut status = IndexStatus { db_bytes: db::index_size_bytes(db_path), ..Default::default() };
    if let Ok((sessions, messages)) = db::open_reader(db_path).and_then(|c| counts(&c)) {
        status.sessions = sessions;
        status.messages = messages;
    }
    status
}

/// Background work: let the scheduler favour everything else.
fn lower_thread_priority() {
    #[cfg(target_os = "macos")]
    // SAFETY: sets the calling thread's own QoS class; no pointers involved.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}
