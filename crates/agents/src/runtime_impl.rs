//! `CliRuntime` — the concrete `AgentRuntime` for any `CliAgentAdapter`.
//!
//! Architecture:
//! - One `CliRuntime` per app. Holds an adapter registry keyed by
//!   `AgentAdapter` and a session table keyed by `AgentSessionId`.
//! - Each session owns its own `PortablePtyBackend` (one PTY per session)
//!   plus a tokio poll task that drains PTY events at 50 ms and feeds them
//!   into a per-session `StatusMachine`. Status transitions are published
//!   on a `tokio::sync::watch` channel that the UI can subscribe to.
//! - Cancel = SIGTERM (process group) → 5 s grace → SIGKILL fallback, all
//!   inside `PortablePtyBackend::close()`. The runtime simply calls
//!   `backend.close(term_id)` and trusts the backend to reap zombie-free.
//!
//! This file is the forcing function for the Phase 3 trait surface — the
//! first slice that turns `AgentRuntime` + `CliAgentAdapter` from
//! contracts into something the app can actually run.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use oximux_core::{AgentAdapter, AgentSessionId, AgentSnapshot, AgentStatus};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalSessionId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::cli::CliAgentAdapter;
use crate::osc_sideband::AgentOscScanner;
use crate::poll_helpers::process_poll_events;
use crate::runtime::{AgentRuntime, AgentSessionConfig, AgentStatusStream};
use crate::status_machine::StatusMachine;

/// How often the poll task drains PTY events and ticks the status machine.
/// 50 ms balances UI latency (badge feels real-time) against syscall load
/// when many panes idle. `MissedTickBehavior::Skip` keeps drift bounded if
/// the executor blocks for longer than one tick.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Upper bound on how long `cancel()` waits for the poll task to publish
/// the terminal status after `close()` triggers the Exit event. The poll
/// task ticks every `POLL_INTERVAL`, so 1 s is roomy without being so
/// large that a stuck task wedges the UI.
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Soft cap on `CommandSpec::stdin_seed` length. Writes larger than the
/// kernel PTY buffer (~4-16 KiB on macOS) can block until the child
/// reads. We warn at this threshold so a misbehaving adapter shows up in
/// logs rather than silently stalling a spawn_blocking thread.
const MAX_SAFE_STDIN_SEED: usize = 4096;

/// `Box<dyn TerminalBackend>` behind an `Arc<Mutex<…>>` so the runtime
/// methods (`send_message`, `cancel`) and the per-session poll task can
/// each touch the backend without ownership games. Locks are held only
/// for the duration of a single non-blocking call.
///
/// Public so the app's terminal renderer can hold one and read PTY output
/// from either a locally-spawned shell OR an agent session — same render
/// path, no enum split. Callers MUST NOT block under the lock; the only
/// safe ops are the non-blocking `TerminalBackend` methods
/// (`drain_events`, `write`, `resize`).
pub type SharedBackend = Arc<Mutex<Box<dyn TerminalBackend>>>;

struct SessionEntry {
    backend: SharedBackend,
    term_id: TerminalSessionId,
    /// Kept so `subscribe_status` can clone — the corresponding `Sender`
    /// lives inside the poll task and survives until terminal-state exit.
    status_rx: watch::Receiver<AgentSnapshot>,
    poll_handle: JoinHandle<()>,
    /// Set by `cancel()` before the PTY is closed so the poll task can
    /// classify the resulting Exit event as user-initiated (`Interrupted`)
    /// rather than a process failure (`Done { code: None }` / `Failed`).
    cancel_requested: Arc<AtomicBool>,
}

struct Inner {
    adapters: HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>,
    sessions: HashMap<AgentSessionId, SessionEntry>,
    next_id: u64,
    /// When set, agent PTYs are spawned through this shared backend (the
    /// out-of-process relay daemon) instead of a private in-process PTY.
    /// Daemon-owned PTYs outlive the app, so an agent tab can re-attach to
    /// its still-running CLI on the next launch — the same survival path
    /// plain terminals already use. `None` (no relay) falls back to a
    /// private `PortablePtyBackend` that dies with the app.
    shared_backend: Option<SharedBackend>,
}

/// The CLI-agent runtime. `clone()` is cheap (just an `Arc` bump); the
/// app holds one and may share it across UI handlers.
#[derive(Clone)]
pub struct CliRuntime {
    inner: Arc<Mutex<Inner>>,
}

impl CliRuntime {
    /// Empty runtime. Adapters are registered via `register_adapter` before
    /// `start_session` calls land. Construction is split from registration
    /// so the app can wire adapters from settings without a panic on a
    /// missing one.
    pub fn new() -> Self {
        Self::with_shared_backend(None)
    }

    /// Build a runtime that spawns agent PTYs through `shared_backend` (the
    /// relay daemon) when `Some`. The app passes the process-wide relay
    /// backend so agent sessions survive app restarts and can re-attach.
    /// `None` keeps the legacy per-session in-process PTY behavior.
    pub fn with_shared_backend(shared_backend: Option<SharedBackend>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                adapters: HashMap::new(),
                sessions: HashMap::new(),
                next_id: 1,
                shared_backend,
            })),
        }
    }

    /// Adopt a PTY that is already alive on a backend (a relay session
    /// re-attached via `attach_existing`) as a fresh agent session: wire up
    /// the status machine + poll loop without spawning a new child. Used by
    /// the restore path to reconnect a tab to its still-running CLI. Errors
    /// when no adapter is registered for `adapter_key` (needed for status
    /// patterns).
    pub fn adopt_session(
        &self,
        adapter_key: AgentAdapter,
        backend: SharedBackend,
        term_id: TerminalSessionId,
    ) -> Result<AgentSessionId> {
        let adapter = {
            let inner = lock_recover(&self.inner, "CliRuntime sessions");
            inner
                .adapters
                .get(&adapter_key)
                .cloned()
                .ok_or_else(|| anyhow!("no adapter registered for {:?}", adapter_key))?
        };
        self.register_session(adapter, backend, term_id)
    }

    /// Wire up the status machine + poll task for a ready `(backend,
    /// term_id)` and record the session. Shared by the spawn path
    /// (`start_session`) and the re-attach path (`adopt_session`).
    fn register_session(
        &self,
        adapter: Arc<dyn CliAgentAdapter>,
        backend: SharedBackend,
        term_id: TerminalSessionId,
    ) -> Result<AgentSessionId> {
        {
            let mut be = lock_recover(&backend, "terminal backend");
            be.subscribe_status_events(term_id)?;
        }
        let patterns: Arc<[_]> = adapter.status_patterns().to_vec().into();
        let machine = StatusMachine::new(patterns);
        let (status_tx, status_rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let poll_handle = tokio::spawn(poll_loop(
            backend.clone(),
            term_id,
            machine,
            status_tx,
            cancel_requested.clone(),
        ));
        let mut inner = lock_recover(&self.inner, "CliRuntime sessions");
        let id = AgentSessionId::new(inner.next_id);
        inner.next_id = inner.next_id.saturating_add(1);
        inner.sessions.insert(
            id,
            SessionEntry {
                backend,
                term_id,
                status_rx,
                poll_handle,
                cancel_requested,
            },
        );
        Ok(id)
    }

    /// Register one adapter under its `AgentAdapter` enum. Last-write-wins
    /// if called twice for the same enum (only relevant for tests that
    /// swap in a mock).
    pub fn register_adapter(&self, key: AgentAdapter, adapter: Arc<dyn CliAgentAdapter>) {
        let mut inner = lock_recover(&self.inner, "CliRuntime sessions");
        inner.adapters.insert(key, adapter);
    }

    /// Hand out a shared handle on the session's PTY backend so the app's
    /// terminal renderer can drain output and write input without going
    /// through `send_message`. The same `Arc` the poll task holds — both
    /// callers compete on the same mutex, which is fine because the only
    /// supported ops on the trait (`drain_events`, `write`, `resize`) are
    /// non-blocking. Errors when the session is unknown (already cancelled
    /// or never started).
    pub fn backend_for(&self, id: AgentSessionId) -> Result<SharedBackend> {
        let inner = lock_recover(&self.inner, "CliRuntime sessions");
        inner
            .sessions
            .get(&id)
            .map(|entry| entry.backend.clone())
            .ok_or_else(|| anyhow!("unknown session {:?}", id))
    }

    /// The `TerminalSessionId` the underlying PTY was assigned at spawn
    /// time. The renderer needs this to filter `TerminalEvent`s coming out
    /// of the shared backend (each backend can serve multiple sessions in
    /// principle — `oximux-pty` does not enforce one-id-per-backend).
    pub fn terminal_session_id(&self, id: AgentSessionId) -> Result<TerminalSessionId> {
        let inner = lock_recover(&self.inner, "CliRuntime sessions");
        inner
            .sessions
            .get(&id)
            .map(|entry| entry.term_id)
            .ok_or_else(|| anyhow!("unknown session {:?}", id))
    }
}

impl Default for CliRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentRuntime for CliRuntime {
    async fn start_session(&self, cfg: AgentSessionConfig) -> Result<AgentSessionId> {
        let adapter = {
            let inner = lock_recover(&self.inner, "CliRuntime sessions");
            inner
                .adapters
                .get(&cfg.adapter)
                .cloned()
                .ok_or_else(|| anyhow!("no adapter registered for {:?}", cfg.adapter))?
        };

        let spec = adapter.build_command(&cfg)?;

        // Merge session env onto adapter env. Adapter env wins (per-CLI
        // hardening like `*_DISABLE_TELEMETRY` should not be overridden by
        // a stray session-level override; if that policy ever flips, swap
        // the iter order here and document it).
        let mut env = cfg.env.clone();
        env.extend(spec.env.iter().cloned());
        let spawn_cfg = SpawnConfig {
            shell: spec.program.to_string_lossy().into_owned(),
            args: spec.args.clone(),
            cwd: cfg.worktree_path.clone(),
            env,
            cols: cfg.cols,
            rows: cfg.rows,
            scrollback: 5000,
            capture_status_events: true,
        };

        // Spawn PTY + stdin_seed inside spawn_blocking — openpty + fork on
        // macOS can briefly block the kernel, and we don't want to stall
        // the tokio executor.
        //
        // H3 (review 260520-1448): `be.write` here is a synchronous
        // `write_all` into the PTY. If `stdin_seed` exceeds the kernel PTY
        // buffer (~4-16 KiB) AND the child has not yet started reading,
        // the call blocks the spawn_blocking thread. Adapters MUST keep
        // seeds short (the common case is one prompt line). Enforced as
        // a warn-level guard; flip to an error if a real abuse appears.
        let stdin_seed = spec.stdin_seed.clone();
        if let Some(seed) = &stdin_seed
            && seed.len() > MAX_SAFE_STDIN_SEED
        {
            tracing::warn!(
                seed_len = seed.len(),
                max = MAX_SAFE_STDIN_SEED,
                "stdin_seed exceeds safe size; spawn may block on PTY buffer"
            );
        }
        let shared = {
            lock_recover(&self.inner, "CliRuntime sessions")
                .shared_backend
                .clone()
        };

        let (backend, term_id): (SharedBackend, TerminalSessionId) = if let Some(shared) = shared {
            // Relay path. The v5 Spawn RPC carries argv, so the agent binary
            // is spawned DIRECTLY as the PTY's foreground process — its own
            // flags ride along, nothing wraps it, and the terminal shows only
            // the agent's banner (no echoed command line). An absolute path is
            // required because the detached daemon's PATH may not include the
            // agent (resolved here via the app's PATH, which already located it
            // at detection time).
            //
            // Fallback: if abs-path resolution fails, spawn the login shell and
            // `exec` the full command into it via a launch line — the shell
            // resolves PATH from the user's profile and carries argv. Either
            // way the agent ends up as the PTY leaf (so cancel's process-group
            // SIGTERM and exit→EOF status both reach it), and the daemon owns
            // the PTY so it survives an app restart and re-attaches on launch.
            // A stdin prompt seed (aider) is written after spawn in both cases.
            let direct_program = resolve_program_abs(&spec.program).await;
            let relay_cfg = SpawnConfig {
                shell: direct_program
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(wrapper_shell),
                args: if direct_program.is_some() {
                    spec.args.clone()
                } else {
                    Vec::new()
                },
                ..spawn_cfg.clone()
            };
            // Direct spawn → the binary IS the process, no launch line to
            // write. Wrapper fallback → write the `exec <program> <args…>` line.
            let launch = direct_program
                .is_none()
                .then(|| build_launch_line(&spec.program, &spec.args));
            let shared_for_spawn = shared.clone();
            let term_id = tokio::task::spawn_blocking(move || -> Result<TerminalSessionId> {
                let mut be = lock_recover(&shared_for_spawn, "terminal backend");
                let id = be.spawn(relay_cfg)?;
                if let Some(launch) = launch {
                    be.write(id, launch.as_bytes())?;
                }
                if let Some(seed) = stdin_seed {
                    be.write(id, &seed)?;
                }
                Ok(id)
            })
            .await??;
            (shared, term_id)
        } else {
            let (backend_box, term_id) = tokio::task::spawn_blocking(
                move || -> Result<(Box<dyn TerminalBackend>, TerminalSessionId)> {
                    let mut be: Box<dyn TerminalBackend> = Box::new(PortablePtyBackend::new());
                    let id = be.spawn(spawn_cfg)?;
                    if let Some(seed) = stdin_seed {
                        be.write(id, &seed)?;
                    }
                    Ok((be, id))
                },
            )
            .await??;
            (Arc::new(Mutex::new(backend_box)), term_id)
        };

        self.register_session(adapter, backend, term_id)
    }

    async fn send_message(&self, id: AgentSessionId, msg: &str) -> Result<()> {
        let (backend, term_id) = {
            let inner = lock_recover(&self.inner, "CliRuntime sessions");
            let entry = inner
                .sessions
                .get(&id)
                .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
            (entry.backend.clone(), entry.term_id)
        };
        let bytes = msg.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut be = lock_recover(&backend, "terminal backend");
            be.write(term_id, &bytes)
        })
        .await??;
        Ok(())
    }

    async fn cancel(&self, id: AgentSessionId) -> Result<()> {
        let entry = {
            let mut inner = lock_recover(&self.inner, "CliRuntime sessions");
            inner
                .sessions
                .remove(&id)
                .ok_or_else(|| anyhow!("unknown session {:?}", id))?
        };
        let backend = entry.backend.clone();
        let term_id = entry.term_id;
        // Flag BEFORE close() so the poll task is guaranteed to see the
        // cancel when the Exit event arrives — the exit cannot precede the
        // close that causes it.
        entry.cancel_requested.store(true, Ordering::SeqCst);
        // close() joins the watcher thread → fully reaps the child; do it
        // off the async runtime.
        //
        // C1 (review 260520-1448): drain_events() before close() so the
        // watcher thread is never blocked on a full sync_channel(256) when
        // close() tries to join it. Under sustained heavy output (`yes`
        // command, verbose compile) the channel can fill in <50 ms, the
        // poll task can't acquire backend.lock() because cancel holds it,
        // and join() deadlocks waiting for a watcher that's blocked on
        // tx.send(). Draining unblocks the sender path before join.
        tokio::task::spawn_blocking(move || {
            let mut be = lock_recover(&backend, "terminal backend");
            let _ = be.drain_events();
            let _ = be.close(term_id);
        })
        .await
        .ok();
        // Wait briefly for the poll task to drain the Exit event and
        // publish the terminal status. If it does not exit in time, abort
        // it (H1, review 260520-1448) — dropping the JoinHandle alone only
        // detaches the task; we want it gone.
        let mut handle = entry.poll_handle;
        tokio::select! {
            _ = &mut handle => {}
            _ = tokio::time::sleep(CANCEL_DRAIN_TIMEOUT) => {
                handle.abort();
                tracing::warn!(?id, "poll task did not exit within CANCEL_DRAIN_TIMEOUT; aborted");
            }
        }
        let mut be = lock_recover(&entry.backend, "terminal backend");
        be.unsubscribe_status_events(term_id);
        Ok(())
    }

    fn subscribe_status(&self, id: AgentSessionId) -> Result<AgentStatusStream> {
        let inner = lock_recover(&self.inner, "CliRuntime sessions");
        let entry = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
        Ok(entry.status_rx.clone())
    }

    fn current_status(&self, id: AgentSessionId) -> Result<AgentStatus> {
        let inner = lock_recover(&self.inner, "CliRuntime sessions");
        let entry = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
        Ok(entry.status_rx.borrow().status.clone())
    }
}

/// The login shell used to wrap a relay-spawned agent. The relay daemon's
/// Spawn RPC takes only a shell (no argv), so we run the user's shell and
/// `exec` the agent into it. Falls back to zsh when `$SHELL` is unset.
fn wrapper_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}

/// Resolve a program (possibly a bare name like `claude`) to an absolute
/// path using the *app* process's PATH — the same PATH that located the
/// binary at detection time. Returns `None` when the program isn't a clean
/// absolute path we can hand to the detached daemon to `exec` directly; the
/// caller then falls back to the login-shell wrapper, which resolves PATH
/// from the user's profile.
async fn resolve_program_abs(program: &std::path::Path) -> Option<std::path::PathBuf> {
    if program.is_absolute() {
        return Some(program.to_path_buf());
    }
    let output = tokio::process::Command::new("which")
        .arg(program.as_os_str())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let resolved = std::path::PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    resolved.is_absolute().then_some(resolved)
}

/// Build the one-line `exec <program> <args…>` command written into the
/// wrapper shell's stdin. Every token is POSIX-quoted so paths/args with
/// spaces or shell metacharacters survive intact.
fn build_launch_line(program: &std::path::Path, args: &[String]) -> String {
    let mut line = String::from("exec ");
    line.push_str(&shell_quote(&program.to_string_lossy()));
    for a in args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    line.push('\n');
    line
}

/// Minimal POSIX shell quoting. Bare-word when the token is all "safe"
/// characters; otherwise single-quote and escape embedded single quotes
/// as `'\''`. Sufficient for argv tokens (no need to handle newlines).
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b',')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Per-session poll loop. Owns the `StatusMachine` for its session — the
/// machine never crosses task boundaries so no Mutex is needed around it.
/// Exits when the PTY emits `Exit` (and the terminal transition has been
/// published) or when all status subscribers drop (the watch sender's
/// `send` returns Err once the last receiver is gone — but we also hold
/// one Receiver in `SessionEntry` so that case only fires after the
/// session has been removed from the table).
async fn poll_loop(
    backend: SharedBackend,
    term_id: TerminalSessionId,
    mut machine: StatusMachine,
    status_tx: watch::Sender<AgentSnapshot>,
    cancel_requested: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // One scanner per session — stateful so an OSC-9999 sequence split across
    // two PTY reads still parses on the next drain.
    let mut scanner = AgentOscScanner::new();
    loop {
        interval.tick().await;
        let events = {
            let mut be = lock_recover(&backend, "terminal backend");
            be.drain_status_events_for(term_id)
        };
        let saw_exit = process_poll_events(
            events,
            term_id,
            &mut machine,
            &mut scanner,
            &status_tx,
            &cancel_requested,
            Instant::now(),
        );
        if saw_exit {
            break;
        }
        // If every subscriber dropped (session removed and UI gone), the
        // sender's `is_closed` flips to true.
        if status_tx.is_closed() {
            break;
        }
    }
    let mut be = lock_recover(&backend, "terminal backend");
    be.unsubscribe_status_events(term_id);
}

/// Lock a mutex, recovering from poison instead of propagating the
/// panic. The guarded values (session table, adapter registry, terminal
/// backend) are per-session islands that stay usable after another
/// thread panicked mid-access — recovering keeps every other agent
/// operation working, whereas propagating would cascade one failed
/// agent future into whole-runtime death.
fn lock_recover<'a, T: ?Sized>(
    m: &'a Mutex<T>,
    what: &'static str,
) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::error!(what, "mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

#[cfg(test)]
#[path = "runtime_impl_tests.rs"]
mod tests;
