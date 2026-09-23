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
//! A reading older than the sideband TTL is ignored on load, so a finished
//! agent doesn't resurrect on the rail across a long-gone restart. The record
//! itself lives on for seven days, because it has a second reader: after a
//! reboot the PTY is dead and the rail has nothing to seed, but the record
//! still names which agent ran there and the conversation id it resumes by,
//! so the cold-restored shell can offer the resume command
//! ([`load_for_resume`]). Callers that run before [`init`] (pure unit tests)
//! read `None` and persist nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use oximux_core::{AgentStatus, SidebandDetail};
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

/// Mirrors `ambient_agent_scan::SIDEBAND_TTL` (30 min): a persisted reading
/// older than this is stale and not re-seeded onto the rail.
const TTL_MS: u64 = 30 * 60 * 1000;

/// How long a record stays readable for a resume offer: 7 days, matching the
/// daemon's checkpoint GC, so an overnight (or week-long) shutdown still
/// offers the resume. Older records are deleted on either read.
const RESUME_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

static REPO: OnceLock<SettingsRepo> = OnceLock::new();

/// Writes are decided on the UI thread and applied on a thread pool, which
/// promises no order: a delayed save could resurrect a record just deleted,
/// or a delete could remove its replacement. Each write therefore carries a
/// ticket taken where it was decided ([`ticket`]); applying one is skipped if
/// a later ticket for the same PTY has already been applied, and writes are
/// serialized while they apply. Entries are never dropped — dropping one
/// would let a stale save through — and there is one small entry per PTY.
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);
static APPLIED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

/// Order stamp for one ambient write, taken where the write is decided
/// (before handing it to a background task).
pub fn ticket() -> u64 {
    NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
}

/// Run `op` for `pty_id` unless a later-ticketed write has already applied.
fn apply_in_order(pty_id: &str, ticket: u64, op: impl FnOnce()) {
    let mut applied = APPLIED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if applied.get(pty_id).is_some_and(|&last| last > ticket) {
        return;
    }
    op();
    applied.insert(pty_id.to_string(), ticket);
}

/// Install the settings repo used for ambient-agent persistence. Called once
/// from `state::hydrate`; later calls are ignored (first install wins).
pub fn init(repo: SettingsRepo) {
    let _ = REPO.set(repo);
}

/// Persist the latest reading for `pty_id`, with the process-scan label of
/// the agent running there (`"Claude Code"`, `"Codex"`, …; `None` when the
/// tree names no agent yet). Best-effort; failures are logged, never
/// propagated. No-op before [`init`]. `ticket` orders it against the PTY's
/// other writes ([`ticket`]).
pub fn persist(
    pty_id: &str,
    status: &AgentStatus,
    detail: &SidebandDetail,
    agent_label: Option<&str>,
    ticket: u64,
) {
    let Some(repo) = REPO.get() else { return };
    apply_in_order(pty_id, ticket, || {
        if let Err(err) = persist_with(repo, pty_id, status, detail, agent_label, now_ms()) {
            tracing::warn!(?err, pty_id, "ambient_state: persist failed");
        }
    });
}

/// Load a fresh reading for `pty_id`, or `None` when absent / stale / pre-init.
/// A record past the resume retention is deleted as a side effect.
pub fn load(pty_id: &str) -> Option<(AgentStatus, SidebandDetail)> {
    load_with(REPO.get()?, pty_id, now_ms())
}

/// What a cold-restored plain terminal can offer to resume: the agent that ran
/// in the dead PTY and the conversation id it resumes by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeOffer {
    /// Process-scan label, e.g. `"Claude Code"`.
    pub agent_label: String,
    /// The agent's own session id, as its hooks reported it.
    pub session_id: String,
}

/// The resume offer for a dead `pty_id`, or `None` when nothing was recorded,
/// the record is past the 7-day retention (then deleted), or it names no
/// agent / no session id. Unlike [`load`] this ignores the 30-min rail TTL:
/// the offer is only pre-typed, never run, so age costs nothing.
pub fn load_for_resume(pty_id: &str) -> Option<ResumeOffer> {
    load_for_resume_with(REPO.get()?, pty_id, now_ms())
}

/// Drop the reading for `pty_id` — its PTY died, was cold-restored, or its tab
/// closed. No-op before [`init`]. `ticket` orders it against the PTY's other
/// writes ([`ticket`]).
pub fn forget(pty_id: &str, ticket: u64) {
    if let Some(repo) = REPO.get() {
        apply_in_order(pty_id, ticket, || {
            let _ = repo.delete(&key(pty_id));
        });
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
    /// Process-scan label of the agent in the PTY. `#[serde(default)]` keeps
    /// records written before the field existed readable (they offer no
    /// resume, since they name no agent).
    #[serde(default)]
    agent_label: Option<String>,
}

/// Repo-explicit core of [`persist`], testable without the global handle.
fn persist_with(
    repo: &SettingsRepo,
    pty_id: &str,
    status: &AgentStatus,
    detail: &SidebandDetail,
    agent_label: Option<&str>,
    now_ms: u64,
) -> Result<(), oximux_storage::StorageError> {
    let rec = Persisted {
        status: status.clone(),
        detail: detail.clone(),
        updated_at_ms: now_ms,
        agent_label: agent_label.map(str::to_owned),
    };
    // These are plain serializable types, so serialization cannot realistically
    // fail; if it ever did, skip the write rather than invent a DB error.
    let Ok(json) = serde_json::to_string(&rec) else {
        return Ok(());
    };
    repo.set(&key(pty_id), &json)
}

/// The record for `pty_id`, unless it is past the resume retention — then it
/// is deleted and `None`. Shared by both readers so the retention rule lives
/// in one place.
fn read_within_retention(repo: &SettingsRepo, pty_id: &str, now_ms: u64) -> Option<Persisted> {
    let raw = repo.get(&key(pty_id)).ok()??;
    let rec: Persisted = serde_json::from_str(&raw).ok()?;
    if now_ms.saturating_sub(rec.updated_at_ms) > RESUME_TTL_MS {
        let _ = repo.delete(&key(pty_id));
        return None;
    }
    Some(rec)
}

/// Repo-explicit core of [`load`], testable without the global handle.
fn load_with(
    repo: &SettingsRepo,
    pty_id: &str,
    now_ms: u64,
) -> Option<(AgentStatus, SidebandDetail)> {
    let rec = read_within_retention(repo, pty_id, now_ms)?;
    // Stale for the rail, but kept: a reboot may still want the resume offer.
    if now_ms.saturating_sub(rec.updated_at_ms) > TTL_MS {
        return None;
    }
    Some((rec.status, rec.detail))
}

/// Repo-explicit core of [`load_for_resume`], testable without the global handle.
fn load_for_resume_with(repo: &SettingsRepo, pty_id: &str, now_ms: u64) -> Option<ResumeOffer> {
    let rec = read_within_retention(repo, pty_id, now_ms)?;
    Some(ResumeOffer {
        agent_label: rec.agent_label?,
        session_id: rec.detail.session_id?,
    })
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

    fn resumable(prompt: &str, session_id: &str) -> SidebandDetail {
        SidebandDetail {
            prompt: Some(prompt.to_string()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    #[test]
    fn round_trips_status_and_prompt() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Running, &detail("fix the parser"), None, 1_000)
            .expect("persist");
        let (status, d) = load_with(&r, "pty-1", 2_000).expect("loadable");
        assert_eq!(status, AgentStatus::Running);
        assert_eq!(d.prompt.as_deref(), Some("fix the parser"));
    }

    #[test]
    fn stale_reading_is_not_returned_to_the_rail_but_is_kept_for_resume() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Idle, &resumable("hi", "sid"), Some("Codex"), 1_000)
            .expect("persist");
        // Just past the rail TTL → not re-seeded, but the row survives: a
        // reboot inside the 7-day retention still wants the resume offer.
        assert!(load_with(&r, "pty-1", 1_000 + TTL_MS + 1).is_none());
        assert!(r.get(&key("pty-1")).expect("get").is_some());
        let two_days = 1_000 + 2 * DAY_MS;
        assert!(load_with(&r, "pty-1", two_days).is_none());
        assert_eq!(
            load_for_resume_with(&r, "pty-1", two_days),
            Some(ResumeOffer { agent_label: "Codex".into(), session_id: "sid".into() })
        );
        assert!(r.get(&key("pty-1")).expect("get").is_some(), "load_for_resume keeps it too");
    }

    #[test]
    fn a_record_past_the_resume_retention_is_deleted_by_either_reader() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Idle, &resumable("hi", "sid"), Some("Codex"), 1_000)
            .expect("persist");
        let eight_days = 1_000 + 8 * DAY_MS;
        assert!(load_for_resume_with(&r, "pty-1", eight_days).is_none());
        assert!(r.get(&key("pty-1")).expect("get").is_none(), "deleted on read");

        persist_with(&r, "pty-2", &AgentStatus::Idle, &resumable("hi", "sid"), Some("Codex"), 1_000)
            .expect("persist");
        assert!(load_with(&r, "pty-2", eight_days).is_none());
        assert!(r.get(&key("pty-2")).expect("get").is_none(), "deleted on read");
    }

    #[test]
    fn a_resume_offer_needs_both_an_agent_and_a_session_id() {
        let r = repo();
        // Agent known, but its hooks never named a session.
        persist_with(&r, "no-sid", &AgentStatus::Idle, &detail("hi"), Some("Claude Code"), 1_000)
            .expect("persist");
        assert!(load_for_resume_with(&r, "no-sid", 2_000).is_none());
        // Session named, but the process tree never identified the agent.
        persist_with(&r, "no-agent", &AgentStatus::Idle, &resumable("hi", "sid"), None, 1_000)
            .expect("persist");
        assert!(load_for_resume_with(&r, "no-agent", 2_000).is_none());
    }

    #[test]
    fn a_legacy_record_without_the_agent_field_still_loads() {
        let r = repo();
        let legacy = r#"{"status":"Idle","detail":{"prompt":"hi","session_id":"sid"},"updated_at_ms":1000}"#;
        r.set(&key("old"), legacy).expect("set");
        let (status, d) = load_with(&r, "old", 2_000).expect("loads");
        assert_eq!(status, AgentStatus::Idle);
        assert_eq!(d.prompt.as_deref(), Some("hi"));
        // No agent recorded → nothing to offer, but nothing breaks either.
        assert!(load_for_resume_with(&r, "old", 2_000).is_none());
    }

    #[test]
    fn forget_removes_the_entry() {
        let r = repo();
        persist_with(&r, "pty-1", &AgentStatus::Idle, &detail("hi"), None, 1_000).expect("persist");
        r.delete(&key("pty-1")).expect("delete");
        assert!(load_with(&r, "pty-1", 1_500).is_none());
    }

    #[test]
    fn missing_pty_loads_none() {
        assert!(load_with(&repo(), "absent", 1_000).is_none());
    }

    #[test]
    fn a_write_that_lands_after_a_later_one_is_skipped() {
        // A delayed save must not resurrect a record a later delete removed.
        let pty = "ticket-order-test-pty";
        let (save, delete) = (ticket(), ticket());
        let mut ran = Vec::new();
        apply_in_order(pty, delete, || ran.push("delete"));
        apply_in_order(pty, save, || ran.push("stale save"));
        let replacement = ticket();
        apply_in_order(pty, replacement, || ran.push("replacement save"));
        assert_eq!(ran, ["delete", "replacement save"]);
    }
}
