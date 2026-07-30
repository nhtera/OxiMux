//! What each driver tool actually does, from the driver's own schemas.
//!
//! The plan for this feature was written against seven input tools. The shipped
//! driver registers roughly fifty, and several of them are not input at all yet
//! defeat an input-only grant model outright — a policy that classifies by
//! "does it have a `pid`" would wave them straight through. The table below is
//! transcribed from `cua-driver list-tools` / `describe`, with the quoted
//! phrases that decided each judgement kept next to it.
//!
//! **Unlisted tools are refused.** A future driver release adding a tool must
//! not silently widen what an agent can do; the cost of that choice is that a
//! driver upgrade can refuse something harmless until the table is updated,
//! which is the right way round.

/// What a tool does, for policy purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Delivers input to a target process. Needs an addressed, granted pid.
    Input,
    /// Reads state. Window-scoped or metadata only — no grant required.
    Read,
    /// Moves or restyles the agent-cursor overlay. Not a system input event,
    /// so it has no target to attribute and needs no grant.
    Overlay,
    /// Allowed only with the user's say-so; left to the permission card.
    Consent,
    /// Never allowed, for the stated reason.
    Forbidden(Refusal),
}

/// Why a tool is refused outright. Each is surfaced to the user, so they say
/// what the tool would have done rather than that a check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Re-invokes recorded tool calls "via the same dispatch path an MCP / CLI
    /// call uses" — every replayed action skips `can_use_tool` entirely. One
    /// approval would stand in for an unbounded number of unreviewed actions,
    /// so this single tool voids the whole enforcement model.
    ReplaysPastActions,
    /// "This DOES steal foreground." The premise of the feature is that agents
    /// drive apps while the user keeps working; a tool whose entire purpose is
    /// to take the foreground and hold it breaks that.
    StealsFocus,
    /// `kill -9` by pid, "Unsaved state is lost". Not input, not reversible,
    /// and equally able to target the user's editor or OxiMux itself.
    KillsProcesses,
    /// Captures the whole display. The session is pinned to window scope
    /// precisely so an agent sees its own target and not the user's screen.
    CapturesWholeScreen,
    /// Writes window screenshots — optionally full-display video — into a
    /// directory the agent picks. A capture path out of the machine.
    RecordsScreen,
    /// Changes the driver's own configuration, which every other client on the
    /// shared daemon then inherits.
    ReconfiguresDriver,
    /// Downloads and runs an installer.
    InstallsSoftware,
    /// Session scope is OxiMux's to set. An agent that can start, end, or
    /// escalate its own session can pick its own capture policy, which is what
    /// the pinning exists to prevent.
    ManagesSessions,
    /// Addressed by an opaque browser `ref` rather than a pid, so a call cannot
    /// be attributed to any grant. A parallel input surface that needs its own
    /// model rather than a guess.
    UnattributableBrowserSurface,
    /// Not in the table.
    Unrecognised,
}

impl Refusal {
    /// A sentence naming what the tool would have done. Written for the user,
    /// not the log — this is what lands in the transcript.
    pub fn reason(self) -> &'static str {
        match self {
            Self::ReplaysPastActions => {
                "replays previously recorded actions without approving each one"
            }
            Self::StealsFocus => "takes the foreground away from you",
            Self::KillsProcesses => "force-quits a process, losing unsaved work",
            Self::CapturesWholeScreen => "captures the whole screen, not just its own window",
            Self::RecordsScreen => "records the screen to a file",
            Self::ReconfiguresDriver => "changes screen-control settings for every app on the machine",
            Self::InstallsSoftware => "installs software",
            Self::ManagesSessions => "changes its own screen-control permissions",
            Self::UnattributableBrowserSurface => {
                "drives a browser through a channel OxiMux cannot attribute to an app"
            }
            Self::Unrecognised => "is not a screen-control action OxiMux recognises",
        }
    }
}

/// Every tool the driver registers, and what it is.
///
/// A table rather than a `match` because two things read it: the runtime policy
/// via [`classify`], and the spawn-time deny list via [`forbidden_names`]. A
/// second hand-written list of "dangerous tools" would drift from this one on
/// the first driver release, and the drift would be silent in the direction
/// that matters — a tool dropped from the deny list is a tool the agent can
/// call.
const TOOLS: &[(&str, ToolClass)] = {
    use Refusal::*;
    use ToolClass::*;
    &[
        // Input: delivered to a target process, each accepting `pid`.
        ("click", Input),
        ("right_click", Input),
        ("double_click", Input),
        ("drag", Input),
        ("scroll", Input),
        ("hotkey", Input),
        ("press_key", Input),
        ("type_text", Input),
        ("set_value", Input),
        // Reads. Window-scoped or pure metadata.
        ("get_window_state", Read),
        ("get_accessibility_tree", Read),
        ("list_windows", Read),
        ("list_apps", Read),
        ("get_screen_size", Read),
        ("get_cursor_position", Read),
        ("zoom", Read),
        ("check_permissions", Read),
        ("health_report", Read),
        ("get_config", Read),
        ("get_recording_state", Read),
        ("get_session_state", Read),
        ("get_agent_cursor_state", Read),
        ("get_browser_state", Read),
        ("check_for_update", Read),
        // The agent cursor: an overlay OxiMux wants drawn, not an input event.
        // `set_agent_cursor_enabled` is here rather than forbidden because
        // showing a cursor is fine; hiding one is caught on the field, since
        // the tool is the same either way.
        ("move_cursor", Overlay),
        // `set_agent_cursor_theme` selects "an already-installed cursor theme"
        // by id — no path, no download, and scoped to the caller's own session.
        // `set_agent_cursor_style` is the name it carried before 0.14, kept
        // because the supported floor is older than the rename.
        ("set_agent_cursor_theme", Overlay),
        ("set_agent_cursor_style", Overlay),
        ("set_agent_cursor_motion", Overlay),
        ("set_agent_cursor_enabled", Overlay),
        // Starts a process the agent will then want to drive. Reasonable in the
        // build-and-run workflow, but it takes a bundle id and extra argv, so
        // it is the user's call and not a silent one.
        ("launch_app", Consent),
        ("replay_trajectory", Forbidden(ReplaysPastActions)),
        ("bring_to_front", Forbidden(StealsFocus)),
        ("kill_app", Forbidden(KillsProcesses)),
        ("get_desktop_state", Forbidden(CapturesWholeScreen)),
        ("start_recording", Forbidden(RecordsScreen)),
        ("stop_recording", Forbidden(RecordsScreen)),
        ("set_config", Forbidden(ReconfiguresDriver)),
        ("install_ffmpeg", Forbidden(InstallsSoftware)),
        ("start_session", Forbidden(ManagesSessions)),
        ("end_session", Forbidden(ManagesSessions)),
        ("escalate_session", Forbidden(ManagesSessions)),
        // The browser family is matched by prefix in `classify` so a future
        // `browser_*` addition is refused with an accurate reason. These are the
        // ones that exist today, listed so they reach the deny list too — a
        // prefix rule cannot be expressed as a CLI flag.
        ("page", Forbidden(UnattributableBrowserSurface)),
        ("browser_click", Forbidden(UnattributableBrowserSurface)),
        ("browser_dialog", Forbidden(UnattributableBrowserSurface)),
        ("browser_download", Forbidden(UnattributableBrowserSurface)),
        ("browser_navigate", Forbidden(UnattributableBrowserSurface)),
        ("browser_pointer", Forbidden(UnattributableBrowserSurface)),
        ("browser_prepare", Forbidden(UnattributableBrowserSurface)),
        ("browser_set_input_files", Forbidden(UnattributableBrowserSurface)),
        ("browser_type", Forbidden(UnattributableBrowserSurface)),
    ]
};

/// Classify a bare driver tool name (`click`, not `mcp__computer-use__click`).
pub fn classify(tool: &str) -> ToolClass {
    if let Some((_, class)) = TOOLS.iter().find(|(name, _)| *name == tool) {
        return *class;
    }
    // Not in the table. A `browser_*` name gets the accurate reason rather than
    // the generic one; everything else fails closed.
    if tool.starts_with("browser_") {
        return ToolClass::Forbidden(Refusal::UnattributableBrowserSurface);
    }
    ToolClass::Forbidden(Refusal::Unrecognised)
}

/// The bare names of every tool that is refused outright.
///
/// Feeds the spawn-time deny list. Note what it cannot carry: a tool the driver
/// adds later is absent here, so the CLI will not block it — [`classify`] still
/// refuses it at runtime, which is why the two layers are not redundant.
pub fn forbidden_names() -> impl Iterator<Item = &'static str> {
    TOOLS
        .iter()
        .filter(|(_, class)| matches!(class, ToolClass::Forbidden(_)))
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_input_tool_is_classified_as_input() {
        // The nine tools that deliver keystrokes, clicks, or values to a
        // process. Missing one means it skips the grant check entirely.
        for tool in [
            "click",
            "right_click",
            "double_click",
            "drag",
            "scroll",
            "hotkey",
            "press_key",
            "type_text",
            "set_value",
        ] {
            assert_eq!(classify(tool), ToolClass::Input, "{tool}");
        }
    }

    #[test]
    fn replay_is_forbidden_because_it_bypasses_approval() {
        // The most consequential entry in the table: a replayed action never
        // reaches the permission handler, so allowing this once would allow
        // everything it recorded.
        assert_eq!(
            classify("replay_trajectory"),
            ToolClass::Forbidden(Refusal::ReplaysPastActions)
        );
    }

    #[test]
    fn tools_that_break_the_background_promise_are_forbidden() {
        assert_eq!(
            classify("bring_to_front"),
            ToolClass::Forbidden(Refusal::StealsFocus)
        );
        assert_eq!(
            classify("get_desktop_state"),
            ToolClass::Forbidden(Refusal::CapturesWholeScreen)
        );
    }

    #[test]
    fn an_agent_cannot_manage_its_own_session_scope() {
        // Capture scope is pinned by OxiMux at session start and is immutable
        // for the session's life. All three of these would let an agent pick
        // its own instead.
        for tool in ["start_session", "end_session", "escalate_session"] {
            assert_eq!(
                classify(tool),
                ToolClass::Forbidden(Refusal::ManagesSessions),
                "{tool}"
            );
        }
    }

    #[test]
    fn the_browser_family_is_refused_as_a_group() {
        // Matched by prefix so a driver release adding `browser_something_new`
        // is refused rather than falling through to the unknown branch — same
        // verdict, but the reason stays accurate.
        for tool in [
            "browser_click",
            "browser_type",
            "browser_navigate",
            "browser_download",
            "browser_set_input_files",
            "browser_something_new",
            "page",
        ] {
            assert_eq!(
                classify(tool),
                ToolClass::Forbidden(Refusal::UnattributableBrowserSurface),
                "{tool}"
            );
        }
    }

    #[test]
    fn the_deny_list_covers_every_forbidden_tool_and_nothing_else() {
        // The property that makes one table safe to read twice: anything
        // `classify` refuses must reach the CLI deny list, and nothing the
        // policy allows may be denied there (which would break the feature
        // rather than merely fail to protect it).
        let denied: Vec<&str> = forbidden_names().collect();
        for name in &denied {
            assert!(
                matches!(classify(name), ToolClass::Forbidden(_)),
                "{name} is on the deny list but not refused by the policy"
            );
        }
        for (name, class) in TOOLS {
            assert_eq!(
                matches!(class, ToolClass::Forbidden(_)),
                denied.contains(name),
                "{name} disagrees between the table and the deny list"
            );
        }
    }

    #[test]
    fn the_deny_list_names_the_tools_that_matter_most() {
        let denied: Vec<&str> = forbidden_names().collect();
        // Spot-check the ones whose absence would be worst, including the
        // browser family — a prefix rule cannot be expressed as a CLI flag, so
        // those must be enumerated to reach the deny list at all.
        for name in [
            "replay_trajectory",
            "kill_app",
            "bring_to_front",
            "get_desktop_state",
            "browser_type",
            "page",
        ] {
            assert!(denied.contains(&name), "{name} missing from the deny list");
        }
        assert!(!denied.contains(&"click"), "input tools must stay available");
        assert!(!denied.contains(&"get_window_state"), "reads must stay available");
    }

    #[test]
    fn the_agent_cursor_family_is_overlay() {
        // The overlay is OxiMux's own attribution marker — it exists so the
        // user can see which agent is acting. Refusing one of these does not
        // block an action, it removes the thing that labels the action, so a
        // rename in the driver must land here rather than fail closed.
        for tool in [
            "move_cursor",
            "set_agent_cursor_theme",
            "set_agent_cursor_style",
            "set_agent_cursor_motion",
            "set_agent_cursor_enabled",
        ] {
            assert_eq!(classify(tool), ToolClass::Overlay, "{tool}");
        }
    }

    #[test]
    fn an_unknown_tool_fails_closed() {
        // A driver upgrade must not widen the surface on its own.
        assert_eq!(
            classify("some_future_tool"),
            ToolClass::Forbidden(Refusal::Unrecognised)
        );
        assert_eq!(classify(""), ToolClass::Forbidden(Refusal::Unrecognised));
    }

    #[test]
    fn reads_need_no_grant() {
        for tool in ["get_window_state", "list_windows", "zoom", "check_permissions"] {
            assert_eq!(classify(tool), ToolClass::Read, "{tool}");
        }
    }

    #[test]
    fn every_refusal_reads_as_a_sentence_about_the_tool() {
        // These strings land in the transcript, so they must describe the
        // action rather than the check that rejected it.
        for refusal in [
            Refusal::ReplaysPastActions,
            Refusal::StealsFocus,
            Refusal::KillsProcesses,
            Refusal::CapturesWholeScreen,
            Refusal::RecordsScreen,
            Refusal::ReconfiguresDriver,
            Refusal::InstallsSoftware,
            Refusal::ManagesSessions,
            Refusal::UnattributableBrowserSurface,
            Refusal::Unrecognised,
        ] {
            let reason = refusal.reason();
            assert!(!reason.is_empty(), "{refusal:?}");
            assert!(
                !reason.contains("policy") && !reason.contains("refus"),
                "{refusal:?} describes the check, not the action: {reason}"
            );
        }
    }
}
