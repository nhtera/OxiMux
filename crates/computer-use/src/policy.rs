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
    /// The OxiMux binary this policy is protecting.
    ///
    /// `None` means "whatever process is running this", which is right in the
    /// app and in tests and *wrong* in the gate, where the running process is
    /// the gate itself. See [`crate::blocked::blocked_reason`].
    pub host: Option<&'a std::path::Path>,
}

/// Decide a single tool call.
pub fn decide(tool_name: &str, input: &Value, ctx: &PolicyContext<'_>) -> Decision {
    // Shell first. A command that drives the GUI is a screen-control call
    // wearing different clothes, and it reaches the same APIs — see
    // [`crate::gui_scripting`] for why a shell tool inherits the grant at all,
    // and for the honest limits of catching it this way.
    if let Some(command) = shell_command(tool_name, input) {
        return decide_shell(&command);
    }

    let Some(tool) = crate::mcp::bare_tool_name(tool_name) else {
        return Decision::NotApplicable;
    };

    // Ahead of the class check, and of everything below it. This refusal is not
    // about which tool or which target — it is about nobody being at the screen
    // to watch. A remote-started turn cannot reach any of it: not the read
    // tools, not a target the agent built itself, not one the user pre-approved
    // from the desk earlier.
    if ctx.grants.is_remote_turn(ctx.session) {
        return Decision::refuse(format!(
            "`{tool}` was reached from a turn started on a paired phone. Screen control needs \
             someone at the screen to see what is being driven and to answer the consent card, \
             so it is refused for remote turns however the target is addressed."
        ));
    }

    let class = classify(tool);
    if let ToolClass::Forbidden(forbidden) = class {
        return Decision::refuse(format!("`{tool}` {}", forbidden.reason()));
    }

    // A call may carry a session id, which selects the capture policy it runs
    // under. Ours is pinned to window scope; another agent's is not ours to
    // borrow, and an id we never issued has a policy we know nothing about.
    //
    // The second sentence is not padding. Measured across the chats that have
    // used this feature, **most invent an id on their first or second call** —
    // `session` is advertised as an optional parameter, `start_session` is on
    // the spawn-time deny list, and nothing ever tells an agent what its
    // session is, so the model fills the field. Refusing is right; refusing
    // without naming the remedy cost every one of them a wasted round-trip and
    // left a failure in the transcript for something the user did not do wrong.
    if let Some(claimed) = input.get("session").and_then(Value::as_str)
        && claimed != ctx.session.as_str()
    {
        return Decision::refuse(format!(
            "`{tool}` was addressed to screen-control session `{claimed}`, which does not belong to \
             this chat. Omit the `session` field — this chat's own session is applied for you."
        ));
    }

    match class {
        ToolClass::Forbidden(_) => unreachable!("handled above"),
        ToolClass::Read => decide_read(tool, input, ctx),
        ToolClass::Overlay => decide_overlay(tool, input),
        ToolClass::Consent => Decision::Ask { pid: None },
        ToolClass::Input => decide_input(tool, input, ctx),
    }
}

/// The pid a call names, if it names one.
///
/// One reader for the field so the read path and the input path cannot come to
/// different conclusions about who a call is aimed at.
///
/// Public for the same reason it is one function: the transcript names the app a
/// call went to, and it has to mean the process the *policy* granted. A second
/// reader that disagreed — about the key, about a pid that will not fit a `u32` —
/// would put one app's name on a card that authorized another's.
pub fn addressed_pid(input: &Value) -> Option<u32> {
    input
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Shell tool names, across the agents this policy runs behind.
///
/// Listed rather than sniffed from the input shape: a tool that merely *has* a
/// `command` field is not necessarily a shell, and guessing wrong in that
/// direction puts this in the path of tools it does not own.
///
/// Public because the enforcing hook has to be *told* which tools to consult it
/// about, and a matcher with its own hand-written copy of this list is a matcher
/// that silently stops covering the shell the day a name is added here. See
/// [`crate::mcp::hook_matcher`].
pub const SHELL_TOOLS: &[&str] = &["Bash", "bash", "shell", "local_shell", "run_terminal_cmd"];

/// The command a shell tool is about to run, or `None` if this is not one.
///
/// Two shapes: a plain string (Claude's `Bash`) and an argv array (`["bash",
/// "-lc", "…"]`, as Codex sends). Joined rather than indexed, because which
/// element holds the script depends on the flags in front of it.
fn shell_command(tool_name: &str, input: &Value) -> Option<String> {
    if !SHELL_TOOLS.contains(&tool_name) {
        return None;
    }
    match input.get("command")? {
        Value::String(command) => Some(command.clone()),
        Value::Array(argv) => Some(
            argv.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// A shell command is either ordinary — in which case this must not touch it —
/// or it is GUI automation, in which case it is refused.
///
/// No `Ask` rung. A consent card for a shell command would have to ask "may this
/// command drive the screen?" without being able to name what it would drive:
/// the target of an `osascript` keystroke is whatever holds focus when it runs,
/// which is not known at approval time and is the user's own window by default.
/// There is nothing honest to put on the card.
fn decide_shell(command: &str) -> Decision {
    match crate::gui_scripting::classify_command(command) {
        None => Decision::NotApplicable,
        Some(kind) => Decision::refuse(format!("This command {}.", kind.reason())),
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

/// A read needs no grant — perception has to work before the agent has anything
/// to ask about, since a read is how it finds the pid in the first place — but
/// it is still a *targeting* question, and the driver's own schema is why.
///
/// `get_window_state` returns a screenshot of the window it names alongside the
/// element tree, and returns it by default. So on this surface "read" and
/// "photograph" are the same call, and the apps that may never be driven may
/// equally never be photographed: our own window carries the consent card the
/// agent would be answering and every other chat the user has open, and a
/// password manager's carries the credentials. Refusing the click while
/// allowing the picture would not be a smaller rule, it would be an incoherent
/// one.
///
/// Unlike the input path there is no earlier check to lean on — nothing was
/// ever granted, so nothing was ever inspected — which is why the `codesign`
/// spawn happens here, on a call that is already grabbing pixels and crossing a
/// socket. Only calls that name a pid pay it; the metadata reads name nothing.
fn decide_read(tool: &str, input: &Value, ctx: &PolicyContext<'_>) -> Decision {
    // A capture written to a path outlives the turn and walks around every
    // filter downstream of this one. In particular it defeats the transcript
    // redaction that keeps screen captures off a paired phone: that matches on
    // the tool which produced the image, and a PNG the agent reads back later
    // is an ordinary file read carrying an ordinary image.
    if input.get("screenshot_out_file").is_some() {
        return Decision::refuse(format!(
            "`{tool}` asked to write its screenshot to a file, which would keep a picture of your screen after the turn ends"
        ));
    }
    let Some(pid) = addressed_pid(input) else {
        return Decision::Allow;
    };
    match executable_of_pid(pid)
        .as_deref()
        .and_then(|exe| crate::blocked::blocked_reason(exe, ctx.host))
    {
        Some(blocked) => Decision::refuse(format!(
            "`{tool}` targeted {}. Agents are never allowed to read or capture it.",
            blocked.reason()
        )),
        None => Decision::Allow,
    }
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

    let Some(pid) = addressed_pid(input) else {
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
            if let Some(blocked) = executable
                .as_deref()
                .and_then(|exe| crate::blocked::blocked_reason(exe, ctx.host))
            {
                return Decision::refuse(format!(
                    "`{tool}` targeted {}. Agents are never allowed to drive it.",
                    blocked.reason()
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
    use std::path::Path;

    use super::*;
    use serde_json::json;

    fn ns(tool: &str) -> String {
        format!("mcp__oximux-computer-use__{tool}")
    }

    /// A live process standing in for "some app the agent wants to drive".
    ///
    /// The obvious candidate — our own pid — is not usable: OxiMux is refused
    /// outright, because an agent that can drive us can approve its own consent
    /// cards. So these tests spawn a real child and target that, which is also
    /// closer to what they claim to be testing.
    struct Target(std::process::Child);

    impl Target {
        /// `/bin/sleep`: present on every macOS, signed by Apple, and outlives
        /// any test that targets it.
        const EXECUTABLE: &'static str = "/bin/sleep";

        fn spawn() -> Self {
            Self(
                std::process::Command::new(Self::EXECUTABLE)
                    .arg("120")
                    .spawn()
                    .expect("spawn a target process"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Target {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct Fixture {
        session: SessionId,
        grants: GrantTable,
        /// Kept alive so the store outlives the fixture, not read directly.
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            // A store per fixture, so tests running in parallel cannot refuse
            // each other's grants over a shared table.
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
                // `None` is the honest value here: the test binary really is
                // the process being protected, so `current_exe()` is right.
                host: None,
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
    fn a_turn_started_on_a_phone_cannot_reach_screen_control_at_all() {
        // Every rung, including the ones that normally say yes without asking:
        // a granted target, a read tool that needs no grant, and the consent
        // path. The gate is the absence of a person, not the shape of the call.
        let f = Fixture::new();
        let target = Target::spawn();
        let pid = target.pid();
        f.grants.grant(pid, &f.session);
        f.grants.begin_remote_turn(&f.session);

        for (tool, input) in [
            ("click", json!({ "pid": pid })),
            ("type_text", json!({ "pid": pid, "text": "hi" })),
            ("get_window_state", json!({ "pid": pid })),
            ("launch_app", json!({ "bundle_id": "com.apple.Safari" })),
        ] {
            let reason = refusal(&f.decide(tool, input)).to_string();
            assert!(reason.contains("paired phone"), "{tool}: {reason}");
        }
    }

    #[test]
    fn provenance_does_not_buy_a_remote_turn_a_way_in() {
        // The rule the finding is specifically about: a phone prompt plus the
        // worktree grant would otherwise drive a binary the agent just built
        // with no card anywhere.
        let target = Target::spawn();
        let root = Path::new(Target::EXECUTABLE).parent().expect("a parent");
        let prov = Provenance::new(root, std::time::UNIX_EPOCH).expect("provenance");

        let f = Fixture::new();
        f.grants.begin_remote_turn(&f.session);
        let ctx = PolicyContext {
            session: &f.session,
            grants: &f.grants,
            provenance: Some(&prov),
            host: None,
        };
        assert!(matches!(
            decide(&ns("click"), &json!({ "pid": target.pid() }), &ctx),
            Decision::Refuse { .. }
        ));
        assert!(
            f.grants.granted_to(&f.session).is_empty(),
            "and it must not have silently taken a grant on the way"
        );
    }

    #[test]
    fn the_next_local_turn_works_again() {
        // Turn-scoped, not a session-wide kill: the user picking the same chat
        // back up at their desk must not find screen control dead.
        let f = Fixture::new();
        let target = Target::spawn();
        let pid = target.pid();

        f.grants.begin_remote_turn(&f.session);
        assert!(matches!(
            f.decide("click", json!({ "pid": pid })),
            Decision::Refuse { .. }
        ));

        f.grants.end_remote_turn(&f.session);
        assert_eq!(
            f.decide("click", json!({ "pid": pid })),
            Decision::Ask { pid: Some(pid) }
        );
    }

    #[test]
    fn one_chats_remote_turn_does_not_gag_another_chat() {
        // Sessions are independent; a phone prompt in one must not stop the
        // agent the user is actively watching at their desk.
        let f = Fixture::new();
        let phone_chat = SessionId::for_agent("chat-remote");
        f.grants.begin_remote_turn(&phone_chat);

        let target = Target::spawn();
        assert_eq!(
            f.decide("click", json!({ "pid": target.pid() })),
            Decision::Ask { pid: Some(target.pid()) }
        );
    }

    #[test]
    fn a_shell_command_is_still_judged_during_a_remote_turn() {
        // The shell path runs before the remote check and must keep its own
        // answers — an ordinary command from a phone prompt is still ordinary.
        let f = Fixture::new();
        f.grants.begin_remote_turn(&f.session);
        assert_eq!(
            decide("Bash", &json!({ "command": "cargo test" }), &f.ctx()),
            Decision::NotApplicable
        );
        assert!(matches!(
            decide("Bash", &json!({ "command": "cliclick c:1,1" }), &f.ctx()),
            Decision::Refuse { .. }
        ));
    }

    #[test]
    fn an_ordinary_shell_command_is_left_entirely_alone() {
        // The negative that costs the most to get wrong: this now sees every
        // shell call an agent makes, all day, in every project.
        let f = Fixture::new();
        for command in ["cargo test", "git status", "ls -la", "rg -n CGEventPost ."] {
            assert_eq!(
                decide("Bash", &json!({ "command": command }), &f.ctx()),
                Decision::NotApplicable,
                "{command}"
            );
        }
    }

    #[test]
    fn a_shell_command_that_drives_the_gui_is_refused() {
        // The bypass measured on this project: the accessibility grant OxiMux
        // takes for the Escape kill switch is inherited by the agent's shell,
        // in every project, whatever the per-project setting says.
        let f = Fixture::new();
        let reason = refusal(&decide(
            "Bash",
            &json!({ "command": r#"osascript -e 'tell application "System Events" to keystroke "x"'"# }),
            &f.ctx(),
        ))
        .to_string();
        assert!(reason.contains("accessibility API"), "{reason}");
        assert!(reason.contains("screen-control tools"), "{reason}");
    }

    #[test]
    fn an_argv_style_shell_tool_is_read_too() {
        // Codex sends `["bash", "-lc", "…"]` rather than a bare string, and a
        // reader that only understood one shape would enforce on one agent.
        let f = Fixture::new();
        assert!(matches!(
            decide(
                "shell",
                &json!({ "command": ["bash", "-lc", "cliclick c:10,10"] }),
                &f.ctx()
            ),
            Decision::Refuse { .. }
        ));
    }

    #[test]
    fn a_shell_refusal_never_becomes_a_consent_card() {
        // There is nothing honest to put on such a card: the target of an
        // osascript keystroke is whatever holds focus when it runs, which is
        // unknown at approval time and is the user's own window by default.
        let f = Fixture::new();
        for command in [
            r#"osascript -e 'tell app "System Events" to keystroke "x"'"#,
            "cliclick c:1,1",
            "osascript /tmp/x.scpt",
        ] {
            let decision = decide("Bash", &json!({ "command": command }), &f.ctx());
            assert!(
                matches!(decision, Decision::Refuse { .. }),
                "{command} -> {decision:?}"
            );
        }
    }

    #[test]
    fn a_tool_that_is_not_a_shell_keeps_its_own_treatment() {
        // `command` is a common field name. A tool merely having one must not
        // drag it into the shell path.
        let f = Fixture::new();
        assert_eq!(
            decide(
                "mcp__other__run",
                &json!({ "command": "cliclick c:10,10" }),
                &f.ctx()
            ),
            Decision::NotApplicable
        );
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
        let target = Target::spawn();
        let pid = target.pid();
        f.grants.grant(pid, &f.session);
        let reason =
            refusal(&f.decide("click", json!({ "pid": pid, "scope": "desktop" }))).to_string();
        assert!(reason.contains("desktop scope"), "{reason}");
    }

    #[test]
    fn foreground_delivery_is_refused_even_with_a_granted_pid() {
        let f = Fixture::new();
        let target = Target::spawn();
        let pid = target.pid();
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
        let target = Target::spawn();
        let pid = target.pid();
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
        let target = Target::spawn();
        let pid = target.pid();
        assert_eq!(
            f.decide("click", json!({ "pid": pid })),
            Decision::Ask { pid: Some(pid) }
        );
    }

    #[test]
    fn one_chat_cannot_drive_another_chats_process() {
        let f = Fixture::new();
        let target = Target::spawn();
        let pid = target.pid();
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
        let target = Target::spawn();
        let pid = target.pid();
        f.grants.grant(pid, &f.session);
        let reason = refusal(&f.decide(
            "click",
            json!({ "pid": pid, "session": "oximux-someone-else" }),
        ))
        .to_string();
        assert!(reason.contains("does not belong to this chat"), "{reason}");
    }

    #[test]
    fn the_session_refusal_tells_the_agent_what_to_do_instead() {
        // Most agents that reach this feature invent a session id on their
        // first or second call: `session` is an advertised optional parameter,
        // `start_session` is denied at spawn, and nothing hands them a real
        // one. The refusal has to close that loop or every chat pays a round
        // trip and shows the user a failure for something that is not their
        // mistake. Pinned because it reads like trimmable politeness.
        let f = Fixture::new();
        let target = Target::spawn();
        let reason = refusal(&f.decide(
            "get_window_state",
            json!({ "pid": target.pid(), "session": "agent-A" }),
        ))
        .to_string();
        assert!(reason.contains("Omit the `session` field"), "{reason}");
    }

    #[test]
    fn a_call_carrying_our_own_session_id_is_fine() {
        let f = Fixture::new();
        let target = Target::spawn();
        let pid = target.pid();
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
        let target = Target::spawn();
        let pid = target.pid();
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
        let target = Target::spawn();
        assert_eq!(
            f.decide("get_window_state", json!({ "pid": target.pid() })),
            Decision::Allow
        );
    }

    #[test]
    fn a_read_that_names_nobody_is_allowed() {
        // The discovery reads carry no target at all. Requiring one would break
        // the only way an agent has of finding a pid to ask about.
        let f = Fixture::new();
        for tool in ["list_windows", "list_apps", "get_screen_size"] {
            assert_eq!(f.decide(tool, json!({})), Decision::Allow, "{tool}");
        }
    }

    #[test]
    fn oximuxs_own_window_may_not_be_captured_either() {
        // `get_window_state` returns a screenshot of the window it names, by
        // default and alongside the tree. So refusing the click while allowing
        // the picture would leave the agent reading its own consent card and
        // every other chat the user has open.
        let f = Fixture::new();
        let reason = refusal(&f.decide(
            "get_window_state",
            json!({ "pid": std::process::id() }),
        ))
        .to_string();
        assert!(reason.contains("OxiMux itself"), "{reason}");
    }

    #[test]
    fn a_read_may_not_write_its_capture_to_a_file() {
        // A capture on disk outlives the turn and walks around every filter
        // downstream — including the transcript redaction that keeps screen
        // captures off a paired phone, which matches on the tool that produced
        // the image and cannot recognise a PNG read back later as a file.
        let f = Fixture::new();
        let target = Target::spawn();
        let reason = refusal(&f.decide(
            "get_window_state",
            json!({ "pid": target.pid(), "screenshot_out_file": "/tmp/shot.png" }),
        ))
        .to_string();
        assert!(reason.contains("file"), "{reason}");
    }

    #[test]
    fn a_read_naming_a_dead_pid_is_allowed_rather_than_refused() {
        // There is nothing to capture, so there is nothing to protect, and the
        // driver will fail the call on its own terms with a better message than
        // this layer could invent. Failing closed here would only add a second,
        // less accurate error for an ordinary race.
        let f = Fixture::new();
        assert_eq!(
            f.decide("get_window_state", json!({ "pid": u32::MAX })),
            Decision::Allow
        );
    }

    #[test]
    fn a_binary_built_in_this_session_is_driven_without_asking() {
        // The workflow the feature exists for: build in a worktree, run it,
        // drive it. The spawned child stands in for the freshly built app, and
        // the directory it lives in for the worktree that produced it.
        let target = Target::spawn();
        let root = Path::new(Target::EXECUTABLE)
            .parent()
            .expect("the target lives somewhere");
        let prov = Provenance::new(root, std::time::UNIX_EPOCH).expect("provenance");

        let f = Fixture::new();
        let ctx = PolicyContext {
            session: &f.session,
            grants: &f.grants,
            provenance: Some(&prov),
            host: None,
        };
        let pid = target.pid();
        assert_eq!(
            decide(&ns("click"), &json!({ "pid": pid }), &ctx),
            Decision::Allow
        );
        // And the grant is recorded, so the next call does not re-derive it.
        assert_eq!(f.grants.granted_to(&f.session), vec![pid]);
    }

    #[test]
    fn oximux_is_refused_however_it_is_addressed() {
        // The consent model assumes the user answers the card. An agent that
        // can click our own window answers it for them, so this refusal has to
        // survive a correctly addressed, background-delivered, window-scoped
        // call — the shape every other check waves through.
        let f = Fixture::new();
        let reason = refusal(&f.decide(
            "click",
            json!({ "pid": std::process::id(), "scope": "window", "delivery_mode": "background" }),
        ))
        .to_string();
        assert!(reason.contains("OxiMux itself"), "{reason}");
        assert!(f.grants.granted_to(&f.session).is_empty());
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
            host: None,
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
