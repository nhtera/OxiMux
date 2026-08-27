//! What `oximux agent-status` decides, separated from how it delivers it.
//!
//! An agent's hook runs a command and hands it the event as JSON on stdin. All
//! of what that command has to work out — which dialect's payload shape it is
//! reading, whether this event should report anything at all, and what the
//! resulting sideband blob says — is decided here, from arguments and bytes.
//!
//! It lives apart from the send for two reasons. The delivery half needs a
//! tokio runtime, the relay client and the data directory, none of which this
//! half has any use for; and both the desktop app and `oximux-cli` run this
//! verb, so a second copy of the decision would be a second set of dialect
//! bugs. What each binary keeps is the ~20 lines that open its own socket.
//!
//! Everything here is a pure function of its inputs — no env, no stdin, no
//! clock — which is why it can be tested at all. The version that lived inside
//! the desktop's `main.rs` read all three directly and had no tests.

use crate::agent_hook_dialects::{self, HookDialect};
use crate::agent_status_hooks;

/// The three states this hook is allowed to report. Anything else is a
/// mis-installed hook, and saying so beats reporting a state the scanner will
/// silently drop.
const STATES: [&str; 3] = ["working", "needs_approval", "idle"];

/// A validated `agent-status` invocation: which agent's payload shape to read,
/// what to report, and whether this particular event has to prove itself first.
pub struct StatusArgs {
    dialect: &'static HookDialect,
    state: String,
    /// Gate a `Notification` hook: report only when the payload is a real
    /// permission prompt. Claude also fires `Notification` for a benign
    /// "waiting for your input" nudge, which must not turn the dot amber.
    filter_notification: bool,
}

/// Hand-written rather than derived: `HookDialect` holds function pointers and
/// the whole static table, none of which belongs in a test failure message.
/// Only the three things that were actually parsed are worth printing.
impl std::fmt::Debug for StatusArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusArgs")
            .field("format", &self.dialect.slug)
            .field("state", &self.state)
            .field("filter_notification", &self.filter_notification)
            .finish()
    }
}

impl StatusArgs {
    /// Parse the flags the hook was installed with.
    ///
    /// `args` is everything after the `agent-status` verb itself. Unknown flags
    /// are ignored rather than refused: an entry written by a newer OxiMux and
    /// run by an older binary should still report the state it does understand,
    /// not fail the agent's turn over a flag it has never heard of.
    ///
    /// `Err` carries the message to print — the caller owns stderr, because the
    /// two binaries spell their own name differently in it.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut state = String::new();
        // Claude's entries carry no `--format`: they are byte-shared with the
        // per-spawn `--settings` injection, which predates the flag. Its
        // dialect is therefore the default rather than a required argument.
        let mut format = String::from("claude");
        let mut filter_notification = false;
        let mut args = args.into_iter();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--state" => state = args.next().unwrap_or_default(),
                "--format" => format = args.next().unwrap_or_default(),
                "--filter-notification" => filter_notification = true,
                _ => {}
            }
        }
        let Some(dialect) = agent_hook_dialects::dialect_for_slug(&format) else {
            return Err(format!(
                "--format must be {} (got {format:?})",
                agent_hook_dialects::known_slugs()
            ));
        };
        if !STATES.contains(&state.as_str()) {
            return Err(format!(
                "--state must be {} (got {state:?})",
                STATES.join("|")
            ));
        }
        Ok(Self {
            dialect,
            state,
            filter_notification,
        })
    }

    /// The sideband payload this event should report, or `None` when it should
    /// report nothing.
    ///
    /// `None` is a success, not a failure: a `Notification` that turned out to
    /// be a benign nudge has nothing to say, and neither does a hook that fired
    /// outside any OxiMux pane (a plain shell, where `pty_id` is absent). Both
    /// must leave the agent's turn alone rather than failing it.
    ///
    /// `stdin_json` is the event as the agent handed it over — `""` where the
    /// event carries no body.
    pub fn payload(&self, stdin_json: &str) -> Option<String> {
        if self.filter_notification && !agent_status_hooks::notification_is_permission(stdin_json) {
            return None;
        }
        // A turn-start event carries the user's prompt and no tool; the other
        // working events carry a tool and no prompt. Both are read off the same
        // JSON, and one of them is `None` for any given hook.
        let tool = agent_hook_dialects::tool_name(stdin_json);
        let prompt = agent_hook_dialects::prompt(stdin_json);
        // The agent's last reply, read only on the turn-end event: it fires
        // once per turn, where a per-tool read would repeat the work for
        // nothing. The agents split on how they hand it over — some put the
        // reply on the payload, others only a transcript path to chase — which
        // is what the dialect settles, so one agent's row reads like another's.
        let message = (self.state == "idle")
            .then(|| agent_hook_dialects::last_message(self.dialect, stdin_json))
            .flatten();
        Some(agent_status_hooks::build_status_payload(
            &self.state,
            tool.as_deref(),
            prompt.as_deref(),
            message.as_deref(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn decode(payload: &str) -> Value {
        serde_json::from_str(payload).expect("payload is JSON")
    }

    #[test]
    fn a_state_outside_the_three_is_refused_rather_than_reported() {
        // Not a pedantic check: the scanner drops an unknown state silently, so
        // a typo'd hook would install, run, and report nothing forever with no
        // symptom but a row that never updates.
        let err = StatusArgs::parse(args(&["--state", "runnning"])).expect_err("must refuse");
        assert!(err.contains("--state"), "{err}");
        assert!(err.contains("runnning"), "the bad value must be quoted back: {err}");
        for good in STATES {
            assert!(StatusArgs::parse(args(&["--state", good])).is_ok(), "{good}");
        }
    }

    #[test]
    fn an_unknown_format_is_refused_and_names_the_ones_that_exist() {
        let err = StatusArgs::parse(args(&["--state", "idle", "--format", "clyde"]))
            .expect_err("must refuse");
        assert!(err.contains("clyde"), "{err}");
        assert!(err.contains("claude"), "must list the real slugs: {err}");
    }

    #[test]
    fn format_defaults_to_claude_because_its_entries_carry_no_flag() {
        // Claude's hook command is byte-shared with the per-spawn `--settings`
        // injection, which has no `--format`. Requiring the flag would break
        // every Claude hook already installed.
        let parsed = StatusArgs::parse(args(&["--state", "idle"])).expect("parses");
        assert_eq!(parsed.dialect.slug, "claude");
    }

    #[test]
    fn an_unknown_flag_is_ignored_so_a_newer_entry_still_reports() {
        // An entry written by a newer OxiMux, run by an older binary: it must
        // report the state it understands rather than failing the agent's turn.
        let parsed = StatusArgs::parse(args(&["--state", "working", "--future-flag", "x"]))
            .expect("must not refuse an unknown flag");
        assert_eq!(parsed.state, "working");
    }

    #[test]
    fn a_benign_notification_reports_nothing_at_all() {
        let parsed = StatusArgs::parse(args(&["--state", "needs_approval", "--filter-notification"]))
            .expect("parses");
        let nudge = r#"{"message":"Claude is waiting for your input"}"#;
        assert_eq!(parsed.payload(nudge), None, "a nudge must not turn the dot amber");
    }

    #[test]
    fn a_real_permission_prompt_passes_the_same_filter() {
        let parsed = StatusArgs::parse(args(&["--state", "needs_approval", "--filter-notification"]))
            .expect("parses");
        let ask = r#"{"message":"Claude needs your permission to use Bash"}"#;
        let payload = parsed.payload(ask).expect("a real prompt must report");
        assert_eq!(decode(&payload)["state"], "needs_approval");
    }

    #[test]
    fn the_last_reply_is_read_on_turn_end_and_nowhere_else() {
        // The expensive read happens once per turn. Asserting the negative
        // matters more than the positive: a `msg` on every tool event would be
        // a transcript read per tool call.
        let body = r#"{"last_assistant_message":"done"}"#;
        let idle = StatusArgs::parse(args(&["--state", "idle"])).expect("parses");
        assert_eq!(decode(&idle.payload(body).unwrap())["msg"], "done");

        let working = StatusArgs::parse(args(&["--state", "working"])).expect("parses");
        assert!(
            decode(&working.payload(body).unwrap()).get("msg").is_none(),
            "a working event must not chase the reply"
        );
    }

    #[test]
    fn an_empty_body_still_reports_the_state() {
        // `idle` (Stop) is installed for agents that hand over no body at all.
        // It must still report, or the row never leaves "working".
        let parsed = StatusArgs::parse(args(&["--state", "idle"])).expect("parses");
        let payload = parsed.payload("").expect("must report");
        assert_eq!(decode(&payload)["state"], "idle");
    }
}
