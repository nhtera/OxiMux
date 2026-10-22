//! The one thread that writes to a PTY's master.
//!
//! Client input used to be written under a mutex by whichever task handled the
//! `Write` request, and a `write_all` blocks for as long as the program is not
//! reading its input. That was tolerable while only clients wrote. The reader
//! now has to write too — the answer to a program-status query — and a reader
//! that blocks on a full input buffer stops draining output, so a program
//! blocked writing output never gets to read its input: a deadlock.
//!
//! So writes go through a queue to a dedicated thread. Client input keeps its
//! old contract — the caller blocks until its bytes are written, and writes land
//! whole and in order — while a reply is enqueued without waiting and dropped if
//! the queue is full.

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::Instant;

use anyhow::{Context, anyhow};

use crate::program_status_query::{PROGRAM_STATUS_REPLY, ReplyTicket};

/// Pending writes per PTY. Client input waits for room; a reply is dropped.
const QUEUE_DEPTH: usize = 64;

enum Job {
    /// Client input; the result goes back on `done`.
    Input {
        bytes: Vec<u8>,
        done: SyncSender<std::io::Result<()>>,
    },
    /// A program-status reply; `ticket` learns when it was written.
    Reply(Arc<ReplyTicket>),
}

/// Handle to a PTY's writer thread. The thread ends once every handle is gone.
#[derive(Clone)]
pub(crate) struct PtyWriter {
    tx: SyncSender<Job>,
}

impl PtyWriter {
    pub(crate) fn spawn(pty_id: &str, mut writer: Box<dyn Write + Send>) -> anyhow::Result<Self> {
        let (tx, rx) = sync_channel::<Job>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name(format!("relay-pty-w-{pty_id}"))
            .spawn(move || {
                for job in rx {
                    match job {
                        Job::Input { bytes, done } => {
                            let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                            let _ = done.send(result);
                        }
                        Job::Reply(ticket) => {
                            let result = writer.write_all(PROGRAM_STATUS_REPLY).and_then(|()| writer.flush());
                            if let Err(e) = result {
                                tracing::debug!(?e, "program-status reply write failed");
                            }
                            ticket.mark_written(Instant::now());
                        }
                    }
                }
            })
            .context("spawn writer thread")?;
        Ok(Self { tx })
    }

    /// Write client input, blocking until it reached the PTY (or failed).
    pub(crate) fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let (done, result) = sync_channel(1);
        self.tx
            .send(Job::Input {
                bytes: bytes.to_vec(),
                done,
            })
            .map_err(|_| anyhow!("pty writer gone"))?;
        result
            .recv()
            .map_err(|_| anyhow!("pty writer gone"))?
            .context("pty write")
    }

    /// Queue a program-status reply without waiting. `false` when the queue is
    /// full or the writer is gone; the reply is then dropped.
    pub(crate) fn try_reply(&self, ticket: &Arc<ReplyTicket>) -> bool {
        match self.tx.try_send(Job::Reply(Arc::clone(ticket))) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A `Write` that records everything, in order.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn input_is_written_whole_and_in_order_around_replies() {
        let sink = Sink::default();
        let writer = PtyWriter::spawn("t", Box::new(sink.clone())).unwrap();
        let ticket = Arc::new(ReplyTicket::default());
        writer.write(b"one").unwrap();
        assert!(writer.try_reply(&ticket));
        writer.write(b"two").unwrap();
        let mut want = b"one".to_vec();
        want.extend_from_slice(PROGRAM_STATUS_REPLY);
        want.extend_from_slice(b"two");
        assert_eq!(*sink.0.lock().unwrap(), want);
    }

    /// A program that never reads: its input buffer is full, so every write
    /// blocks. A reply must still return at once, and drop once the queue fills.
    #[test]
    fn a_reply_never_waits_on_a_blocked_writer() {
        struct Stuck(std::sync::mpsc::Receiver<()>);
        impl Write for Stuck {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (release, stuck) = std::sync::mpsc::channel();
        let writer = PtyWriter::spawn("t", Box::new(Stuck(stuck))).unwrap();
        let ticket = Arc::new(ReplyTicket::default());
        let started = Instant::now();
        let accepted = (0..QUEUE_DEPTH * 2).filter(|_| writer.try_reply(&ticket)).count();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(accepted <= QUEUE_DEPTH + 1, "accepted {accepted}");
        drop(release);
    }
}
