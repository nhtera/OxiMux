//! The shape screen control leaves behind on platforms that do not have it.
//!
//! Computer use is macOS-only for now (see `docs/windows-port-exclusions.md`).
//! But the chat view that hosts it is one of the largest surfaces in the app,
//! and threading `#[cfg]` through its ~35 screen-control touchpoints would put
//! platform conditionals in the middle of the transcript renderer — the code
//! most likely to be edited by someone thinking about something else entirely.
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
//!
//! Deleting this file means putting the `cfg`s back, not gaining anything.
//!
//! # Windows still gets the policing hook, and that is not a contradiction
//!
//! This file used to connect **without declaring** anything, on the grounds
//! that there is "no capability to police". That was true when written and is
//! false on Windows, for a reason that inverts the macOS argument.
//!
//! On macOS the side door exists because OxiMux *holds* an Accessibility grant
//! and every child inherits it — the capability is delegated, so removing the
//! delegation removes the reach. On Windows there is no grant to hold or
//! inherit: `SendInput`, window messages, and UI Automation are available to
//! any unelevated process in the interactive session, always. The capability is
//! **ambient**. It does not arrive with the driver and it does not leave with
//! it, so an agent's shell here can drive the screen on a machine where screen
//! control was never installed, never enabled, and cannot be.
//!
//! `docs/windows-port-exclusions.md` states the rule as "the moment any
//! screen-driving capability lands on Windows, the policing hook has to land
//! with it". The capability was already there. So the hook is declared, on
//! every Claude chat, with **no driver and no tools** — which is the tier the
//! macOS module calls the wider one, and the only tier that applies here.
//!
//! What is *not* declared is any MCP server: there is no verified driver to
//! point at (Phase 2 of `plans/260801-0157-windows-computer-use/`), and
//! `oximux-computer-use` refuses to build one on Windows regardless.

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
    ///
    /// It does carry one thing: a label. The hook is told which chat it is
    /// enforcing for, and the gate derives a session id from that label — so a
    /// blank one would give every chat the same identity in a store they all
    /// share. Nothing here can grant anything, so no rows are written today;
    /// getting the identity right anyway costs a counter and stops the shared
    /// store from being wrong the moment something does write to it.
    #[derive(Debug)]
    pub(in super::super) struct ScreenControl {
        label: String,
    }

    /// Hands out chat ids, mirroring the macOS module. Monotonic and never
    /// reused within a process.
    static NEXT_CHAT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    impl Default for ScreenControl {
        fn default() -> Self {
            Self::new(Path::new("."))
        }
    }

    impl ScreenControl {
        pub(in super::super) fn new(_cwd: &Path) -> Self {
            let id = NEXT_CHAT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self {
                label: format!("chat-{id}"),
            }
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

        /// This chat's label, as handed to the gate. Test-visible so the
        /// uniqueness the shared grant store depends on can be asserted.
        #[cfg(test)]
        pub(in super::super) fn label(&self) -> &str {
            &self.label
        }

        pub(in super::super) fn begin_remote_turn(&self) {}
        pub(in super::super) fn end_remote_turn(&self) {}
        pub(in super::super) fn end_turn_activity(&self) {}
    }

    /// Spawn the agent, attaching the policing hook where one is warranted.
    ///
    /// The macOS version injects an MCP server *plus* the hook. Here only the
    /// hook is ever attached, and only on Windows — see the module docs for why
    /// the ambient/delegated distinction makes that the right split rather than
    /// an inconsistency.
    ///
    /// A missing gate binary degrades to connecting without it, loudly. That is
    /// the same choice the macOS path makes: refusing to start a chat because
    /// a helper is missing would be a worse trade than starting one, but it must
    /// not be silent, because the symptom is a chat that looks enforced and is
    /// not.
    pub(in super::super) fn connect_declaring(
        #[allow(unused_mut)] mut spec: ConnectSpec,
        chat: &ScreenControl,
        _cx: &App,
    ) -> anyhow::Result<(Arc<dyn AgentConnection>, Receiver<ThreadEvent>)> {
        #[cfg(windows)]
        declare_hook_only(&mut spec, chat);
        #[cfg(not(windows))]
        let _ = chat;

        oximux_agents::thread::connect(spec)
    }

    /// Attach the `PreToolUse` hook, with no server and no tool names.
    ///
    /// Deliberately *not* a call to the macOS module's `plan`: that one takes a
    /// driver resolver, and there is nothing here that could ever return one.
    /// Passing `None` through a function whose whole shape is "decide whether
    /// to include a driver" would read as a decision rather than an absence.
    #[cfg(windows)]
    fn declare_hook_only(spec: &mut ConnectSpec, chat: &ScreenControl) {
        use oximux_agents::thread::Transport;

        // Hooks are the Claude CLI's mechanism; the other transports have no
        // equivalent, so there is nowhere to put the policy. Same rule as the
        // macOS path, and the same reason.
        if spec.transport != Transport::StreamJson {
            return;
        }

        let (Some(gate), Ok(host)) = (gate_binary(), std::env::current_exe()) else {
            tracing::warn!(
                "screen-control gate binary not found beside the app; Windows chats \
                 will run without the Bash policing hook"
            );
            return;
        };

        let declaration = oximux_computer_use::mcp::declaration(
            // No driver, ever, on this path. `oximux-computer-use` also refuses
            // to build a server spec on Windows, so this is belt and braces.
            None,
            &oximux_computer_use::mcp::HookSpec {
                command: &gate,
                chat: &chat.label,
                grants: &grants_path(),
                host: &host,
                // No provenance: build-provenance only ever buys a *grant*
                // without asking, and nothing here can hold a grant. Claiming
                // it would hand the gate a fact it has no use for.
                worktree: None,
                started_at: None,
            },
        );

        debug_assert!(
            declaration.server.is_none() && declaration.disallowed_tools.is_empty(),
            "the absent path must never declare a driver"
        );
        spec.settings_json = Some(declaration.hook_settings);
    }

    /// The gate binary, which ships beside the app executable.
    ///
    /// `None` rather than a guess: a hook command pointing at a missing file is
    /// a hook that never refuses anything.
    #[cfg(windows)]
    fn gate_binary() -> Option<std::path::PathBuf> {
        let gate = std::env::current_exe()
            .ok()?
            .parent()?
            .join(oximux_computer_use::gate_binary_file_name());
        gate.is_file().then_some(gate)
    }

    /// Where the grant store lives, matching the real module exactly so the
    /// two processes cannot drift onto different files.
    ///
    /// Resolved through `app_paths` for that reason: spelling the directory
    /// out here is precisely how the two ended up on different files once
    /// already, since the hand-rolled `dirs::data_dir()` landed in the roaming
    /// profile on Windows while the rest of the app used the local one.
    #[cfg(windows)]
    fn grants_path() -> std::path::PathBuf {
        crate::app_paths::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(oximux_computer_use::grants::GRANTS_FILE_NAME)
    }

    /// Drop any grants a previous run left behind.
    ///
    /// Nothing on this platform can *write* a grant — there is no consent card
    /// to approve one — so this is expected to be a no-op against a store that
    /// does not exist. It runs anyway because "expected to be empty" and
    /// "guaranteed to be empty" are different claims, and the store is shared
    /// with a build that can write to it: a user moving a synced data directory
    /// between machines is exactly the case where the assumption breaks.
    ///
    /// `pub` to match the macOS module: the crate root re-exports this for the
    /// boot-time sweep.
    pub fn clear_stale_screen_control_grants() {
        #[cfg(windows)]
        if !oximux_computer_use::GrantTable::at(grants_path()).clear() {
            tracing::error!(
                path = ?grants_path(),
                "could not clear screen-control grants from the last run"
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chat_gets_its_own_label() {
        // The gate derives a session id from this label, and every chat on the
        // machine shares one grant store. Two chats answering to the same label
        // would be one identity in that store — which is harmless only for
        // exactly as long as nothing can write a grant.
        let a = computer_use::ScreenControl::new(Path::new("."));
        let b = computer_use::ScreenControl::new(Path::new("."));
        assert_ne!(a.label(), b.label());
        assert!(a.label().starts_with("chat-"), "{}", a.label());
    }

    /// The hook-only shape, asserted on the crate call this module makes.
    ///
    /// The wiring around it needs a gate binary on disk and a real
    /// `ConnectSpec`, so this pins the part that carries the meaning: a
    /// declaration built with no driver is a hook and nothing else.
    #[cfg(windows)]
    #[test]
    fn the_absent_path_declares_a_hook_and_never_a_driver() {
        let declaration = oximux_computer_use::mcp::declaration(
            None,
            &oximux_computer_use::mcp::HookSpec {
                command: Path::new(r"C:\Program Files\OxiMux\oximux-screen-gate.exe"),
                chat: "chat-1",
                grants: Path::new(r"C:\Users\u\AppData\Roaming\OxiMux\grants.json"),
                host: Path::new(r"C:\Program Files\OxiMux\oximux.exe"),
                worktree: None,
                started_at: None,
            },
        );

        assert!(declaration.server.is_none(), "no driver on this platform");
        assert!(declaration.disallowed_tools.is_empty());

        // And the hook is really there, aimed at the gate, matching Bash.
        let v: serde_json::Value =
            serde_json::from_str(&declaration.hook_settings).expect("valid json");
        let hook = &v["hooks"]["PreToolUse"][0];
        let command = hook["hooks"][0]["command"].as_str().expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(
            hook["matcher"].as_str().expect("matcher").contains("Bash"),
            "the shell is the whole point of the hook here"
        );
    }
}
