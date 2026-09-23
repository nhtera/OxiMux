//! Resuming an agent's own conversation on a cold restore.
//!
//! A cockpit tab's CLI keeps its conversation on disk under an id the CLI
//! itself resumes by (`claude --resume <id>`, `codex resume <id>`, `pi
//! --session <id>`, `omp --resume <id>`). After a reboot or relay death the
//! PTY is gone, so the tab respawns the CLI — and without that id it came back
//! as a brand-new conversation under the old title. This module is the pure
//! half of closing that gap: what id to persist, what resumption to spawn
//! with, and when a rejected resume should fall back to a fresh start.
//!
//! Everything here is a function of its inputs. The id crosses a PTY byte
//! stream (a hostile process could emit anything into the OSC sideband), so
//! it is validated to a conservative charset before it is stored or handed to
//! a spawn builder — nothing here ever needs shell quoting.

use oximux_core::{AgentAdapter, AgentSnapshot, AgentStatus, SessionResumption};

/// Longest id accepted, matching the scanner's own cap on the sideband field.
const MAX_ID_LEN: usize = 64;

/// True for an id that is safe to persist and to pass straight to a CLI:
/// 1–64 bytes of `[A-Za-z0-9_.-]`. Every real id (UUIDs, Codex thread ids,
/// Pi/omp session ids) fits; anything with a slash, a space or a control
/// byte is refused rather than escaped. A leading `-` is refused too: the id
/// is the argument after `--resume` / `resume` / `--session`, where it would
/// otherwise read as a flag.
pub fn is_valid_provider_session(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The id to persist from a tab's latest status snapshot, or `None` when the
/// agent has not named one yet (no hook has fired) or what it named is not a
/// plausible id.
pub fn provider_session_from_snapshot(snap: &AgentSnapshot) -> Option<String> {
    let id = snap.detail.as_ref()?.session_id.as_deref()?.trim();
    is_valid_provider_session(id).then(|| id.to_owned())
}

/// True when `adapter` has a resume shape in its spawn builder. `Custom` has
/// no builder at all (its argv is the user's own), so it is never resumed.
pub fn adapter_can_resume(adapter: AgentAdapter) -> bool {
    matches!(
        adapter,
        AgentAdapter::ClaudeCode | AgentAdapter::Codex | AgentAdapter::Pi | AgentAdapter::Omp
    )
}

/// How a cold-restored cockpit tab should spawn its CLI: continue the
/// persisted conversation when there is a valid one and the adapter can, a
/// fresh session otherwise. Always `Resume`, never `Fork` — a restore puts the
/// user back in the same conversation; forking is the explicit Fork action.
pub fn restore_resumption(adapter: AgentAdapter, provider_session: Option<&str>) -> SessionResumption {
    match provider_session.map(str::trim) {
        Some(id) if adapter_can_resume(adapter) && is_valid_provider_session(id) => {
            SessionResumption::Resume { id: id.to_owned() }
        }
        _ => SessionResumption::None,
    }
}

/// The command a cold-restored plain terminal pre-types for a hand-typed
/// agent, from the process-scan label recorded with the ambient reading and
/// the session id its hooks reported. Spellings mirror each adapter's spawn
/// builder (`crates/agents/src/cli/*.rs`); the id is validated so the line
/// never needs quoting. `None` for an agent without a known resume shape.
pub fn resume_shell_line(agent_label: &str, session_id: &str) -> Option<String> {
    let id = session_id.trim();
    if !is_valid_provider_session(id) {
        return None;
    }
    Some(match agent_label {
        "Claude Code" => format!("claude --resume {id}"),
        "Codex" => format!("codex resume {id}"),
        "omp" => format!("omp --resume {id}"),
        "Pi" => format!("pi --session {id}"),
        _ => return None,
    })
}

/// How long a resumed tab waits for its verdict before it claims its rail row
/// anyway. A CLI handed an id it has no transcript for normally exits within a
/// second or two, so the common rejection is settled before the row is ever
/// claimed and the fresh session claims it directly; a later rejection repoints
/// the claimed row instead (see `WorkspaceRoot::repoint_live_agent`).
pub const ROW_CLAIM_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a resumed CLI may stay silent before its exit stops counting as a
/// rejection. The verdict normally settles on the first hook event; this
/// ceiling only matters when no hook ever arrives (status hooks turned off), so
/// that an hour-old session that crashes is shown as failed, not quietly
/// replaced by a fresh one.
pub const RESUME_VERDICT_CEILING: std::time::Duration = std::time::Duration::from_secs(10 * 60);

// The restore path waits `CEILING - GRACE` after the claim: it must not underflow.
const _: () = assert!(RESUME_VERDICT_CEILING.as_secs() > ROW_CLAIM_GRACE.as_secs());

/// True when a status observed before the resumed CLI has reported anything
/// means it rejected the id, so the tab should respawn fresh — exactly once.
/// Only a failed exit counts: a clean exit is the user quitting, and silence
/// (a slow start, a CLI parked on its own startup prompt) never triggers, so a
/// slow machine is never killed mid-start.
pub fn should_fallback(status: &AgentStatus) -> bool {
    match status {
        AgentStatus::Failed(_) => true,
        AgentStatus::Done { code: Some(code) } => *code != 0,
        _ => false,
    }
}

/// True once a hook event proves the resumed CLI is past its startup: it
/// reported a prompt, a tool, or a reply. Output alone never counts — a CLI
/// can paint a startup menu (an update notice, a trust prompt) before it even
/// looks at the id, and only rejects it once that menu is dismissed. The
/// session id alone does not count either: a resumed session is seeded with
/// the id it was spawned on, before any hook has fired.
pub fn agent_has_reported(snapshot: &AgentSnapshot) -> bool {
    snapshot.detail.as_ref().is_some_and(|d| {
        d.prompt.is_some() || d.tool_name.is_some() || d.last_message.is_some()
    })
}

/// How a resumed CLI's startup ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeVerdict {
    /// It exited failed before reporting anything: the id was refused. Carries
    /// the withheld exit, for the caller to publish if the fallback itself
    /// cannot start — a failure must never be swallowed.
    Rejected(AgentSnapshot),
    /// It reported a hook event, or ended in a way that is not a rejection.
    Settled,
}

/// Forward the resumed session's snapshots from `inner` to `proxy` until the
/// verdict is known. The rejecting exit itself is never forwarded, so nothing
/// reading `proxy` — the tab's watcher, the rail row — ever sees a refused
/// resume as a failure; the fallback carries on the same proxy with the fresh
/// session. Cancel-safe: the only await is `changed()`, and every pass
/// re-sends the current value, so a dropped call resumes cleanly.
pub async fn forward_until_verdict(
    inner: &mut oximux_agents::AgentStatusStream,
    proxy: &tokio::sync::watch::Sender<AgentSnapshot>,
) -> ResumeVerdict {
    loop {
        let snapshot = inner.borrow_and_update().clone();
        if should_fallback(&snapshot.status) {
            return ResumeVerdict::Rejected(snapshot);
        }
        let settled = agent_has_reported(&snapshot) || snapshot.status.is_terminal();
        proxy.send_replace(snapshot);
        if settled || inner.changed().await.is_err() {
            return ResumeVerdict::Settled;
        }
    }
}

/// [`forward_until_verdict`] bounded by `limit`: `None` when it is still
/// undecided at the deadline.
pub async fn verdict_within(
    inner: &mut oximux_agents::AgentStatusStream,
    proxy: &tokio::sync::watch::Sender<AgentSnapshot>,
    executor: &gpui::BackgroundExecutor,
    limit: std::time::Duration,
) -> Option<ResumeVerdict> {
    let verdict = std::pin::pin!(forward_until_verdict(inner, proxy));
    let deadline = std::pin::pin!(executor.timer(limit));
    match futures::future::select(verdict, deadline).await {
        futures::future::Either::Left((verdict, _)) => Some(verdict),
        futures::future::Either::Right(_) => None,
    }
}

/// Forward every snapshot from `inner` to `proxy` until the session's sender
/// is gone. The proxy's own sender drops with the caller afterwards, which is
/// what ends the readers' loops, exactly as the direct stream would have.
pub async fn forward(
    mut inner: oximux_agents::AgentStatusStream,
    proxy: &tokio::sync::watch::Sender<AgentSnapshot>,
) {
    loop {
        proxy.send_replace(inner.borrow_and_update().clone());
        if inner.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::SidebandDetail;

    const UUID: &str = "019f650c-a70a-77c4-8fa4-f81e6e6ad1f3";

    fn snap(session_id: Option<&str>) -> AgentSnapshot {
        AgentSnapshot {
            status: AgentStatus::Idle,
            detail: Some(SidebandDetail {
                session_id: session_id.map(str::to_owned),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn a_uuid_is_accepted_and_hostile_ids_are_refused() {
        assert!(is_valid_provider_session(UUID));
        assert!(is_valid_provider_session("thread_01ABC.xyz-9"));
        assert!(!is_valid_provider_session(""));
        assert!(!is_valid_provider_session("   "));
        assert!(!is_valid_provider_session("../x"));
        assert!(!is_valid_provider_session("a b"));
        assert!(!is_valid_provider_session("a;rm"));
        assert!(!is_valid_provider_session("x\u{7}y"));
        assert!(!is_valid_provider_session("--dangerously-skip-permissions"));
        assert!(!is_valid_provider_session("-x"));
        assert!(is_valid_provider_session(&"a".repeat(MAX_ID_LEN)));
        assert!(!is_valid_provider_session(&"a".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn snapshot_id_is_trimmed_validated_or_absent() {
        assert_eq!(provider_session_from_snapshot(&snap(Some(UUID))).as_deref(), Some(UUID));
        assert_eq!(
            provider_session_from_snapshot(&snap(Some(&format!("  {UUID} ")))).as_deref(),
            Some(UUID)
        );
        assert!(provider_session_from_snapshot(&snap(Some("../x"))).is_none());
        assert!(provider_session_from_snapshot(&snap(None)).is_none());
        assert!(provider_session_from_snapshot(&AgentSnapshot::from_status(AgentStatus::Idle)).is_none());
    }

    #[test]
    fn every_adapter_resumes_only_with_a_valid_id_and_custom_never_does() {
        let resumable = [
            AgentAdapter::ClaudeCode,
            AgentAdapter::Codex,
            AgentAdapter::Pi,
            AgentAdapter::Omp,
        ];
        for adapter in resumable {
            assert_eq!(
                restore_resumption(adapter, Some(UUID)),
                SessionResumption::Resume { id: UUID.into() },
                "{adapter:?} resumes a valid id"
            );
            assert_eq!(restore_resumption(adapter, None), SessionResumption::None);
            assert_eq!(restore_resumption(adapter, Some("../x")), SessionResumption::None);
            assert_eq!(restore_resumption(adapter, Some("")), SessionResumption::None);
        }
        assert_eq!(restore_resumption(AgentAdapter::Custom, Some(UUID)), SessionResumption::None);
        assert!(!adapter_can_resume(AgentAdapter::Custom));
    }

    #[test]
    fn resume_shell_line_pins_each_agents_spelling() {
        assert_eq!(
            resume_shell_line("Claude Code", UUID).as_deref(),
            Some("claude --resume 019f650c-a70a-77c4-8fa4-f81e6e6ad1f3")
        );
        assert_eq!(
            resume_shell_line("Codex", "019a2b3c").as_deref(),
            Some("codex resume 019a2b3c")
        );
        assert_eq!(resume_shell_line("omp", UUID).as_deref(), Some(&*format!("omp --resume {UUID}")));
        assert_eq!(resume_shell_line("Pi", UUID).as_deref(), Some(&*format!("pi --session {UUID}")));
        // Unknown agent, or an id that would need quoting: no line at all.
        assert!(resume_shell_line("Gemini CLI", UUID).is_none());
        assert!(resume_shell_line("Claude Code", "a b").is_none());
        assert!(resume_shell_line("Claude Code", "").is_none());
    }

    #[test]
    fn fallback_fires_only_on_a_failed_exit() {
        assert!(should_fallback(&AgentStatus::Failed("exit 1".into())));
        assert!(should_fallback(&AgentStatus::Done { code: Some(2) }));
        // Clean exit, signal death, still running, or interrupted: never.
        assert!(!should_fallback(&AgentStatus::Done { code: Some(0) }));
        assert!(!should_fallback(&AgentStatus::Done { code: None }));
        assert!(!should_fallback(&AgentStatus::Running));
        assert!(!should_fallback(&AgentStatus::Idle));
        assert!(!should_fallback(&AgentStatus::Interrupted));
    }

    fn reported(prompt: Option<&str>, tool: Option<&str>, message: Option<&str>) -> AgentSnapshot {
        AgentSnapshot {
            status: AgentStatus::Running,
            detail: Some(SidebandDetail {
                session_id: Some(UUID.into()),
                prompt: prompt.map(str::to_owned),
                tool_name: tool.map(str::to_owned),
                last_message: message.map(str::to_owned),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn only_a_hook_event_counts_as_the_agent_reporting() {
        assert!(agent_has_reported(&reported(Some("hi"), None, None)));
        assert!(agent_has_reported(&reported(None, Some("Edit"), None)));
        assert!(agent_has_reported(&reported(None, None, Some("done"))));
        // The seeded id, or output with no detail at all, is not a report.
        assert!(!agent_has_reported(&snap(Some(UUID))));
        assert!(!agent_has_reported(&AgentSnapshot::from_status(AgentStatus::Running)));
    }

    #[test]
    fn a_rejection_behind_a_startup_menu_is_caught_and_never_forwarded() {
        // The CLI paints a menu (output, no hook), sits there, then exits
        // failed once the menu is dismissed — however long that took.
        let (tx, mut inner) = tokio::sync::watch::channel(snap(Some(UUID)));
        let (proxy, proxy_rx) = tokio::sync::watch::channel(snap(Some(UUID)));
        tx.send_replace(AgentSnapshot::from_status(AgentStatus::Running));
        tx.send_replace(AgentSnapshot::from_status(AgentStatus::Failed("exit 1".into())));
        let verdict = futures::executor::block_on(forward_until_verdict(&mut inner, &proxy));
        assert!(matches!(verdict, ResumeVerdict::Rejected(ref s) if should_fallback(&s.status)));
        assert!(!should_fallback(&proxy_rx.borrow().status), "the refusal must not reach readers");
    }

    #[test]
    fn a_failed_exit_after_the_first_hook_is_the_users_to_see() {
        let (tx, mut inner) = tokio::sync::watch::channel(reported(Some("hi"), None, None));
        let (proxy, proxy_rx) = tokio::sync::watch::channel(snap(Some(UUID)));
        let verdict = futures::executor::block_on(forward_until_verdict(&mut inner, &proxy));
        assert_eq!(verdict, ResumeVerdict::Settled);
        assert_eq!(proxy_rx.borrow().detail.as_ref().unwrap().prompt.as_deref(), Some("hi"));
        // From here everything is forwarded, a failure included.
        tx.send_replace(AgentSnapshot::from_status(AgentStatus::Failed("exit 1".into())));
        drop(tx);
        futures::executor::block_on(forward(inner, &proxy));
        assert!(should_fallback(&proxy_rx.borrow().status));
    }

    #[test]
    fn a_clean_exit_before_any_hook_settles_without_a_fallback() {
        let (tx, mut inner) = tokio::sync::watch::channel(snap(Some(UUID)));
        let (proxy, proxy_rx) = tokio::sync::watch::channel(snap(Some(UUID)));
        tx.send_replace(AgentSnapshot::from_status(AgentStatus::Done { code: Some(0) }));
        let verdict = futures::executor::block_on(forward_until_verdict(&mut inner, &proxy));
        assert_eq!(verdict, ResumeVerdict::Settled);
        assert_eq!(proxy_rx.borrow().status, AgentStatus::Done { code: Some(0) });
    }

}
