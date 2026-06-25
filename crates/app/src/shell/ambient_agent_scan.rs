//! Hook-driven status for agents in *plain* terminals.
//!
//! A spawned/tracked agent tab runs a full `AgentRuntime` whose poll loop reads
//! the PTY, decodes the OSC-9999 status sideband, and publishes rich status. A
//! hand-typed `claude`/`codex`/… in an ordinary terminal has no such runtime —
//! historically the sidebar could only guess its state from the OSC *title*
//! glyphs, which are coarse and flap. But OxiMux's global hooks already emit
//! the same OSC-9999 packets (keyed by `OXIMUX_PTY_ID`) onto that terminal's
//! output stream; nothing was consuming them.
//!
//! [`AmbientAgentScan`] consumes them. The terminal view feeds it each output
//! chunk; it extracts the sideband event and tracks the agent's reported status
//! plus the user's prompt (cached across the turn, like the runtime poll loop)
//! and the live tool step. This gives a hand-launched agent the SAME stable,
//! hook-driven status as a spawned one, instead of the flaky title heuristic.
//!
//! The title path is kept as a fast presence/liveness signal (an agent that has
//! launched but not yet emitted a hook still shows up); the sideband, when
//! present, wins for the status and the descriptor.

use std::time::{Duration, Instant};

use oximux_agents::osc_sideband::{AgentOscScanner, map_state_to_status};
use oximux_core::{AgentStatus, SidebandDetail};

/// Six-byte OSC-9999 introducer (`ESC ] 9 9 9 9`). A chunk without it — and
/// with no sequence mid-parse — cannot complete a sideband event, so the
/// scanner (and its per-chunk allocation) is skipped entirely for the common
/// plain-shell output.
const SIDEBAND_MARKER: &[u8] = b"\x1b]9999";

/// How long a hook-reported status stays authoritative without a refresh. Hooks
/// fire on every prompt/tool step while an agent works, so a live agent never
/// goes stale; this only bounds how long a *finished* agent's last state
/// lingers before the title path (or its absence) takes over — mirrors the
/// reference app's stale-after window.
const SIDEBAND_TTL: Duration = Duration::from_secs(30 * 60);

/// A fresh hook-derived reading for one terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmbientSideband {
    pub status: AgentStatus,
    pub detail: SidebandDetail,
}

/// Per-terminal OSC-9999 consumer. Cheap to construct; one per `TerminalView`.
pub struct AmbientAgentScan {
    scanner: AgentOscScanner,
    status: Option<AgentStatus>,
    detail: SidebandDetail,
    /// The user's most recent prompt — the agent's stable title. A prompt-submit
    /// event carries it once; the tool/idle events that follow carry none, so it
    /// is re-attached to every reading until the next prompt replaces it.
    cached_prompt: Option<String>,
    last_seen: Option<Instant>,
}

impl Default for AmbientAgentScan {
    fn default() -> Self {
        Self::new()
    }
}

impl AmbientAgentScan {
    pub fn new() -> Self {
        Self {
            scanner: AgentOscScanner::new(),
            status: None,
            detail: SidebandDetail::default(),
            cached_prompt: None,
            last_seen: None,
        }
    }

    /// Feed one PTY output chunk. The scanner runs only when a sideband marker
    /// is present (or a prior sequence is still mid-parse), so a plain shell's
    /// output costs one substring scan and no allocation.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) {
        if !self.scanner.is_active() && !contains_marker(bytes) {
            return;
        }
        let out = self.scanner.feed(bytes);
        if let Some(ev) = out.event {
            self.apply(ev.state, ev.detail, now);
        }
    }

    fn apply(
        &mut self,
        state: oximux_core::AgentSidebandState,
        mut detail: SidebandDetail,
        now: Instant,
    ) {
        // A prompt-submit event carries the title; cache it so later tool/idle
        // events keep surfacing it. Never blank a cached prompt with an empty.
        if detail.prompt.as_deref().is_some_and(|p| !p.is_empty()) {
            self.cached_prompt = detail.prompt.clone();
        } else {
            detail.prompt = self.cached_prompt.clone();
        }
        // Carry the agent's last reply forward when this event brings none. Only
        // a finished turn (`Stop`) supplies a message; the prompt/tool events
        // that follow carry none and must not blank it, so the row keeps showing
        // the last reply across the next turn instead of reverting to a bare
        // status verb — matching the reference cockpit, which preserves the last
        // assistant message until a newer reply replaces it. (`tool_name` is left
        // to the incoming event so an idle row correctly drops a stale tool.)
        if detail.last_message.is_none() {
            detail.last_message = self.detail.last_message.clone();
        }
        let status = map_state_to_status(state, detail.tool_name.clone());
        // The one link in the global-hook → relay → plain-terminal path that
        // can't be unit-tested (needs a live hand-typed agent emitting to this
        // PTY). `RUST_LOG=oximux_app=debug` surfaces it for live verification.
        tracing::debug!(
            ?status,
            tool = ?detail.tool_name,
            prompt = ?detail.prompt,
            "ambient agent OSC-9999 sideband decoded"
        );
        self.status = Some(status);
        self.detail = detail;
        self.last_seen = Some(now);
    }

    /// Re-prime the scan from a persisted reading on restore, so a still-running
    /// agent in a re-attached terminal is listed immediately instead of waiting
    /// for its next hook (an agent idle at its prompt emits none). `now` marks
    /// the reading fresh from here on — wall-clock TTL was already enforced by
    /// the persistence layer before this is called.
    pub fn seed(&mut self, status: AgentStatus, detail: SidebandDetail, now: Instant) {
        self.cached_prompt = detail.prompt.clone();
        self.status = Some(status);
        self.detail = detail;
        self.last_seen = Some(now);
    }

    /// The current hook-derived reading, or `None` when no sideband has ever
    /// arrived or the last one is older than [`SIDEBAND_TTL`].
    pub fn current(&self, now: Instant) -> Option<AmbientSideband> {
        let last = self.last_seen?;
        if now.saturating_duration_since(last) > SIDEBAND_TTL {
            return None;
        }
        Some(AmbientSideband {
            status: self.status.clone()?,
            detail: self.detail.clone(),
        })
    }
}

/// True when `bytes` contains the OSC-9999 introducer.
fn contains_marker(bytes: &[u8]) -> bool {
    bytes.len() >= SIDEBAND_MARKER.len()
        && bytes
            .windows(SIDEBAND_MARKER.len())
            .any(|w| w == SIDEBAND_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(payload: &str) -> Vec<u8> {
        let mut v = b"\x1b]9999;".to_vec();
        v.extend_from_slice(payload.as_bytes());
        v.push(0x07);
        v
    }

    #[test]
    fn plain_output_never_registers_an_agent() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();
        scan.feed(b"\x1b[32mok\x1b[0m just a colored shell line\n", now);
        assert!(scan.current(now).is_none());
    }

    #[test]
    fn sideband_drives_status_and_caches_prompt_across_the_turn() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();

        scan.feed(&osc(r#"{"v":1,"state":"working","prompt":"fix the parser"}"#), now);
        let cur = scan.current(now).expect("agent detected");
        assert_eq!(cur.status, AgentStatus::Running);
        assert_eq!(cur.detail.prompt.as_deref(), Some("fix the parser"));

        // A later tool step carries no prompt — the cached title must persist.
        scan.feed(
            &osc(r#"{"v":1,"state":"working","tool":"Edit","tool_input":"x.rs"}"#),
            now,
        );
        let cur = scan.current(now).expect("still detected");
        assert_eq!(cur.detail.prompt.as_deref(), Some("fix the parser"));
        assert_eq!(cur.detail.tool_name.as_deref(), Some("Edit"));
    }

    #[test]
    fn last_assistant_message_survives_the_next_turn() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();
        // A finished turn brings the agent's reply.
        scan.feed(
            &osc(r#"{"v":1,"state":"idle","msg":"All done — 3 files changed."}"#),
            now,
        );
        assert_eq!(
            scan.current(now)
                .and_then(|c| c.detail.last_message)
                .as_deref(),
            Some("All done — 3 files changed.")
        );
        // The next prompt carries no reply — the last one must persist so the
        // row keeps showing it instead of reverting to a bare status verb.
        scan.feed(&osc(r#"{"v":1,"state":"working","prompt":"now add tests"}"#), now);
        assert_eq!(
            scan.current(now)
                .and_then(|c| c.detail.last_message)
                .as_deref(),
            Some("All done — 3 files changed.")
        );
        // A tool step likewise carries none; the reply persists while the tool
        // is surfaced freshly.
        scan.feed(
            &osc(r#"{"v":1,"state":"working","tool":"Edit","tool_input":"x.rs"}"#),
            now,
        );
        let cur = scan.current(now).expect("still detected");
        assert_eq!(
            cur.detail.last_message.as_deref(),
            Some("All done — 3 files changed.")
        );
        assert_eq!(cur.detail.tool_name.as_deref(), Some("Edit"));
        // A newer finished turn replaces the reply.
        scan.feed(&osc(r#"{"v":1,"state":"idle","msg":"Tests added."}"#), now);
        assert_eq!(
            scan.current(now)
                .and_then(|c| c.detail.last_message)
                .as_deref(),
            Some("Tests added.")
        );
    }

    #[test]
    fn needs_approval_carries_the_tool_reason() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();
        scan.feed(&osc(r#"{"v":1,"state":"needs_approval","tool":"Bash"}"#), now);
        assert_eq!(
            scan.current(now).map(|c| c.status),
            Some(AgentStatus::NeedsApproval("Bash".into()))
        );
    }

    #[test]
    fn reading_expires_after_the_ttl() {
        let mut scan = AmbientAgentScan::new();
        let start = Instant::now();
        scan.feed(&osc(r#"{"v":1,"state":"idle"}"#), start);
        assert!(scan.current(start).is_some());
        assert!(scan.current(start + SIDEBAND_TTL + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn seed_primes_status_and_prompt_for_immediate_listing() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();
        // A fresh scan (post-restore) reports nothing until seeded.
        assert!(scan.current(now).is_none());
        scan.seed(
            AgentStatus::Running,
            SidebandDetail {
                prompt: Some("hi 2".into()),
                ..Default::default()
            },
            now,
        );
        let cur = scan.current(now).expect("seeded reading is visible");
        assert_eq!(cur.status, AgentStatus::Running);
        assert_eq!(cur.detail.prompt.as_deref(), Some("hi 2"));
        // A later tool step with no prompt keeps the seeded title (cached).
        scan.feed(&osc(r#"{"v":1,"state":"working","tool":"Edit"}"#), now);
        assert_eq!(
            scan.current(now).and_then(|c| c.detail.prompt),
            Some("hi 2".to_string())
        );
    }

    #[test]
    fn marker_guard_handles_a_split_payload() {
        let mut scan = AmbientAgentScan::new();
        let now = Instant::now();
        let full = osc(r#"{"v":1,"state":"working"}"#);
        let (head, tail) = full.split_at(9); // mid-payload boundary
        scan.feed(head, now);
        // The tail has no fresh marker, but the scanner is mid-parse so it must
        // still be fed — the guard checks `is_active()`.
        scan.feed(tail, now);
        assert_eq!(scan.current(now).map(|c| c.status), Some(AgentStatus::Running));
    }
}
