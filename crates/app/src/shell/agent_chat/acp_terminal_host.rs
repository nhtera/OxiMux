//! App-side backing for ACP embedded terminals — the concrete
//! [`AcpTerminalHost`] the domain crate delegates its `terminal/*` handlers to.
//!
//! An ACP agent asks the client (us) to spawn a command, poll its output, wait
//! for exit, and kill/release it, then embeds the live terminal inline in a tool
//! card. This host serves those by spawning a real PTY through the app's own
//! terminal stack (relay daemon, in-process fallback) via
//! [`spawn_embedded_command`], so the same widget/scrollback/exit machinery that
//! powers every other terminal renders the embedded one — no second engine.
//!
//! Each terminal runs a small watcher thread that drains the PTY's independent
//! status-event stream (`subscribe_status_events`) into a byte ring + exit
//! latch, so `terminal/output`/`terminal/wait_for_exit` answer without touching
//! the renderer's event queue (the mounted `TerminalView` drains that
//! separately). The host is installed once at boot; the chat view reads
//! [`embedded_terminal_backend`] to mount the inline `TerminalView` and calls
//! [`release_embedded`] to reap on tab close.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use futures::channel::oneshot;
use oximux_agents::SharedBackend;
use oximux_agents::thread::acp::{
    AcpTerminalHost, TerminalExitLite, TerminalOutputLite, TerminalSpawnSpec,
};
use oximux_pty::{TerminalEvent, TerminalSessionId};

use crate::shell::terminal_view::spawn_embedded_command;

/// Default retained-output cap when the agent doesn't set `outputByteLimit`.
/// Generous enough for a build log; the ring truncates from the front past it.
const DEFAULT_BYTE_LIMIT: usize = 1 << 20; // 1 MiB
/// Watcher poll cadence for the status-event stream. Short enough to feel live,
/// long enough that a handful of embedded terminals don't thrash the shared
/// relay backend mutex.
const WATCH_INTERVAL: Duration = Duration::from_millis(60);

/// The output ring + exit latch a terminal's watcher thread updates and the
/// `terminal/output`/`wait_for_exit` handlers read.
struct Observation {
    /// Raw bytes captured so far, truncated from the front to `byte_limit`.
    output: Vec<u8>,
    /// Whether front-truncation has dropped any bytes.
    truncated: bool,
    /// Set once the command exits; makes further `wait_for_exit` resolve at once.
    exit: Option<TerminalExitLite>,
    /// Parked `wait_for_exit` senders, fired when `exit` is first set.
    waiters: Vec<oneshot::Sender<TerminalExitLite>>,
    byte_limit: usize,
}

/// One live embedded terminal: the PTY it renders on + its observation state.
struct EmbeddedTerminal {
    backend: SharedBackend,
    session_id: TerminalSessionId,
    obs: Arc<Mutex<Observation>>,
    /// Signals the watcher thread to stop (set on `release`).
    stop: Arc<AtomicBool>,
    _watcher: JoinHandle<()>,
}

/// The process-wide embedded-terminal host. Terminals are keyed by the
/// client-minted id (globally unique, so one registry serves every ACP session).
#[derive(Default)]
pub struct EmbeddedTerminalHost {
    next_id: AtomicU64,
    terminals: Mutex<HashMap<String, EmbeddedTerminal>>,
}

impl EmbeddedTerminalHost {
    /// The backend + session for a terminal, so the chat view can mount a
    /// `TerminalView` on the same PTY. `None` once released / never created.
    fn backend_for(&self, terminal_id: &str) -> Option<(SharedBackend, TerminalSessionId)> {
        let terminals = self.terminals.lock().unwrap_or_else(|p| p.into_inner());
        terminals.get(terminal_id).map(|t| (t.backend.clone(), t.session_id))
    }
}

impl AcpTerminalHost for EmbeddedTerminalHost {
    fn create(&self, spec: TerminalSpawnSpec) -> Result<String, String> {
        let cwd = spec
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let (backend, session_id) = spawn_embedded_command(spec.command, spec.args, cwd, spec.env)
            .ok_or_else(|| "failed to spawn embedded terminal".to_string())?;
        // Enable the independent Output/Exit stream this host observes (the
        // renderer keeps its own queue untouched).
        {
            let mut guard = backend.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .subscribe_status_events(session_id)
                .map_err(|e| format!("subscribe status events: {e}"))?;
        }
        let byte_limit = spec.output_byte_limit.map(|b| b as usize).unwrap_or(DEFAULT_BYTE_LIMIT);
        let obs = Arc::new(Mutex::new(Observation {
            output: Vec::new(),
            truncated: false,
            exit: None,
            waiters: Vec::new(),
            byte_limit,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = spawn_watcher(backend.clone(), session_id, obs.clone(), stop.clone());
        let id = format!("acp-term-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        self.terminals.lock().unwrap_or_else(|p| p.into_inner()).insert(
            id.clone(),
            EmbeddedTerminal { backend, session_id, obs, stop, _watcher: watcher },
        );
        Ok(id)
    }

    fn output(&self, terminal_id: &str) -> Result<TerminalOutputLite, String> {
        let terminals = self.terminals.lock().unwrap_or_else(|p| p.into_inner());
        let t = terminals.get(terminal_id).ok_or_else(|| format!("unknown terminal {terminal_id}"))?;
        let o = t.obs.lock().unwrap_or_else(|p| p.into_inner());
        Ok(TerminalOutputLite {
            output: String::from_utf8_lossy(&o.output).into_owned(),
            truncated: o.truncated,
            exit_status: o.exit.clone(),
        })
    }

    fn wait_for_exit(&self, terminal_id: &str) -> oneshot::Receiver<TerminalExitLite> {
        let (tx, rx) = oneshot::channel();
        let terminals = self.terminals.lock().unwrap_or_else(|p| p.into_inner());
        match terminals.get(terminal_id) {
            Some(t) => {
                let mut o = t.obs.lock().unwrap_or_else(|p| p.into_inner());
                match o.exit.clone() {
                    // Already exited → resolve immediately.
                    Some(exit) => {
                        let _ = tx.send(exit);
                    }
                    // Still running → the watcher fires this on the exit event.
                    None => o.waiters.push(tx),
                }
            }
            // Unknown id → resolve with a default status rather than hang the turn.
            None => {
                let _ = tx.send(TerminalExitLite::default());
            }
        }
        rx
    }

    fn kill(&self, terminal_id: &str) -> Result<(), String> {
        let terminals = self.terminals.lock().unwrap_or_else(|p| p.into_inner());
        let t = terminals.get(terminal_id).ok_or_else(|| format!("unknown terminal {terminal_id}"))?;
        // Terminate the child but keep the entry valid: output stays readable and
        // the watcher will latch the resulting exit for `wait_for_exit`.
        let mut guard = t.backend.lock().unwrap_or_else(|p| p.into_inner());
        guard.close(t.session_id).map_err(|e| format!("kill terminal: {e}"))
    }

    fn release(&self, terminal_id: &str) -> Result<(), String> {
        let entry = self.terminals.lock().unwrap_or_else(|p| p.into_inner()).remove(terminal_id);
        if let Some(t) = entry {
            t.stop.store(true, Ordering::Relaxed);
            let mut guard = t.backend.lock().unwrap_or_else(|p| p.into_inner());
            guard.unsubscribe_status_events(t.session_id);
            // `close` is idempotent, so this is safe even if the mounted view's
            // Drop also closed the session.
            let _ = guard.close(t.session_id);
        }
        Ok(())
    }
}

/// Drain the terminal's status stream into its observation until it exits or is
/// released. Runs on its own OS thread (few, short-lived), so it never touches
/// the GPUI executor or a tokio runtime — the relay backend's own handle drives
/// the underlying socket.
fn spawn_watcher(
    backend: SharedBackend,
    session_id: TerminalSessionId,
    obs: Arc<Mutex<Observation>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("acp-term-watch".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let events = {
                    let mut guard = backend.lock().unwrap_or_else(|p| p.into_inner());
                    guard.drain_status_events_for(session_id)
                };
                let mut exited: Option<TerminalExitLite> = None;
                for ev in events {
                    match ev {
                        TerminalEvent::Output { bytes, .. } => {
                            let mut o = obs.lock().unwrap_or_else(|p| p.into_inner());
                            o.output.extend_from_slice(&bytes);
                            if o.output.len() > o.byte_limit {
                                let excess = o.output.len() - o.byte_limit;
                                o.output.drain(0..excess);
                                o.truncated = true;
                            }
                        }
                        TerminalEvent::Exit { code, .. } => {
                            // The PTY layer collapses "killed by signal" and
                            // "detached" into `code: None` (no signal name), so a
                            // signal-terminated command surfaces as
                            // `{exit_code: None, signal: None}`. Distinguishing
                            // them would need signal plumbing through `TerminalEvent`.
                            exited = Some(TerminalExitLite {
                                exit_code: code.map(|c| c as u32),
                                signal: None,
                            });
                        }
                        _ => {}
                    }
                }
                if let Some(exit) = exited {
                    let mut o = obs.lock().unwrap_or_else(|p| p.into_inner());
                    o.exit = Some(exit.clone());
                    for tx in o.waiters.drain(..) {
                        let _ = tx.send(exit.clone());
                    }
                    // The command is done and its final output is captured; the
                    // entry stays so `output` keeps answering until release.
                    return;
                }
                thread::sleep(WATCH_INTERVAL);
            }
        })
        .expect("spawn acp terminal watcher thread")
}

/// The installed host, shared between the ACP worker (via the trait object) and
/// the chat view (for mounting + reaping).
static HOST: OnceLock<Arc<EmbeddedTerminalHost>> = OnceLock::new();

/// Install the embedded-terminal host once at app boot: registers it with the
/// ACP layer (so agents can drive terminals) and keeps a handle for the chat
/// view to mount + reap. Idempotent — a second call is ignored.
pub fn install() {
    let host = Arc::new(EmbeddedTerminalHost::default());
    if HOST.set(host.clone()).is_ok() {
        oximux_agents::thread::acp::install_terminal_host(host);
    }
}

/// The backend + session for an ACP terminal id, so the chat view can mount a
/// `TerminalView` on the live PTY. `None` before boot install or after release.
pub fn embedded_terminal_backend(terminal_id: &str) -> Option<(SharedBackend, TerminalSessionId)> {
    HOST.get()?.backend_for(terminal_id)
}

/// Reap an embedded terminal (kill + free) — called when its chat tab closes or
/// its card leaves the transcript. No-op before install / for an unknown id.
pub fn release_embedded(terminal_id: &str) {
    if let Some(host) = HOST.get() {
        let _ = host.release(terminal_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spawning a real PTY needs the app terminal stack, so these focus on the
    // registry/observation logic that doesn't require a live child.

    #[test]
    fn wait_for_exit_on_unknown_id_resolves_default() {
        let host = EmbeddedTerminalHost::default();
        let rx = host.wait_for_exit("nope");
        let exit = futures::executor::block_on(rx).expect("resolved, not cancelled");
        assert_eq!(exit, TerminalExitLite::default());
    }

    #[test]
    fn output_on_unknown_id_errors() {
        let host = EmbeddedTerminalHost::default();
        assert!(host.output("nope").is_err());
    }

    #[test]
    fn release_unknown_id_is_noop() {
        let host = EmbeddedTerminalHost::default();
        assert!(host.release("nope").is_ok());
    }

    #[test]
    fn front_truncation_keeps_the_tail_within_the_byte_limit() {
        // Exercise the ring math the watcher runs, independent of a PTY.
        let obs = Observation {
            output: Vec::new(),
            truncated: false,
            exit: None,
            waiters: Vec::new(),
            byte_limit: 4,
        };
        let obs = Arc::new(Mutex::new(obs));
        let mut o = obs.lock().unwrap();
        for chunk in [b"abc".as_slice(), b"defg".as_slice()] {
            o.output.extend_from_slice(chunk);
            if o.output.len() > o.byte_limit {
                let excess = o.output.len() - o.byte_limit;
                o.output.drain(0..excess);
                o.truncated = true;
            }
        }
        assert_eq!(&o.output, b"defg");
        assert!(o.truncated);
    }
}
