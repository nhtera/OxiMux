//! The reconnect policy: a pure, clock-free state machine over a maintained
//! connection's lifecycle. It owns no transport, timers, or async — the driver
//! feeds it lifecycle events (`begin`/`on_dial_result`/`on_lost`/
//! `on_retry_elapsed`) and it returns the next [`ConnAction`] and updates the
//! observable [`ConnState`]. Separating *policy* from *plumbing* makes the backoff
//! schedule and give-up rule testable with no network and no real time; the async
//! driver that dials (via a [`Connector`](crate::Connector)), sleeps, and drives
//! the demux pump layers on top where a spawn runtime is available.
//!
//! Schedule: a lost connection retries **immediately** once; each subsequent failed
//! dial backs off `base · 2ⁿ` (1s, 2s, 4s at the default `base = 1s`) until the
//! budget is spent, then the host is [`ConnState::Unreachable`] until the user acts.
//!
//! **Driver contract:** the driver keeps a single dial/timer in flight (awaiting
//! each [`ConnAction`] before feeding the next event), so stale timers and
//! concurrent dials never arise — the machine leans on that over generation tokens.

use std::time::Duration;

/// Where a maintained connection currently is — the enum the UI reflects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// No connection, and none being attempted.
    Disconnected,
    /// A dial + handshake is in progress.
    Connecting,
    /// Live — RPCs flow and the event stream streams.
    Connected,
    /// The last dial failed; waiting `delay` before retry number `attempt`.
    WaitingToRetry { attempt: u32, delay: Duration },
    /// Retry budget spent — unreachable until the user retries/re-pairs. `cause` is
    /// the last dial's failure reason, for the UI to show.
    Unreachable { cause: String },
}

/// What the driver should do next after feeding the machine an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnAction {
    /// Dial now (fresh resolve + handshake), then report via [`Reconnect::on_dial_result`].
    Dial,
    /// Sleep `delay`, then feed [`Reconnect::on_retry_elapsed`].
    Wait(Duration),
    /// Stop — the retry budget is spent (state is now [`ConnState::Unreachable`]).
    GiveUp,
    /// Nothing to do; the connection is settled (Connected or Disconnected).
    Idle,
}

/// The reconnect policy. Construct with [`Reconnect::new`] (3 retries, 1s base) or
/// [`Reconnect::with_budget`], then drive it from the connection lifecycle.
#[derive(Debug, Clone)]
pub struct Reconnect {
    state: ConnState,
    /// Consecutive failed dials since the last success — indexes the backoff and
    /// is checked against `max_retries`. Reset to 0 on connect and on a fresh loss.
    retries: u32,
    max_retries: u32,
    base: Duration,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self::new()
    }
}

impl Reconnect {
    /// The default policy: 3 backoff retries, 1s base (1s, 2s, 4s).
    pub fn new() -> Self {
        Self::with_budget(3, Duration::from_secs(1))
    }

    /// A custom budget: `max_retries` failed dials tolerated before giving up, and
    /// the `base` delay the backoff doubles from.
    pub fn with_budget(max_retries: u32, base: Duration) -> Self {
        Self { state: ConnState::Disconnected, retries: 0, max_retries, base }
    }

    /// The observable connection state — what the UI renders.
    pub fn state(&self) -> &ConnState {
        &self.state
    }

    /// Begin the first connection attempt from a clean slate.
    pub fn begin(&mut self) -> ConnAction {
        self.retries = 0;
        self.state = ConnState::Connecting;
        ConnAction::Dial
    }

    /// Report the result of a dial + handshake. `Ok` → connected; `Err(cause)` →
    /// back off and retry, or give up (surfacing `cause`) once the budget is spent.
    pub fn on_dial_result(&mut self, result: Result<(), String>) -> ConnAction {
        match result {
            Ok(()) => {
                self.retries = 0;
                self.state = ConnState::Connected;
                ConnAction::Idle
            }
            Err(cause) => self.schedule_retry(cause),
        }
    }

    /// A live connection dropped. The first reconnect is immediate; backoff only
    /// engages if that (and subsequent) dials fail. Ignored unless currently
    /// [`ConnState::Connected`] — a stray drop signal can't resurrect a given-up or
    /// idle connection (only [`Self::begin`] leaves those).
    pub fn on_lost(&mut self) -> ConnAction {
        if self.state != ConnState::Connected {
            return ConnAction::Idle;
        }
        self.retries = 0;
        self.state = ConnState::Connecting;
        ConnAction::Dial
    }

    /// The scheduled backoff delay elapsed — dial again.
    pub fn on_retry_elapsed(&mut self) -> ConnAction {
        self.state = ConnState::Connecting;
        ConnAction::Dial
    }

    /// After a failed dial: schedule the next backoff retry, or transition to
    /// unreachable (carrying `cause`) once the budget is spent.
    fn schedule_retry(&mut self, cause: String) -> ConnAction {
        if self.retries >= self.max_retries {
            self.state = ConnState::Unreachable { cause };
            return ConnAction::GiveUp;
        }
        // `base · 2^retries` — 1s, 2s, 4s; `saturating`/`min` guard the shift.
        let delay = self.base.saturating_mul(1u32 << self.retries.min(31));
        self.retries += 1;
        self.state = ConnState::WaitingToRetry { attempt: self.retries, delay };
        ConnAction::Wait(delay)
    }
}
