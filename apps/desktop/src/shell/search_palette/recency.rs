//! Recency for the search palette. In-memory only — nothing is persisted.
//!
//! Tabs carry their own stamp (`PaneGroupTab::last_focused_ms`, set in
//! `PaneGroup::bump_mru`, which has no app context to reach a shared map).
//! Worktrees are stamped here, from `WorkspaceRoot::activate_workspace` — the
//! one choke point every worktree visit (rail click, palette, back/forward,
//! open-existing) goes through. Anything never stamped this run falls back to
//! the rail's agent-session time, then the row's creation time.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};

/// Worktree visit stamps for one window, keyed by workspace id.
#[derive(Debug, Default)]
pub struct RecencyLedger {
    worktrees: HashMap<String, i64>,
}

impl RecencyLedger {
    pub fn stamp_worktree(&mut self, workspace_id: &str, now_ms: i64) {
        self.worktrees.insert(workspace_id.to_string(), now_ms);
    }

    pub fn worktree_ms(&self, workspace_id: &str) -> Option<i64> {
        self.worktrees.get(workspace_id).copied()
    }
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Unix ms of an RFC-3339 timestamp; `None` when empty or unparseable.
pub fn rfc3339_ms(ts: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts).ok().map(|t| t.timestamp_millis())
}

/// RFC-3339 rendering of unix ms — the shape `relative_age_compact` reads, so
/// the palette shares the rail's age formatter instead of adding a second one.
pub fn ms_to_rfc3339(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

/// Seed for something with no stamp this run: the rail's newest agent-session
/// time (`rail_last_active`), else the row's `created_at`, else 0 (unknown —
/// sorts last).
pub fn fallback_ms(rail_last_active: Option<&str>, created_at: &str) -> i64 {
    rail_last_active
        .and_then(rfc3339_ms)
        .or_else(|| rfc3339_ms(created_at))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_prefers_session_time_over_creation() {
        let session = "2026-09-01T10:00:00Z";
        let created = "2026-01-01T00:00:00Z";
        assert_eq!(fallback_ms(Some(session), created), rfc3339_ms(session).unwrap());
    }

    #[test]
    fn fallback_uses_creation_when_session_missing_or_bad() {
        let created = "2026-01-01T00:00:00Z";
        let want = rfc3339_ms(created).unwrap();
        assert_eq!(fallback_ms(None, created), want);
        assert_eq!(fallback_ms(Some("not a date"), created), want);
    }

    #[test]
    fn fallback_is_zero_when_nothing_parses() {
        assert_eq!(fallback_ms(None, ""), 0);
    }

    #[test]
    fn ledger_stamp_overwrites() {
        let mut l = RecencyLedger::default();
        assert_eq!(l.worktree_ms("w"), None);
        l.stamp_worktree("w", 5);
        l.stamp_worktree("w", 9);
        assert_eq!(l.worktree_ms("w"), Some(9));
    }

    #[test]
    fn ms_round_trips_through_rfc3339() {
        let ms = 1_790_000_000_123;
        assert_eq!(rfc3339_ms(&ms_to_rfc3339(ms)), Some(ms));
    }
}
