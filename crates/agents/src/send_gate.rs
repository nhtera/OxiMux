//! The one queue every send to a chat session goes through.
//!
//! A message for a chat agent can come from its composer, a phone, another
//! agent (`send_agent`) or the host itself (a child's events). Writing any of
//! them straight to the CLI would bypass what the user set up: a Stop, a turn
//! still streaming, an armed retry, a permission prompt waiting for an answer.
//! So they all queue here, and the gate decides when each may go.
//!
//! The gate is GPUI-free and lives behind `Arc<Mutex<_>>`, shared by the
//! session's view and its registry handle. It never sends: the actual send
//! belongs to whoever owns the connection (the desktop view's `send_text`).
//! When something becomes sendable the gate calls its **waker**, and the owner
//! takes the item under the same lock and sends it. A handle with no waker
//! (nothing has registered one) sends directly, as before the gate existed.
//!
//! **Queue or steer.** By default an item waits for the turn to end
//! ([`DeliveryMode::Queue`]). An item may instead be written into the running
//! turn ([`DeliveryMode::Steer`]) when the CLI folds a mid-turn send in (it
//! reports which lines it started on); see [`SendGate::steer_available`].
//! Steering never answers or buries a prompt, so it waits while one is pending.
//!
//! The decision and the write happen under one lock with no await between, so
//! a turn that ends in the gap simply makes the write start the next turn,
//! which is what queueing would have done.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::thread::entry::ChatImage;
use uuid::Uuid;

/// Why an item is not going out right now. Shown on its chip, so "why isn't
/// this sending?" always has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendWait {
    /// Sendable.
    None,
    /// A turn is running.
    Busy,
    /// A turn is starting but has not said so yet (a steer waits for it).
    Starting,
    /// A permission or question prompt is waiting for an answer.
    Interaction,
    /// A provider limit closed the last turn; a retry is armed.
    Limited,
    /// Restored from before a restart; waits for the user (set by the host).
    HeldRestored,
    /// The agent's process is gone.
    HostOffline,
    /// The user pressed Stop. Child events wait for the user's next message
    /// or Send all; they never start a turn on their own.
    StoppedByUser,
}

/// When an item may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    /// After the running turn ends.
    #[default]
    Queue,
    /// Into the running turn, when the CLI supports it; otherwise as `Queue`.
    Steer,
}

/// Who an item came from. Set by the host, never inferred from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendSource {
    /// The user at this desk.
    User,
    /// A remote client (a phone) or an agent's `send_agent`: nobody is at the
    /// screen this session drives.
    Remote,
    /// A batch of child events for this parent (an outbox delivery).
    ParentEvent,
}

/// What holds a session's sends, as its driver sees it. Mirrored into the
/// gate in one call after every change ([`SendGate::set_holds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Holds {
    /// A turn is running.
    pub turn_active: bool,
    /// The user stopped the session; it resumes on their next message.
    pub interrupted: bool,
    /// Nothing to send to: the process is gone, not started, or signing in.
    pub disconnected: bool,
    /// A provider limit closed the last turn and a retry is armed.
    pub retry_armed: bool,
    /// A permission or question prompt waits for an answer.
    pub prompt_pending: bool,
}

/// One queued message, in the form it is written to the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedSend {
    /// The gate's handle on the item (chips, `remove`, `steer_now`).
    pub id: u64,
    /// The id the CLI reports the line by. For a child-event batch this is
    /// the outbox key, so acceptance settles the outbox rows.
    pub uuid: Uuid,
    pub text: String,
    pub images: Vec<ChatImage>,
    pub mode: DeliveryMode,
    pub source: SendSource,
}

/// What the owner should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// Nothing to do.
    Idle,
    /// Take the next item with [`SendGate::take_next_if_open`] and start a turn.
    Flush,
    /// Take the steerable item with [`SendGate::take_steerable`] and write it
    /// into the running turn.
    Steer,
}

type Waker = Arc<dyn Fn() + Send + Sync>;
type SentHook = Arc<dyn Fn(Uuid, bool) + Send + Sync>;

/// The gate. See the module docs.
#[derive(Default)]
pub struct SendGate {
    queue: VecDeque<QueuedSend>,
    next_id: u64,
    waker: Option<Waker>,
    /// Told when the owner wrote an item (or failed to): the host settles the
    /// child events it queued by the item's uuid.
    sent_hook: Option<SentHook>,
    /// A turn is running (from the first send until its end frame).
    turn_active: bool,
    /// The running turn has said it started (a steer may be written).
    turn_started: bool,
    interrupted: bool,
    disconnected: bool,
    retry_armed: bool,
    prompt_pending: bool,
    stopped_by_user: bool,
    /// The CLI folds mid-turn sends and reports which it started (version
    /// gate, adapter, and the runtime self-check all passed).
    steer_supported: bool,
    /// Who started the running turn.
    turn_source: Option<SendSource>,
}

impl std::fmt::Debug for SendGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendGate")
            .field("queue", &self.queue.len())
            .field("turn_active", &self.turn_active)
            .field("stopped_by_user", &self.stopped_by_user)
            .finish_non_exhaustive()
    }
}

impl SendGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Who to ping when something becomes sendable. Called with the gate's
    /// lock held, so it must only signal (never lock the gate).
    pub fn set_waker(&mut self, waker: impl Fn() + Send + Sync + 'static) {
        self.waker = Some(Arc::new(waker));
    }

    pub fn clear_waker(&mut self) {
        self.waker = None;
    }

    /// Who to tell when an item was written. Called with the gate's lock
    /// held, so it must only signal.
    pub fn set_sent_hook(&mut self, hook: impl Fn(Uuid, bool) + Send + Sync + 'static) {
        self.sent_hook = Some(Arc::new(hook));
    }

    /// The owner wrote the item queued under `uuid` (`ok`), or the write
    /// failed.
    pub fn report_sent(&self, uuid: Uuid, ok: bool) {
        if let Some(hook) = &self.sent_hook {
            hook(uuid, ok);
        }
    }

    /// Whether an owner is listening. Without one, a sender writes directly.
    pub fn has_waker(&self) -> bool {
        self.waker.is_some()
    }

    /// Queue a message. Returns its id and uuid. `uuid` is the caller's when
    /// it has one (an outbox key), otherwise a fresh one.
    pub fn push(
        &mut self,
        text: String,
        images: Vec<ChatImage>,
        source: SendSource,
        mode: DeliveryMode,
        uuid: Option<Uuid>,
    ) -> (u64, Uuid) {
        self.next_id += 1;
        let id = self.next_id;
        let uuid = uuid.unwrap_or_else(Uuid::new_v4);
        // Only the host's own deliveries are events: text from anyone else that
        // merely looks like one goes out as plain text.
        let text = match source {
            SendSource::ParentEvent => text,
            _ => crate::envelope::neutralize_forged(&text).into_owned(),
        };
        self.queue.push_back(QueuedSend { id, uuid, text, images, mode, source });
        self.wake_if_ready();
        (id, uuid)
    }

    /// Drop an item (the user edited or cancelled its chip). `None` when it
    /// already went out.
    pub fn remove(&mut self, id: u64) -> Option<QueuedSend> {
        let at = self.queue.iter().position(|q| q.id == id)?;
        self.queue.remove(at)
    }

    pub fn queued(&self) -> impl Iterator<Item = &QueuedSend> {
        self.queue.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Why `item` is waiting (or [`SendWait::None`]).
    pub fn wait_of(&self, item: &QueuedSend) -> SendWait {
        if item.source == SendSource::ParentEvent && self.stopped() {
            return SendWait::StoppedByUser;
        }
        if self.disconnected {
            return SendWait::HostOffline;
        }
        if self.prompt_pending {
            return SendWait::Interaction;
        }
        if self.retry_armed {
            return SendWait::Limited;
        }
        if self.turn_active {
            if item.mode == DeliveryMode::Steer && self.steer_supported && !self.turn_started {
                return SendWait::Starting;
            }
            if !(item.mode == DeliveryMode::Steer && self.steer_available()) {
                return SendWait::Busy;
            }
        }
        SendWait::None
    }

    /// What the owner should do now.
    pub fn ready(&self) -> Ready {
        if self.steer_available() && self.queue.iter().any(|q| q.mode == DeliveryMode::Steer) {
            return Ready::Steer;
        }
        if self.next_sendable().is_some() {
            return Ready::Flush;
        }
        Ready::Idle
    }

    /// The user stopped the session: child events wait for them.
    fn stopped(&self) -> bool {
        self.interrupted || self.stopped_by_user
    }

    /// A new turn may start: nothing running, nothing holding.
    fn open(&self) -> bool {
        !self.turn_active && !self.disconnected && !self.retry_armed && !self.prompt_pending
    }

    /// Where the oldest item that may start a turn now sits. After a Stop a
    /// person's message still goes (it resumes the session, and takes the held
    /// child events with it); child events alone never start a turn then.
    fn next_sendable(&self) -> Option<usize> {
        if !self.open() {
            return None;
        }
        self.queue.iter().position(|q| q.source != SendSource::ParentEvent || !self.stopped())
    }

    /// The oldest item that may start a turn now. The caller sends it at once
    /// (the gate counts the turn as running from here).
    pub fn take_next_if_open(&mut self) -> Option<QueuedSend> {
        let item = self.queue.remove(self.next_sendable()?)?;
        self.begin_turn(item.source);
        Some(item)
    }

    /// Whether a line may be written into the running turn now.
    pub fn steer_available(&self) -> bool {
        self.steer_supported
            && self.turn_active
            && self.turn_started
            && !self.interrupted
            && !self.disconnected
            && !self.retry_armed
            && !self.prompt_pending
            && !self.stopped_by_user
    }

    /// The oldest steer-mode item, if steering is available now.
    pub fn take_steerable(&mut self) -> Option<QueuedSend> {
        if !self.steer_available() {
            return None;
        }
        let at = self.queue.iter().position(|q| q.mode == DeliveryMode::Steer)?;
        self.queue.remove(at)
    }

    /// The user's Send now on a queued item: steer it if possible. `None`
    /// (the item stays queued) when steering is not available.
    pub fn steer_now(&mut self, id: u64) -> Option<QueuedSend> {
        if !self.steer_available() {
            return None;
        }
        self.remove(id)
    }

    /// The user is sending a message after a Stop: the child events held for
    /// them go first, as part of the same turn. Clears the Stop hold.
    pub fn take_held_events(&mut self) -> Vec<QueuedSend> {
        self.stopped_by_user = false;
        let (events, rest): (VecDeque<_>, VecDeque<_>) =
            self.queue.drain(..).partition(|q| q.source == SendSource::ParentEvent);
        self.queue = rest;
        events.into()
    }

    // ── what the owner reports ─────────────────────────────────────────────

    /// Mirror the driver's holds in one go; wakes the owner (once) when that
    /// opens the gate.
    pub fn set_holds(&mut self, holds: Holds) {
        let was = self.ready();
        if holds.turn_active && !self.turn_active {
            self.turn_started = false;
        }
        if !holds.turn_active {
            self.turn_started = false;
            self.turn_source = None;
        }
        self.turn_active = holds.turn_active;
        self.interrupted = holds.interrupted;
        self.disconnected = holds.disconnected;
        self.retry_armed = holds.retry_armed;
        self.prompt_pending = holds.prompt_pending;
        if self.ready() != was {
            self.wake_if_ready();
        }
    }

    /// The owner started a turn by itself (a direct send).
    pub fn begin_turn(&mut self, source: SendSource) {
        self.turn_active = true;
        self.turn_started = false;
        self.turn_source = Some(source);
    }

    /// The running turn said it started.
    pub fn on_turn_started(&mut self) {
        if self.turn_active {
            self.turn_started = true;
            self.wake_if_ready();
        }
    }

    pub fn on_turn_end(&mut self) {
        self.turn_active = false;
        self.turn_started = false;
        self.turn_source = None;
        self.wake_if_ready();
    }

    /// Whether the running turn was started by child events rather than a
    /// person. Such a turn may not raise the session's permission mode.
    pub fn turn_started_by_events(&self) -> bool {
        self.turn_source == Some(SendSource::ParentEvent)
    }

    pub fn set_prompt_pending(&mut self, pending: bool) {
        self.prompt_pending = pending;
        self.wake_if_ready();
    }

    pub fn set_retry_armed(&mut self, armed: bool) {
        self.retry_armed = armed;
        self.wake_if_ready();
    }

    pub fn set_disconnected(&mut self, disconnected: bool) {
        self.disconnected = disconnected;
        if disconnected {
            self.turn_active = false;
            self.turn_started = false;
        }
        self.wake_if_ready();
    }

    /// The process was stopped and will be resumed by the next user send.
    pub fn set_interrupted(&mut self, interrupted: bool) {
        self.interrupted = interrupted;
        if interrupted {
            self.turn_active = false;
            self.turn_started = false;
        }
        self.wake_if_ready();
    }

    /// The user pressed Stop: nothing new is written until they act.
    pub fn stop_by_user(&mut self) {
        self.stopped_by_user = true;
    }

    /// The user's Send all: release the Stop hold.
    pub fn clear_stop(&mut self) {
        self.stopped_by_user = false;
        self.wake_if_ready();
    }

    pub fn stopped_by_user(&self) -> bool {
        self.stopped_by_user
    }

    pub fn set_steer_supported(&mut self, supported: bool) {
        self.steer_supported = supported;
        self.wake_if_ready();
    }

    pub fn steer_supported(&self) -> bool {
        self.steer_supported
    }

    fn wake_if_ready(&self) {
        if self.ready() != Ready::Idle
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }
}

#[cfg(test)]
#[path = "send_gate_tests.rs"]
mod tests;
