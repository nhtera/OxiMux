//! The desktop's remote-control state: a process-wide [`SessionRegistry`] that the
//! (future) in-app iroh host serves, plus an `enabled` flag that gates whether live
//! agent sessions are fanned into it at all.
//!
//! Held as a [`gpui::Global`] so any [`AgentChatView`] can bind its session into the
//! registry on connect and tee its `ThreadEvent`s in — but only while remote control
//! is enabled, so a disabled desktop pays **zero** per-event cost (no clone, no
//! registration). The registry itself is `gpui`-free and lives behind an `Arc`, so
//! the network layer subscribes and commands sessions off the UI thread.
//!
//! [`AgentChatView`]: crate::shell::agent_chat

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gpui::Global;
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::AgentConnection;

/// Monotonic source of stable per-view remote session ids. Decoupled from an
/// agent's own (often not-yet-known-at-connect) session id: the phone only needs a
/// key that stays stable for a desktop session's lifetime, and a fresh chat has no
/// id until its subprocess assigns one. Process-wide, so ids never collide.
static REMOTE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Mint the next stable remote session id (`"agent-N"`). Human-readable so it reads
/// sensibly in the phone's session list.
pub fn next_remote_session_id() -> String {
    format!("agent-{}", REMOTE_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// A view's live tie into the registry: the handle to tee events through, plus the
/// registry itself so teardown can `unregister` without a `gpui` context (a `Drop`
/// has none). Held `Option`ally on the view — `None` when remote is disabled.
pub struct RemoteBinding {
    registry: Arc<SessionRegistry>,
    handle: Arc<SessionHandle>,
}

impl RemoteBinding {
    /// Fan one backend event into the bound session (assign seq, store, broadcast).
    pub fn ingest(&self, event: oximux_agents::thread::ThreadEvent) {
        self.handle.ingest(event);
    }

    /// Remove the session from the registry. The map holds its own `Arc`, so this
    /// explicit call is required — dropping the view's handle alone won't evict it.
    pub fn unregister(self, id: &str) {
        self.registry.unregister(id);
    }
}

/// Process-wide remote-control state, installed once at boot as a `gpui::Global`.
pub struct RemoteControl {
    registry: Arc<SessionRegistry>,
    /// `AtomicBool` (not `bool`) so a future Settings toggle can flip it through the
    /// shared `&Global` reference without needing `&mut` access to the global.
    enabled: AtomicBool,
}

impl Global for RemoteControl {}

impl Default for RemoteControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControl {
    /// A fresh, **disabled** remote-control state — no sessions are fanned in until
    /// something enables it (the enablement UI + host bind land in a later slice).
    pub fn new() -> Self {
        Self { registry: Arc::new(SessionRegistry::new()), enabled: AtomicBool::new(false) }
    }

    /// The shared session registry (the host serves from this same instance).
    pub fn registry(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    /// Register `id`→`conn` and return the binding **iff remote is enabled**;
    /// `None` when disabled, so the caller does no work and holds no binding.
    pub fn bind(&self, id: &str, conn: Arc<dyn AgentConnection>) -> Option<RemoteBinding> {
        if !self.enabled() {
            return None;
        }
        let handle = self.registry.register(id.to_string(), conn);
        Some(RemoteBinding { registry: self.registry.clone(), handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::{StubConnection, ThreadEvent};

    fn a_conn() -> Arc<dyn AgentConnection> {
        Arc::new(StubConnection::default())
    }

    /// Disabled is the default and binds nothing — the per-event path stays free.
    #[test]
    fn disabled_binds_nothing() {
        let rc = RemoteControl::new();
        assert!(!rc.enabled());
        assert!(rc.bind("agent-1", a_conn()).is_none());
        assert!(rc.registry().is_empty(), "no session registered while disabled");
    }

    /// Enabled binds a session whose teed events reach a live subscriber in order.
    #[test]
    fn enabled_binds_and_tees_in_order() {
        let rc = RemoteControl::new();
        rc.set_enabled(true);

        let binding = rc.bind("agent-1", a_conn()).expect("bound while enabled");
        let mut rx = rc.registry().subscribe("agent-1").expect("registered");

        binding.ingest(ThreadEvent::AssistantText("hi".into()));
        binding.ingest(ThreadEvent::AssistantText(" there".into()));

        let (seq1, ev1) = rx.try_recv().expect("first teed event");
        assert_eq!(seq1, 1);
        assert_eq!(ev1, ThreadEvent::AssistantText("hi".into()));
        assert_eq!(rx.try_recv().expect("second teed event").0, 2, "seq advances");

        binding.unregister("agent-1");
        assert!(rc.registry().get("agent-1").is_none(), "unregister evicts the session");
    }
}
