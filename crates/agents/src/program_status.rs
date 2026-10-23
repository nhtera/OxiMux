//! Agent status from OSC 7501 program-status reports.
//!
//! A program that speaks the protocol says outright what it is doing, which
//! beats every other source: hook sidebands arrive late or not at all when no
//! hooks are installed, and the regex table guesses from the screen. So a
//! report, once trusted, outranks both (see `StatusMachine::feed_program_status`).
//!
//! Only the root record is mapped — sub-records describe a program's internal
//! jobs, not whether it needs the user.

use oximux_core::AgentStatus;
use oximux_pty::program_status::{BlockedKind, ProgramRecord, ProgramState};

/// The status a root record means, and the line to show for it. `done` and
/// `error` end a turn, not the process, so both read as `Idle`: the terminal
/// `Done`/`Failed` states belong to process exit. A failed turn keeps its
/// reason in the message.
pub fn map_program_status(record: &ProgramRecord) -> (AgentStatus, Option<String>) {
    let msg = record.msg.clone();
    match record.state {
        ProgramState::Idle => (AgentStatus::Idle, None),
        ProgramState::Working => (AgentStatus::Running, None),
        ProgramState::Done => (AgentStatus::Idle, msg),
        ProgramState::Error => (AgentStatus::Idle, Some(format!("error: {}", msg.unwrap_or_default()))),
        ProgramState::Blocked => match record.kind {
            Some(BlockedKind::Permission) => {
                let reason = msg.clone().or_else(|| record.app.clone()).unwrap_or_default();
                (AgentStatus::NeedsApproval(reason), msg)
            }
            Some(BlockedKind::Question | BlockedKind::Auth) | None => (AgentStatus::WaitingForInput, msg),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: ProgramState, kind: Option<BlockedKind>, msg: Option<&str>) -> ProgramRecord {
        ProgramRecord {
            state,
            kind,
            progress: None,
            app: Some("tool".into()),
            title: None,
            msg: msg.map(str::to_owned),
        }
    }

    #[test]
    fn every_state_maps_as_documented() {
        use ProgramState::*;
        let cases = [
            (record(Idle, None, None), AgentStatus::Idle, None),
            (record(Working, None, None), AgentStatus::Running, None),
            (record(Done, None, Some("ok")), AgentStatus::Idle, Some("ok")),
            (record(Error, None, Some("boom")), AgentStatus::Idle, Some("error: boom")),
            (
                record(Blocked, Some(BlockedKind::Permission), Some("approve Bash: ls")),
                AgentStatus::NeedsApproval("approve Bash: ls".into()),
                Some("approve Bash: ls"),
            ),
            (record(Blocked, Some(BlockedKind::Permission), None), AgentStatus::NeedsApproval("tool".into()), None),
            (record(Blocked, Some(BlockedKind::Question), Some("q?")), AgentStatus::WaitingForInput, Some("q?")),
            (record(Blocked, Some(BlockedKind::Auth), None), AgentStatus::WaitingForInput, None),
            (record(Blocked, None, None), AgentStatus::WaitingForInput, None),
        ];
        for (rec, status, msg) in cases {
            assert_eq!(map_program_status(&rec), (status, msg.map(str::to_owned)), "{rec:?}");
        }
    }
}
