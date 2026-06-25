//! Process-wide persistence for ambient-agent readings, keyed by relay PTY id.
//!
//! An ambient agent (a hand-typed `claude`/`codex`/… in a plain terminal) is
//! detected purely at runtime from the OSC-9999 sideband the global hooks emit.
//! Those packets are never retained in the PTY's byte ring, so a quit/reopen
//! would drop the agent from the rail until its next hook fires — and an agent
//! idle at its prompt fires nothing until the user types again. This module
//! persists the LAST reading per PTY so a warm re-attach re-seeds the scan and
//! the rail lists the still-running agent immediately, the way the reference
//! cockpit keeps a "sleeping" agent alive across a restart.
//!
//! Stored in the settings KV table under `ambient_agent:<pty_id>` — the same
//! shape as the existing `terminal_tabs:*` / `open_windows` entries, so no
//! schema migration is needed. The PTY id is the natural key: a warm re-attach
//! reuses the surviving daemon PTY id (so the reading matches), while a cold
//! restore mints a fresh shell with no agent (so nothing matches — correct).
//!
//! A reading older than the sideband TTL is ignored (and dropped) on load, so
//! a finished agent doesn't resurrect across a long-gone restart. Callers that
//! run before [`init`] (pure unit tests) read `None` and persist nothing.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use oximux_core::{AgentStatus, SidebandDetail};
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

/// Mirrors `ambient_agent_scan::SIDEBAND_TTL` (30 min): a persisted reading
/// older than this is stale and not restored.
const TTL_MS: u64 = 30 * 60 * 1000;

static REPO: OnceLock<SettingsRepo> = OnceLock::new();

/// Install the settings repo used for ambient-agent persistence. Called once
/// from `state::hydrate`; later calls are ignored (first install wins).
pub fn init(repo: SettingsRepo) {
    let _ = REPO.set(repo);
}

/// Persist the latest reading for `pty_id`. Best-effort; failures are logged,
/// never propagated. No-op before [`init`].
pub fn persist(pty_id: &str, status: &AgentStatus, detail: &SidebandDetail) {
    let Some(repo) = REPO.get() else { return };
    if let Err(err) = persist_with(repo, pty_id, status, detail, now_ms()) {
        tracing::warn!(?err, pty_id, "ambient_state: persist failed");
    }
}

/// Load a fresh reading for `pty_id`, or `None` when absent / stale / pre-init.
/// A stale entry is deleted as a side effect so it cannot linger.
pub fn load(pty_id: &str) -> Option<(AgentStatus, SidebandDetail)> {
    load_with(REPO.get()?, pty_id, now_ms())
}

/// Drop the reading for `pty_id` — its PTY died, was cold-restored, or its tab
/// closed. No-op before [`init`].
pub fn forget(pty_id: &str) {
    if let Some(repo) = REPO.get() {
        let _ = repo.delete(&key(pty_id));
    }
}

fn key(pty_id: &str) -> String {
    format!("ambient_agent:{pty_id}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Persisted shape: the runtime reading plus a wall-clock stamp. The scan's own
/// `last_seen` is a monotonic `Instant` that cannot survive a process restart,
/// so freshness is gated here on wall-clock time instead.
#[derive(Serialize, Deserialize)]
struct Persisted {
    status: AgentStatus,
    detail: SidebandDetail,
    updated_at_ms: u64,
}

/// Repo-explicit core of [`persist`], testable without the global handle.
fn persist_with(
    repo: &SettingsRepo,
    pty_id: &str,
    status: &AgentStatus,
    detail: &SidebandDetail,
    now_ms: u64,
) -> Result<(), oximux_storage::StorageError> {
    let rec = Persisted {
        status: status.clone(),
        detail: detail.clone(),
        updated_at_ms: now_ms,
    };
    // These are plain serializable types, so serialization cannot realistically
    // fail; if it ever did, skip the write rather than invent a DB error.
    let Ok(json) = serde_json::to_string(&rec) else {
        return Ok(());
    };
    repo.set(&key(pty_id), &json)
}

/// Repo-explicit core of [`load`], testable without the global handle.
fn load_with(
    repo: &SettingsRepo,
    pty_id: &str,
    now_ms: u64,
) -> Option<(AgentStatus, SidebandDetail)> {
    let raw = repo.get(&key(pty_id)).ok()??;
    let rec: Persisted = serde_json::from_str(&raw).ok()?;
    if now_ms.saturating_sub(rec.updated_at_ms) > TTL_MS {
        let _ = repo.delete(&key(pty_id));
        return None;
    }
    Some((rec.status, rec.detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::open_memory;

    fn repo() -> SettingsRepo {
        SettingsRepo::new(open_memory().expect("memory db"))
    }

    fn detail(prompt: &str) -> SidebandDetail {
        SidebandDetail {
            prompt: Some(prompt.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_status_and_prompt() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Running, &detail("fix the parser"), 1_000)
            .expect("persist");
        let (status, d) = load_with(&r, "pty-1", 2_000).expect("loadable");
        assert_eq!(status, AgentStatus::Running);
        assert_eq!(d.prompt.as_deref(), Some("fix the parser"));
    }

    #[test]
    fn stale_reading_is_dropped_and_not_returned() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Idle, &detail("hi"), 1_000).expect("persist");
        // Just past the TTL → gone, and the row is deleted as a side effect.
        assert!(load_with(&r, "pty-1", 1_000 + TTL_MS + 1).is_none());
        assert!(r.get(&key("pty-1")).expect("get").is_none());
    }

    #[test]
    fn forget_removes_the_entry() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Idle, &detail("hi"), 1_000).expect("persist");
        r.delete(&key("pty-1")).expect("delete");
        assert!(load_with(&r, "pty-1", 1_500).is_none());
    }

    #[test]
    fn missing_pty_loads_none() {
        assert!(load_with(&repo(), "absent", 1_000).is_none());
    }
}
