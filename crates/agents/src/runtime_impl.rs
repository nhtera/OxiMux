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
use oximux_core::{AgentAdapter, AgentSessionId, AgentStatus};
use oximux_pty::{
    PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::cli::CliAgentAdapter;
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
    status_rx: watch::Receiver<AgentStatus>,
    poll_handle: JoinHandle<()>,
}

struct Inner {
    adapters: HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>,
    sessions: HashMap<AgentSessionId, SessionEntry>,
    next_id: u64,
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
        Self {
            inner: Arc::new(Mutex::new(Inner {
                adapters: HashMap::new(),
                sessions: HashMap::new(),
                next_id: 1,
            })),
        }
    }

    /// Register one adapter under its `AgentAdapter` enum. Last-write-wins
    /// if called twice for the same enum (only relevant for tests that
    /// swap in a mock).
    pub fn register_adapter(&self, key: AgentAdapter, adapter: Arc<dyn CliAgentAdapter>) {
        let mut inner = self.inner.lock().expect("CliRuntime mutex poisoned");
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
        let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
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
        let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
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
            let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
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
        let backend: SharedBackend = Arc::new(Mutex::new(backend_box));

        let patterns: Arc<[_]> = adapter.status_patterns().to_vec().into();
        let machine = StatusMachine::new(patterns);
        let (status_tx, status_rx) = watch::channel(AgentStatus::Idle);

        let poll_backend = backend.clone();
        let poll_handle = tokio::spawn(poll_loop(poll_backend, term_id, machine, status_tx));

        let id = {
            let mut inner = self.inner.lock().expect("CliRuntime mutex poisoned");
            let id = AgentSessionId::new(inner.next_id);
            inner.next_id = inner.next_id.saturating_add(1);
            inner.sessions.insert(
                id,
                SessionEntry {
                    backend,
                    term_id,
                    status_rx,
                    poll_handle,
                },
            );
            id
        };
        Ok(id)
    }

    async fn send_message(&self, id: AgentSessionId, msg: &str) -> Result<()> {
        let (backend, term_id) = {
            let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
            let entry = inner
                .sessions
                .get(&id)
                .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
            (entry.backend.clone(), entry.term_id)
        };
        let bytes = msg.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut be = backend.lock().expect("backend mutex poisoned");
            be.write(term_id, &bytes)
        })
        .await??;
        Ok(())
    }

    async fn cancel(&self, id: AgentSessionId) -> Result<()> {
        let entry = {
            let mut inner = self.inner.lock().expect("CliRuntime mutex poisoned");
            inner
                .sessions
                .remove(&id)
                .ok_or_else(|| anyhow!("unknown session {:?}", id))?
        };
        let backend = entry.backend.clone();
        let term_id = entry.term_id;
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
            let mut be = backend.lock().expect("backend mutex poisoned");
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
        Ok(())
    }

    fn subscribe_status(&self, id: AgentSessionId) -> Result<AgentStatusStream> {
        let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
        let entry = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
        Ok(entry.status_rx.clone())
    }

    fn current_status(&self, id: AgentSessionId) -> Result<AgentStatus> {
        let inner = self.inner.lock().expect("CliRuntime mutex poisoned");
        let entry = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {:?}", id))?;
        Ok(entry.status_rx.borrow().clone())
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
    status_tx: watch::Sender<AgentStatus>,
) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let events = {
            let mut be = backend.lock().expect("backend mutex poisoned");
            be.drain_events()
        };
        let now = Instant::now();
        let mut saw_exit = false;
        // Defensive id filter. We use a per-session backend so all events
        // here are ours, but if that invariant ever changes (shared backend
        // experiment, fixture replay reusing a backend), this guard keeps
        // sessions from polluting each other's status.
        for ev in events {
            match ev {
                TerminalEvent::Output { id, bytes } if id == term_id => {
                    if let Some(t) = machine.feed(&bytes, now) {
                        let _ = status_tx.send(t.to);
                    }
                }
                TerminalEvent::Exit { id, code } if id == term_id => {
                    if let Some(t) = machine.note_exit(code) {
                        let _ = status_tx.send(t.to);
                    }
                    saw_exit = true;
                }
                _ => {}
            }
        }
        if let Some(t) = machine.tick(now) {
            let _ = status_tx.send(t.to);
        }
        if saw_exit {
            return;
        }
        // If every subscriber dropped (session removed and UI gone), the
        // sender's `is_closed` flips to true.
        if status_tx.is_closed() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CustomCommandAdapter;
    use std::path::PathBuf;

    fn echo_cfg(program: &str, args: Vec<String>) -> AgentSessionConfig {
        AgentSessionConfig {
            adapter: AgentAdapter::Custom,
            worktree_path: PathBuf::from("/"),
            prompt: None,
            model: None,
            effort: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
            custom_command: Some((program.to_string(), args)),
        }
    }

    fn runtime_with_custom() -> CliRuntime {
        let rt = CliRuntime::new();
        rt.register_adapter(AgentAdapter::Custom, Arc::new(CustomCommandAdapter));
        rt
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_session_unknown_adapter_errors() {
        let rt = CliRuntime::new();
        let err = rt
            .start_session(echo_cfg("/bin/true", vec![]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no adapter"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn custom_echo_runs_to_done() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/echo", vec!["hello".into()]))
            .await
            .expect("start_session");

        let mut rx = rt.subscribe_status(id).expect("subscribe");
        // Wait until status becomes terminal or 3 s elapses.
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if rx.borrow().is_terminal() {
                    return rx.borrow().clone();
                }
                if rx.changed().await.is_err() {
                    // Sender dropped before terminal — runtime bug; surface
                    // as a panic so the test fails loudly.
                    panic!("status sender closed before terminal status");
                }
            }
        })
        .await;
        let final_status = result.expect("did not reach terminal status in time");
        match final_status {
            AgentStatus::Done { code } => {
                // /bin/echo exits 0
                assert_eq!(code, Some(0), "expected exit code 0, got {code:?}");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn current_status_starts_at_idle() {
        let rt = runtime_with_custom();
        // /bin/sleep gives us a few seconds of "running but not yet done"
        // to query while the session is live.
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["2".into()]))
            .await
            .expect("start_session");
        let initial = rt.current_status(id).expect("current_status");
        assert!(matches!(initial, AgentStatus::Idle | AgentStatus::Running));
        // Cleanup so the test doesn't take 2 s.
        let _ = rt.cancel(id).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_terminates_session_and_removes_entry() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
            .await
            .expect("start_session");
        // Cancel should return well before the 30 s sleep finishes.
        let cancel_started = Instant::now();
        rt.cancel(id).await.expect("cancel");
        assert!(
            cancel_started.elapsed() < Duration::from_secs(5),
            "cancel took {:?}, expected < 5 s",
            cancel_started.elapsed()
        );
        // Session is gone from the table — subscribe_status now errors.
        let err = rt.subscribe_status(id).unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_message_writes_to_pty() {
        let rt = runtime_with_custom();
        // /bin/cat echoes whatever we feed it; we just verify write() does
        // not error and the session survives the write.
        let id = rt
            .start_session(echo_cfg("/bin/cat", vec![]))
            .await
            .expect("start_session");
        rt.send_message(id, "ping\n").await.expect("send_message");
        // status is still non-terminal
        let s = rt.current_status(id).expect("current_status");
        assert!(!s.is_terminal());
        let _ = rt.cancel(id).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_message_unknown_session_errors() {
        let rt = runtime_with_custom();
        let bogus = AgentSessionId::new(999);
        let err = rt.send_message(bogus, "x").await.unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_unknown_session_errors() {
        let rt = runtime_with_custom();
        let bogus = AgentSessionId::new(999);
        let err = rt.cancel(bogus).await.unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    // M2 (review 260520-1448): explicit subscribe-then-cancel coverage of
    // the user-facing contract — UI badge holds a Receiver across a cancel
    // and must observe the final terminal status before the session is
    // removed from the table.
    #[tokio::test(flavor = "multi_thread")]
    async fn subscribe_then_cancel_publishes_terminal_status() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
            .await
            .expect("start_session");
        let mut rx = rt.subscribe_status(id).expect("subscribe");
        rt.cancel(id).await.expect("cancel");
        // After cancel returns, the poll task has either exited (publishing
        // terminal status) or been aborted. In the happy path we see a
        // terminal state on the receiver.
        let final_status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let s = rx.borrow().clone();
                if s.is_terminal() {
                    return s;
                }
                if rx.changed().await.is_err() {
                    return rx.borrow().clone();
                }
            }
        })
        .await
        .expect("did not reach terminal status after cancel");
        assert!(
            final_status.is_terminal(),
            "expected terminal, got {final_status:?}"
        );
    }

    // M3 (review 260520-1448): double-cancel must error on the second call
    // with the typed "unknown session" message — proves the table remove
    // is the source of truth, not just the OS-level kill.
    #[tokio::test(flavor = "multi_thread")]
    async fn double_cancel_second_call_errors() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
            .await
            .expect("start_session");
        rt.cancel(id).await.expect("first cancel");
        let err = rt.cancel(id).await.unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    // Phase 3 step 9 sub-1: the app renderer needs the same backend Arc the
    // poll task holds so it can drain output and resize without going
    // through `send_message`. `backend_for` hands out a clone; both
    // callers compete on the same mutex.
    #[tokio::test(flavor = "multi_thread")]
    async fn backend_for_returns_live_handle_shared_with_poll_task() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
            .await
            .expect("start_session");
        let backend = rt.backend_for(id).expect("backend_for");
        // Arc count: one in SessionEntry, one cloned into the poll task,
        // one we just took. Asserting an exact count would tie the test
        // to the poll-task internals; instead prove the Arc is shared by
        // exercising the same mutex from both sides.
        let term_id = rt.terminal_session_id(id).expect("terminal_session_id");
        let resize_ok = tokio::task::spawn_blocking(move || {
            let mut be = backend.lock().expect("backend mutex poisoned");
            be.resize(term_id, 100, 30).is_ok()
        })
        .await
        .expect("spawn_blocking");
        assert!(resize_ok, "renderer-side resize must succeed");
        // Session is still alive and reachable through the runtime.
        let s = rt.current_status(id).expect("current_status");
        assert!(!s.is_terminal(), "session must not be killed by resize");
        let _ = rt.cancel(id).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backend_for_unknown_session_errors() {
        let rt = runtime_with_custom();
        let bogus = AgentSessionId::new(999);
        // `SharedBackend` wraps a trait object so `Result<SharedBackend>`
        // doesn't impl `Debug`; pattern-match the Err arm instead of
        // `unwrap_err()`.
        match rt.backend_for(bogus) {
            Ok(_) => panic!("expected unknown-session error"),
            Err(e) => assert!(e.to_string().contains("unknown session")),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn terminal_session_id_unknown_session_errors() {
        let rt = runtime_with_custom();
        let bogus = AgentSessionId::new(999);
        let err = rt.terminal_session_id(bogus).unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }

    // After cancel removes the SessionEntry, backend_for must surface the
    // typed error — proves the session table (not the OS-level handle) is
    // the source of truth.
    #[tokio::test(flavor = "multi_thread")]
    async fn backend_for_after_cancel_returns_unknown_session() {
        let rt = runtime_with_custom();
        let id = rt
            .start_session(echo_cfg("/bin/sleep", vec!["30".into()]))
            .await
            .expect("start_session");
        rt.cancel(id).await.expect("cancel");
        match rt.backend_for(id) {
            Ok(_) => panic!("expected unknown-session error after cancel"),
            Err(e) => assert!(e.to_string().contains("unknown session")),
        }
    }
}
