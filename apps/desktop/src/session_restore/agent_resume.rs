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
/// byte is refused rather than escaped.
pub fn is_valid_provider_session(id: &str) -> bool {
    !id.is_empty()
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

/// How long after spawn a resumed CLI's failure still counts as "the resume
/// was rejected" rather than "the agent ran and then died". A CLI handed an id
/// it has no transcript for exits within a second or two; a genuine session
/// that fails five seconds in is the user's to see, not ours to restart.
pub const RESUME_FALLBACK_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// True when a status observed during the resume window means the CLI
/// rejected the id, so the tab should respawn fresh — exactly once. Only a
/// failed exit counts: a clean exit is the user quitting, and silence (a slow
/// start, a CLI that errors but keeps running) never triggers, so a slow
/// machine is never killed mid-start. The window is the watcher's, not this
/// predicate's: a failure that happened early but was only noticed late (a
/// stalled background timer) still counts, because the exit itself was early.
pub fn should_fallback(status: &AgentStatus) -> bool {
    match status {
        AgentStatus::Failed(_) => true,
        AgentStatus::Done { code: Some(code) } => *code != 0,
        _ => false,
    }
}

/// Watch a freshly resumed session for a rejected id: `true` when a failed
/// exit is observed before the watcher gives up (see [`should_fallback`]),
/// `false` once the CLI is clearly running past [`RESUME_FALLBACK_WINDOW`],
/// or exited in a way that is not a rejection. Polls the watch channel on a
/// short timer rather than awaiting `changed()`, so the deadline needs no
/// select and a channel that never changes (a CLI that prints nothing) still
/// returns at the window. The failure check runs before the deadline check,
/// so a timer that stalled past the window still acts on an early exit.
pub async fn resume_rejected(
    status_rx: &oximux_agents::AgentStatusStream,
    executor: &gpui::BackgroundExecutor,
) -> bool {
    let started = std::time::Instant::now();
    loop {
        let status = status_rx.borrow().status.clone();
        if should_fallback(&status) {
            return true;
        }
        if status.is_terminal() || started.elapsed() >= RESUME_FALLBACK_WINDOW {
            return false;
        }
        executor.timer(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::SidebandDetail;
    use std::time::Duration;

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
        assert!(RESUME_FALLBACK_WINDOW >= Duration::from_secs(5));
    }
}
