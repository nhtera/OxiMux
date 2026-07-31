//! The shape screen control leaves behind on platforms that do not have it.
//!
//! Computer use is macOS-only and a locked exclusion elsewhere (see
//! `docs/windows-port-exclusions.md`): the driver talks to the macOS
//! Accessibility APIs and there is no equivalent to port. But the chat view
//! that hosts it is one of the largest surfaces in the app, and threading
//! `#[cfg]` through its ~35 screen-control touchpoints would put platform
//! conditionals in the middle of the transcript renderer — the code most likely
//! to be edited by someone thinking about something else entirely.
//!
//! So the three macOS modules keep their names here and answer "there is no
//! such thing" to everything. That is a real answer, not a stub that pretends:
//!
//! - `is_screen_call` is a **name predicate over the transcript**, so it stays
//!   truthful rather than constant-false. A transcript written on a Mac and
//!   read here still contains screen-control calls, and labelling them
//!   correctly is the whole job of the card renderer.
//! - Everything that *acts* — deciding, approving, granting — is absent. No
//!   call can arrive to decide about, because nothing here can produce one.
//! - `connect_declaring` connects **without declaring**: the agent is spawned
//!   with no screen-control MCP server and no policing hook, which is exactly
//!   right when there is no capability to police.
//!
//! Deleting this file means putting the `cfg`s back, not gaining anything.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use gpui::{AnyElement, App, Context};
use oximux_agents::thread::{AgentConnection, ConnectSpec, ThreadEvent, ToolCall};
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

use super::AgentChatView;

/// Stand-in for `agent_chat::computer_use`.
pub(super) mod computer_use {
    use super::*;

    /// A chat's screen-control state where there is none to hold.
    ///
    /// Constructed and stored like the real one so the view's field and its two
    /// spawn paths do not need to know the difference; every method is the
    /// no-target answer.
    #[derive(Debug, Default)]
    pub(in super::super) struct ScreenControl;

    impl ScreenControl {
        pub(in super::super) fn new(_cwd: &Path) -> Self {
            Self
        }

        /// Nothing was ever asked, so nothing can be approved — and `Ok` is the
        /// honest answer rather than a refusal, because a refusal would render
        /// an error on a card that cannot exist.
        pub(in super::super) fn approve(
            &self,
            _tool_name: &str,
            _input: &Value,
        ) -> Result<(), String> {
            Ok(())
        }

        pub(in super::super) fn begin_remote_turn(&self) {}
        pub(in super::super) fn end_remote_turn(&self) {}
        pub(in super::super) fn end_turn_activity(&self) {}
    }

    /// Spawn the agent with no screen-control declaration attached.
    ///
    /// The macOS version injects an MCP server plus the hook that polices Bash
    /// for side-door routes to it. Both are omitted together — a declaration
    /// without enforcement would be worse than neither, and here there is
    /// nothing on the far end of either one.
    pub(in super::super) fn connect_declaring(
        spec: ConnectSpec,
        _chat: &ScreenControl,
        _cx: &App,
    ) -> anyhow::Result<(Arc<dyn AgentConnection>, Receiver<ThreadEvent>)> {
        oximux_agents::thread::connect(spec)
    }

    /// No grant table exists to hold stale entries. `pub` to match the macOS
    /// module: the crate root re-exports this for the boot-time sweep.
    pub fn clear_stale_screen_control_grants() {}
}

/// Stand-in for `agent_chat::screen_card`.
pub(super) mod screen_card {
    use super::*;

    /// True for a screen-control tool call — including one replayed from a
    /// transcript written on a Mac, which is why this is not `false`.
    pub(in super::super) fn is_screen_call(name: &str) -> bool {
        oximux_agent_core::screen_tools::is_computer_use_tool(name)
    }

    /// The bare tool name, minus the driver-specific phrasing the macOS build
    /// derives from its own tool table.
    pub(in super::super) fn display_name(tc: &ToolCall) -> Option<String> {
        let bare = oximux_agent_core::screen_tools::bare_tool_name(&tc.name)?;
        Some(format!("Computer use · {bare}"))
    }

    /// The target process is named by the policy module, which is macOS-only.
    pub(in super::super) fn target(_tc: &ToolCall, app: Option<&str>) -> Option<String> {
        app.map(str::to_string)
    }

    /// Verdict lines are read out of the driver's reply shape; a transcript
    /// from elsewhere still carries the raw result, which renders below.
    pub(in super::super) fn outcome(_tc: &ToolCall) -> Option<String> {
        None
    }

    pub(in super::super) fn refusal(_tc: &ToolCall) -> Option<&str> {
        None
    }
}

/// Stand-in for `agent_chat::screen_consent`.
pub(super) mod screen_consent {
    use super::*;

    /// Never constructed: a consent card is raised by the policy, and the
    /// policy is not here. The type stays so the view's prompt map and the tool
    /// card's parameter keep their shape.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in super::super) struct ScreenPrompt {
        pub app: String,
        pub bundle_id: Option<String>,
    }

    impl ScreenPrompt {
        pub(in super::super) fn question(&self, provider: &str) -> String {
            format!("Let {provider} control {}?", self.app)
        }
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(in super::super) struct ScreenContext {
        pub prompt: Option<ScreenPrompt>,
        pub app: Option<String>,
    }

    pub(in super::super) fn warning_banner(
        _prompt: &ScreenPrompt,
        _theme: Theme,
        _density: Density,
        _typo: &Typography,
    ) -> Option<AnyElement> {
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(in super::super) fn always_allow_pill(
        _prompt: &ScreenPrompt,
        _tool_id: &str,
        _request_id: &str,
        _input: &Value,
        _theme: Theme,
        _density: Density,
        _typo: &Typography,
        _cx: &mut Context<AgentChatView>,
    ) -> Option<AnyElement> {
        None
    }
}

impl AgentChatView {
    /// No policy to enforce: nothing here can issue a screen-control call, so
    /// any permission request reaching this point belongs to another tool and
    /// is answered by the normal permission path.
    pub(super) fn enforce_screen_control(
        &mut self,
        _tool_name: String,
        _input: Value,
        _request_id: String,
        _tool_id: String,
        _cx: &mut Context<Self>,
    ) {
    }

    pub(super) fn note_screen_capture(&self, _ev: &ThreadEvent) {}

    pub(super) fn screen_context(&self, _tc: &ToolCall) -> screen_consent::ScreenContext {
        screen_consent::ScreenContext::default()
    }
}
