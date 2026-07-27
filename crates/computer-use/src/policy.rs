//! The decision an agent's screen-control call gets.
//!
//! Called synchronously from the permission handler, which is the one place
//! OxiMux is actually in the path: the driver runs as its own process that the
//! agent talks to directly, so nothing here sits between the agent and the
//! driver. What we get is the `can_use_tool` round-trip, and what we can
//! inspect is exactly the fields present in that call's input JSON.
//!
//! Two shapes of guard, and they are not redundant:
//!
//! - **Tool class** — some tools are refused whatever their arguments, because
//!   what they do cannot be made safe by addressing it correctly (see
//!   [`crate::tools`]).
//! - **Field validation** — for the input tools, the driver leaves `pid`
//!   *optional* and treats its absence as "the frontmost application". A grant
//!   table that only inspects `pid` when `pid` happens to be present enforces
//!   nothing at all, because omitting it is the documented way to target
//!   whatever the user is looking at. So the policy requires what the driver
//!   makes optional.

use serde_json::Value;

use crate::grants::{GrantTable, Provenance, Verdict};
use crate::proc::executable_of_pid;
use crate::session::SessionId;
use crate::tools::{classify, ToolClass};

/// What to do with a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Not a screen-control tool. The caller must leave it entirely alone —
    /// every other tool keeps whatever behaviour it has today.
    NotApplicable,
    /// Resolve as allowed without troubling the user.
    Allow,
    /// Leave the permission card up. The user decides, and an approval for an
    /// input tool records a grant for `pid`.
    Ask { pid: Option<u32> },
    /// Resolve as denied. `reason` is shown to the user and returned to the
    /// agent, so it says what was refused and why in one sentence.
    Refuse { reason: String },
}

impl Decision {
    fn refuse(reason: impl Into<String>) -> Self {
        Self::Refuse {
            reason: reason.into(),
        }
    }
}

/// Everything the decision depends on besides the call itself.
pub struct PolicyContext<'a> {
    /// The driver session id belonging to this agent.
    pub session: &'a SessionId,
    /// Shared across every agent — cross-drive detection needs the whole table.
    pub grants: &'a GrantTable,
    /// What this agent built for itself, when it has a resolvable worktree.
    pub provenance: Option<&'a Provenance>,
}

/// Decide a single tool call.
pub fn decide(tool_name: &str, input: &Value, ctx: &PolicyContext<'_>) -> Decision {
    let Some(tool) = crate::mcp::bare_tool_name(tool_name) else {
        return Decision::NotApplicable;
    };

    let class = classify(tool);
    if let ToolClass::Forbidden(forbidden) = class {
        return Decision::refuse(format!("`{tool}` {}", forbidden.reason()));
    }

    // A call may carry a session id, which selects the capture policy it runs
    // under. Ours is pinned to window scope; another agent's is not ours to
    // borrow, and an id we never issued has a policy we know nothing about.
    if let Some(claimed) = input.get("session").and_then(Value::as_str)
        && claimed != ctx.session.as_str()
    {
        return Decision::refuse(format!(
            "`{tool}` was addressed to screen-control session `{claimed}`, which does not belong to this chat"
        ));
    }

    match class {
        ToolClass::Forbidden(_) => unreachable!("handled above"),
        ToolClass::Read => Decision::Allow,
        ToolClass::Overlay => decide_overlay(tool, input),
        ToolClass::Consent => Decision::Ask { pid: None },
        ToolClass::Input => decide_input(tool, input, ctx),
    }
}

/// The agent cursor is the user's only ambient sign that something else is
/// driving the screen. Moving or restyling it is cosmetic; switching it off is
/// not, so that one field is checked rather than the tool waved through.
fn decide_overlay(tool: &str, input: &Value) -> Decision {
    if tool == "set_agent_cursor_enabled" && input.get("enabled") == Some(&Value::Bool(false)) {
        return Decision::refuse(
            "`set_agent_cursor_enabled` would hide the on-screen marker showing that an agent is in control",
        );
    }
    Decision::Allow
}

fn decide_input(tool: &str, input: &Value, ctx: &PolicyContext<'_>) -> Decision {
    // "Use desktop with no pid/window_id to type into the frontmost
    // application" — the driver's own words. There is no target to attribute,
    // and the frontmost application is by definition whatever the user is
    // working in.
    if input.get("scope").and_then(Value::as_str) == Some("desktop") {
        return Decision::refuse(format!(
            "`{tool}` asked for desktop scope, which targets whatever window you are using"
        ));
    }

    // "foreground": briefly front the window, act, restore. Even brief, it
    // takes the keyboard away mid-keystroke — the one thing this feature
    // promises not to do.
    if input.get("delivery_mode").and_then(Value::as_str) == Some("foreground") {
        return Decision::refuse(format!(
            "`{tool}` asked to come to the foreground, which would interrupt what you are doing"
        ));
    }

    let Some(pid) = input.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok())
    else {
        return Decision::refuse(format!(
            "`{tool}` did not name a target process, so it would act on whatever window is in front"
        ));
    };

    match ctx.grants.check(pid, ctx.session) {
        // No blocked-app check on this path, deliberately. A granted pid was
        // checked when it was granted, and its executable is pinned — a change
        // comes back as `Recycled` below, not as a silent swap. Re-checking
        // would spawn `codesign` on every click, and the enforcing process is
        // spawned fresh per tool call, so nothing would ever be cached.
        Verdict::Granted => Decision::Allow,
        Verdict::HeldByAnother { .. } => Decision::refuse(format!(
            "`{tool}` targeted process {pid}, which another chat is driving"
        )),
        Verdict::Unresolvable => Decision::refuse(format!(
            "`{tool}` targeted process {pid}, which is not running"
        )),
        Verdict::Recycled { .. } => Decision::refuse(format!(
            "`{tool}` targeted process {pid}, which is now a different program than the one you approved"
        )),
        Verdict::Ungranted => {
            let executable = executable_of_pid(pid);
            // Ahead of both the grant and the consent card: whether an app may
            // be driven at all is not a question of *which* chat is asking, and
            // not one a card can settle — approving "a click" into a password
            // manager is not approving what that enables. Checked here rather
            // than at the top so it costs a `codesign` spawn only for a target
            // nobody has approved yet, never on the repeated-click path.
            if executable
                .as_deref()
                .is_some_and(crate::blocked::is_blocked)
            {
                return Decision::refuse(format!(
                    "`{tool}` targeted a password manager or keychain, which agents are never allowed to drive"
                ));
            }
            // A binary this agent built in its own worktree during this session
            // is the workflow the feature exists for; asking about it every
            // time would be noise. Anything else is the user's call.
            let built_here = ctx
                .provenance
                .zip(executable)
                .is_some_and(|(prov, exe)| prov.built_this_session(&exe));
            if built_here && ctx.grants.grant(pid, ctx.session) == Verdict::Granted {
                Decision::Allow
            } else {
                Decision::Ask { pid: Some(pid) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ns(tool: &str) -> String {
        format!("mcp__oximux-computer-use__{tool}")
    }

    struct Fixture {
        session: SessionId,
        grants: GrantTable,
        /// Kept alive so the store outlives the fixture, not read directly.
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            // A store per fixture: every test here claims our own pid, the one
            // guaranteed to resolve, so a shared store would have parallel
            // tests refusing each other's grants.
            let dir = tempfile::tempdir().expect("tempdir");
            Self {
                session: SessionId::for_agent("chat-a"),
                grants: GrantTable::in_data_dir(dir.path()),
                _dir: dir,
            }
        }

        fn ctx(&self) -> PolicyContext<'_> {
            PolicyContext {
                session: &self.session,
                grants: &self.grants,
                provenance: None,
            }
        }

        fn decide(&self, tool: &str, input: Value) -> Decision {
            decide(&ns(tool), &input, &self.ctx())
        }
    }

    fn refusal(decision: &Decision) -> &str {
        match decision {
            Decision::Refuse { reason } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_from_another_server_is_left_alone() {
        // The single most important negative: everything that is not a
        // screen-control tool must keep behaving exactly as it does today.
        let f = Fixture::new();
        for tool in ["Bash", "Read", "mcp__other__click", "ExitPlanMode"] {
            assert_eq!(
                decide(tool, &json!({}), &f.ctx()),
                Decision::NotApplicable,
                "{tool}"
            );
        }
    }

    #[test]
    fn a_call_with_no_pid_is_refused() {
        // `pid` is optional at the driver and its absence means "frontmost
        // application". This is the refusal that makes the grant table mean
        // anything at all.
        let f = Fixture::new();
        let reason = refusal(&f.decide("type_text", json!({ "text": "hello" }))).to_string();
        assert!(reason.contains("did not name a target process"), "{reason}");
    }

    #[test]
    fn desktop_scope_is_refused_even_with_a_granted_pid() {
        // Scope wins over the grant: desktop scope ignores `pid` and lands on
        // the frontmost window regardless of what else the call carries.
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        let reason =
            refusal(&f.decide("click", json!({ "pid": pid, "scope": "desktop" }))).to_string();
        assert!(reason.contains("desktop scope"), "{reason}");
    }

    #[test]
    fn foreground_delivery_is_refused_even_with_a_granted_pid() {
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        let reason = refusal(&f.decide(
            "press_key",
            json!({ "pid": pid, "key": "return", "delivery_mode": "foreground" }),
        ))
        .to_string();
        assert!(reason.contains("foreground"), "{reason}");
    }

    #[test]
    fn background_delivery_on_a_granted_pid_is_allowed() {
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        assert_eq!(
            f.decide(
                "type_text",
                json!({ "pid": pid, "text": "hi", "delivery_mode": "background" })
            ),
            Decision::Allow
        );
    }

    #[test]
    fn an_ungranted_pid_asks_rather_than_refusing() {
        let f = Fixture::new();
        let pid = std::process::id();
        assert_eq!(
            f.decide("click", json!({ "pid": pid })),
            Decision::Ask { pid: Some(pid) }
        );
    }

    #[test]
    fn one_chat_cannot_drive_another_chats_process() {
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &SessionId::for_agent("chat-b"));
        let reason = refusal(&f.decide("click", json!({ "pid": pid }))).to_string();
        assert!(reason.contains("another chat"), "{reason}");
    }

    #[test]
    fn a_dead_pid_is_refused_not_asked() {
        // An unresolvable pid cannot be attributed to a program, so there is
        // nothing coherent to put in front of the user.
        let f = Fixture::new();
        let reason = refusal(&f.decide("click", json!({ "pid": u32::MAX }))).to_string();
        assert!(reason.contains("not running"), "{reason}");
    }

    #[test]
    fn a_call_claiming_another_sessions_id_is_refused() {
        // Capture scope is per-session and immutable. Borrowing a different
        // session id is how a call would escape the scope OxiMux pinned.
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        let reason = refusal(&f.decide(
            "click",
            json!({ "pid": pid, "session": "oximux-someone-else" }),
        ))
        .to_string();
        assert!(reason.contains("does not belong to this chat"), "{reason}");
    }

    #[test]
    fn a_call_carrying_our_own_session_id_is_fine() {
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        assert_eq!(
            f.decide(
                "click",
                json!({ "pid": pid, "session": f.session.as_str() })
            ),
            Decision::Allow
        );
    }

    #[test]
    fn replaying_a_trajectory_is_refused_whatever_it_carries() {
        // Refused on class alone: a replayed action is dispatched driver-side
        // and never reaches this function, so approving the replay approves
        // every action inside it sight unseen.
        let f = Fixture::new();
        let reason = refusal(&f.decide(
            "replay_trajectory",
            json!({ "dir": "/tmp/trajectory", "session": f.session.as_str() }),
        ))
        .to_string();
        assert!(reason.contains("replays"), "{reason}");
    }

    #[test]
    fn tools_that_would_take_the_screen_are_refused() {
        let f = Fixture::new();
        let pid = std::process::id();
        f.grants.grant(pid, &f.session);
        // Each of these is refused despite naming a properly granted target —
        // the class decides, not the addressing.
        for (tool, input) in [
            ("bring_to_front", json!({ "pid": pid })),
            ("kill_app", json!({ "pid": pid })),
            ("get_desktop_state", json!({})),
            ("start_recording", json!({ "output_dir": "/tmp/rec" })),
        ] {
            assert!(
                matches!(f.decide(tool, input), Decision::Refuse { .. }),
                "{tool} must be refused"
            );
        }
    }

    #[test]
    fn hiding_the_agent_cursor_is_refused_but_showing_it_is_not() {
        let f = Fixture::new();
        assert!(matches!(
            f.decide("set_agent_cursor_enabled", json!({ "enabled": false })),
            Decision::Refuse { .. }
        ));
        assert_eq!(
            f.decide("set_agent_cursor_enabled", json!({ "enabled": true })),
            Decision::Allow
        );
    }

    #[test]
    fn reading_window_state_needs_no_grant() {
        // Perception has to work before the agent has anything to ask about —
        // it is how it finds the pid in the first place.
        let f = Fixture::new();
        assert_eq!(
            f.decide("get_window_state", json!({ "pid": std::process::id() })),
            Decision::Allow
        );
    }

    #[test]
    fn a_binary_built_in_this_session_is_driven_without_asking() {
        // The workflow the feature exists for: build in a worktree, run it,
        // drive it. Our own test binary stands in for the freshly built app.
        let exe = std::env::current_exe().expect("test binary path");
        let root = exe.parent().expect("binary lives somewhere");
        let prov = Provenance::new(root, std::time::UNIX_EPOCH).expect("provenance");

        let f = Fixture::new();
        let ctx = PolicyContext {
            session: &f.session,
            grants: &f.grants,
            provenance: Some(&prov),
        };
        let pid = std::process::id();
        assert_eq!(
            decide(&ns("click"), &json!({ "pid": pid }), &ctx),
            Decision::Allow
        );
        // And the grant is recorded, so the next call does not re-derive it.
        assert_eq!(f.grants.granted_to(&f.session), vec![pid]);
    }

    #[test]
    fn provenance_does_not_override_the_unaddressed_refusals() {
        // Building the target does not buy the right to type into the frontmost
        // window — the field checks run before provenance is consulted.
        let exe = std::env::current_exe().expect("test binary path");
        let prov = Provenance::new(exe.parent().unwrap(), std::time::UNIX_EPOCH).expect("prov");
        let f = Fixture::new();
        let ctx = PolicyContext {
            session: &f.session,
            grants: &f.grants,
            provenance: Some(&prov),
        };
        for input in [
            json!({ "text": "x" }),
            json!({ "pid": std::process::id(), "scope": "desktop" }),
            json!({ "pid": std::process::id(), "delivery_mode": "foreground" }),
        ] {
            assert!(
                matches!(
                    decide(&ns("type_text"), &input, &ctx),
                    Decision::Refuse { .. }
                ),
                "{input} must still be refused"
            );
        }
    }

    #[test]
    fn every_refusal_names_the_tool_it_refused() {
        // The transcript shows these; "denied by policy" would tell the user
        // nothing about what the agent was trying to do.
        let f = Fixture::new();
        for (tool, input) in [
            ("type_text", json!({ "text": "x" })),
            ("click", json!({ "pid": std::process::id(), "scope": "desktop" })),
            ("kill_app", json!({ "pid": 1 })),
            ("browser_click", json!({ "ref": "x" })),
        ] {
            let decision = f.decide(tool, input);
            assert!(
                refusal(&decision).contains(tool),
                "{tool}: {}",
                refusal(&decision)
            );
        }
    }
}
