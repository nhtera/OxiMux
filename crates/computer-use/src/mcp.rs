//! The MCP server OxiMux declares for an agent that may drive the screen.
//!
//! This is where the injection seam built previously meets the driver: the
//! returned [`McpServerSpec`] is what gets handed to a spawned agent, so
//! `claude` launches `cua-driver mcp` itself and talks to it directly.
//!
//! That out-of-process hop is why OxiMux's Rust is not in the tool-dispatch
//! path, and why the enforcement point is the permission round-trip rather
//! than anything in this crate.

use std::path::Path;

use oximux_agent_core::thread::McpServerSpec;

// The server name and the tool-name predicates derived from it live in
// `oximux-agent-core`, because the transcript scrubber has to match on them
// even where this crate does not build. Re-exported here so callers keep
// reaching for them at the screen-control crate, which is where they read.
pub use oximux_agent_core::screen_tools::{
    bare_tool_name, is_computer_use_tool, tool_prefix, SERVER_NAME,
};

// Read only by `server_spec`, and only in its non-Windows arm: the advisory
// host label describes a macOS permission grant that has no Windows analogue.
// The constant itself stays defined for every platform — it is also the
// never-drive identity, which is meaningful regardless.
#[cfg(not(windows))]
use crate::HOST_BUNDLE_ID;

/// Build the server declaration for a verified driver.
///
/// Two flags are deliberately *not* passed:
///
/// - `--socket`: the daemon is a machine-wide singleton on a fixed path shared
///   with every other MCP client. Overriding it would fragment that into a
///   second daemon needing its own TCC grants.
/// - `--claude-code-computer-use-compat`: that swaps the standard tool surface
///   for a screenshot-shaped one matching an agent's *native* computer-use
///   tools. Agent Chat cannot use those anyway, and the standard surface is the
///   one with the accessibility-tree paths that work on background windows.
///
/// # Not available on Windows yet
///
/// Gated behind the `windows-screen-control` feature, because this is the
/// function that constitutes a screen-driving capability: its return value is
/// handed to a spawned agent.
///
/// The safety pair `docs/windows-port-exclusions.md` demanded — the
/// `gui_scripting` brake and the Escape kill switch — has landed. What is still
/// missing is the *trust* gate: the published Windows driver binaries are
/// unsigned, so [`crate::verify`] has nothing to check and this would declare
/// an unverified third-party binary to an agent. See the crate docs.
///
/// A direct caller on Windows gets a "cannot find function" error rather than a
/// working driver declaration. See [`declaration`] for the same rule applied to
/// the path callers are actually meant to use.
#[cfg(any(not(windows), feature = "windows-screen-control"))]
pub fn server_spec(driver: &Path) -> McpServerSpec {
    // `--host-bundle-id` is an advisory label the driver echoes back in
    // `check_permissions` output, and `permissions` is documented "(macOS)" —
    // there is no grant on Windows for it to describe. Confirmed against the
    // driver's own manifest, which recommends a bare `["mcp"]` there:
    //
    //   cua-driver manifest -p  ->  "mcp_invocation": { "args": ["mcp"] }
    //
    // `HOST_BUNDLE_ID` itself stays meaningful on every platform — it is also
    // the identity an agent may never drive (see [`crate::blocked`]).
    #[cfg(windows)]
    let args = vec!["mcp".to_string()];
    #[cfg(not(windows))]
    let args = vec![
        "mcp".to_string(),
        "--host-bundle-id".to_string(),
        HOST_BUNDLE_ID.to_string(),
    ];

    McpServerSpec::new(SERVER_NAME, driver.display().to_string()).args(args)
}

/// Everything a chat must be given in order to have screen control.
///
/// The parts travel together deliberately, and in one direction: handing an
/// agent the server without the hook is the failure this type exists to
/// prevent, because the runtime policy would then run only in the permission
/// modes that actually prompt, and measurement showed two ordinary situations
/// where none does.
///
/// The reverse is not a failure but the ordinary case — see [`server`].
///
/// [`server`]: Declaration::server
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The driver, when this chat is allowed to reach it.
    ///
    /// `None` for every chat the user has not opted in, which is nearly all of
    /// them — and those chats are still given the hook. The reason is that the
    /// thing being gated is not really the tools: the macOS Accessibility grant
    /// behind them belongs to the OxiMux *process*, every child inherits it, and
    /// an agent's shell is a child. So a chat with no screen-control tools can
    /// still drive the screen through `osascript`, and the gate is what refuses
    /// that. Registering it only where the tools are declared would put the
    /// check exactly where it is least needed.
    pub server: Option<McpServerSpec>,
    /// Namespaced tool names for `--disallowedTools`. The CLI removes these
    /// from the agent's surface entirely, in every permission mode, and a deny
    /// outranks a user's own `permissions.allow` rule.
    ///
    /// Empty when there is no server: these names would then match nothing, and
    /// a deny list full of tools that do not exist reads as protection it is
    /// not providing.
    pub disallowed_tools: Vec<String>,
    /// Inline JSON for `--settings`, registering the `PreToolUse` hook that
    /// decides everything the deny list cannot express — anything depending on
    /// a call's *arguments*, or on which chat holds which target.
    pub hook_settings: String,
}

/// Where the enforcing hook lives and what it needs told.
pub struct HookSpec<'a> {
    /// The gate binary, shipped alongside the app.
    pub command: &'a Path,
    /// This chat's screen-control session id.
    pub chat: &'a str,
    /// The shared grant store. Passed explicitly so the app and the hook cannot
    /// resolve different files.
    pub grants: &'a Path,
    /// OxiMux's own executable — normally `std::env::current_exe()`.
    ///
    /// Passed for the same reason as `grants`, and it matters more than it
    /// looks: the gate is a *separate binary*, so it cannot ask what process it
    /// is and get a useful answer. Without this, "an agent may never drive
    /// OxiMux" holds only for a shipped build, which is identifiable by bundle
    /// id — a development build is ad-hoc signed with none, and that is the
    /// build this feature is written in.
    pub host: &'a Path,
    /// The chat's worktree and start time, for build provenance. `None` leaves
    /// the hook asking about every target rather than trusting its own builds.
    pub worktree: Option<&'a Path>,
    pub started_at: Option<std::time::SystemTime>,
}

/// Build the full declaration for a chat.
///
/// The single way to obtain the server spec, so no call site can take one part
/// and leave the rest. `driver` is `None` for a chat that gets no
/// screen-control tools — it still gets the hook, for the reason on
/// [`Declaration::server`].
pub fn declaration(driver: Option<&Path>, hook: &HookSpec<'_>) -> Declaration {
    // Windows may not reach a screen-control tool while the driver it would
    // reach is unverifiable, so every chat is treated as a chat with no driver:
    // no server, and no deny list naming tools that are not there.
    //
    // The hook below is still built and still installed — that is the whole
    // point, and the reason this degrades rather than refusing to compile. A
    // Windows build gets the policing hook on every chat before it gets any
    // screen control at all, which is the safe order and the one
    // `docs/windows-port-exclusions.md` asks for.
    #[cfg(all(windows, not(feature = "windows-screen-control")))]
    let driver: Option<&Path> = {
        let _ = driver;
        None
    };

    let mut command = format!(
        "{} --chat {} --grants {} --host-exe {}",
        shell_quote(&hook.command.display().to_string()),
        shell_quote(hook.chat),
        shell_quote(&hook.grants.display().to_string()),
        shell_quote(&hook.host.display().to_string()),
    );
    if let Some(worktree) = hook.worktree {
        command.push_str(&format!(" --worktree {}", shell_quote(&worktree.display().to_string())));
    }
    if let Some(started) = hook.started_at {
        let secs = started
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        command.push_str(&format!(" --since {secs}"));
    }

    let hook_settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": hook_matcher(),
                "hooks": [{ "type": "command", "command": command }],
            }]
        }
    })
    .to_string();

    Declaration {
        #[cfg(any(not(windows), feature = "windows-screen-control"))]
        server: driver.map(server_spec),
        // `driver` is unconditionally `None` here, but `server_spec` does not
        // exist to name at all, so the field is built rather than mapped.
        #[cfg(all(windows, not(feature = "windows-screen-control")))]
        server: None,
        disallowed_tools: driver
            .map(|_| {
                crate::tools::forbidden_names()
                    .map(|tool| format!("{}{tool}", tool_prefix()))
                    .collect()
            })
            .unwrap_or_default(),
        hook_settings,
    }
}

/// Which tool names the gate is consulted about.
///
/// Two families, and the second is the one an earlier draft missed: this
/// server's own tools, and the **shell**. A command that drives the GUI is a
/// screen-control call by another road — it reaches the same APIs with the same
/// inherited grant — and [`crate::policy`] already refuses those. Scoped to the
/// MCP prefix, that refusal would never have run, because the gate would never
/// have been asked about a `Bash` call.
///
/// Built from [`crate::policy::SHELL_TOOLS`] so the two cannot drift: a shell
/// tool added there is matched here without anyone remembering to.
///
/// Anchored, and the trailing `.*` is deliberate rather than redundant. The
/// matcher is a regex whose anchoring the CLI does not document, and this shape
/// is correct under either reading — a bare `^…prefix` would match nothing at
/// all if the CLI wraps the pattern in a full-string match.
pub fn hook_matcher() -> String {
    let mut alternatives: Vec<String> = crate::policy::SHELL_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect();
    alternatives.push(format!("{}.*", tool_prefix()));
    format!("^({})$", alternatives.join("|"))
}

/// Single-quote a value for the shell the CLI runs hook commands through.
///
/// Paths here come from the user's filesystem — a worktree with a space or an
/// apostrophe would otherwise split into extra arguments and the gate would
/// reject its own command line, silently leaving the chat unenforced.
#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Quote a value for a Windows command line.
///
/// # Why the POSIX form is not merely suboptimal here but broken
///
/// Single quotes mean nothing to `cmd.exe`. Handed
/// `'C:\Program Files\OxiMux\oximux-screen-gate.exe'` it would look for a
/// program named `'C:\Program` and fail — and the paths this quotes are
/// `%LOCALAPPDATA%`- and `C:\Program Files`-shaped, so the case with a space in
/// it is the *normal* one rather than the edge case it is on macOS.
///
/// A hook command that cannot run is the worst failure mode this crate has: the
/// chat proceeds, the gate never executes, and nothing reports that the policy
/// stopped being enforced.
///
/// # The rules being implemented
///
/// `CommandLineToArgvW`, which is what the gate's own argv parsing goes
/// through:
///
/// - a run of backslashes followed by `"` — each backslash escapes the next, so
///   they must be doubled and the quote escaped;
/// - a run of backslashes at the end of the value — doubled, or they would
///   escape the closing quote and swallow it. `C:\repo\` is an ordinary path
///   and would otherwise produce `"C:\repo\"`;
/// - backslashes anywhere else — literal, and left alone.
///
/// # What this does not fix
///
/// `cmd.exe` expands `%NAME%` *inside* double quotes, so a path containing a
/// percent-delimited run that happens to match a defined environment variable
/// is still rewritten before the gate sees it. There is no escape for that on a
/// `cmd /c` command line — only a batch file has `%%` — so it is recorded
/// rather than handled. Percent signs are legal but rare in Windows paths, and
/// both delimiters must be present and the name must resolve.
#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');

    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                quoted.push(ch);
            }
            '"' => {
                // Every pending backslash is about to precede a quote, so each
                // one needs its own escape, and then the quote needs one.
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push('\\');
                quoted.push('"');
            }
            _ => {
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    // Trailing run: double it so the closing quote stays a closing quote.
    for _ in 0..backslashes {
        quoted.push('\\');
    }

    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    use oximux_agent_core::thread::to_claude_mcp_config;

    /// Tests of what a chat *with* a driver is handed.
    ///
    /// Gated off on Windows for the same reason the code is: there is no
    /// `server_spec` to call, and a declaration there never carries a server.
    /// The Windows counterpart is
    /// `the_windows_gate_withholds_the_driver_but_not_the_hook`, which asserts
    /// exactly that.
    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn spec_invokes_the_driver_in_mcp_mode() {
        let spec = server_spec(Path::new("/Applications/CuaDriver.app/Contents/MacOS/cua-driver"));
        assert_eq!(spec.name, SERVER_NAME);
        assert_eq!(
            spec.command,
            "/Applications/CuaDriver.app/Contents/MacOS/cua-driver"
        );
        assert_eq!(spec.args[0], "mcp");
    }

    /// The argv difference Phase 0 measured against the driver's own manifest.
    ///
    /// Windows gets a bare `["mcp"]`; macOS additionally carries the advisory
    /// host label. Asserted per-platform in one test rather than two so the
    /// contrast is visible — the risk here is not getting one wrong, it is
    /// changing one and forgetting the other exists.
    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn the_host_bundle_label_is_macos_only() {
        let spec = server_spec(Path::new("/bin/cua-driver"));
        assert_eq!(spec.args[0], "mcp");

        #[cfg(windows)]
        assert_eq!(
            spec.args,
            vec!["mcp".to_string()],
            "`permissions` is macOS-only, so there is no grant for the label to \
             describe — the driver's own manifest recommends a bare `mcp`"
        );

        #[cfg(not(windows))]
        {
            assert!(spec.args.iter().any(|a| a == "--host-bundle-id"));
            assert!(spec.args.iter().any(|a| a == HOST_BUNDLE_ID));
        }
    }

    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn spec_omits_socket_and_compat_flags() {
        // Both would change which daemon or which tool surface the agent gets.
        let spec = server_spec(Path::new("/bin/cua-driver"));
        assert!(!spec.args.iter().any(|a| a == "--socket"));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a == "--claude-code-computer-use-compat")
        );
    }

    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn spec_renders_into_the_agent_config_payload() {
        // End-to-end through the injection seam: what an agent is handed.
        let spec = server_spec(Path::new("/bin/cua-driver"));
        let raw = to_claude_mcp_config(std::slice::from_ref(&spec)).expect("some config");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let entry = &v["mcpServers"][SERVER_NAME];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "/bin/cua-driver");
        assert_eq!(entry["args"][0], "mcp");
    }

    fn declared() -> Declaration {
        declaration(
            Some(Path::new("/bin/cua-driver")),
            &HookSpec {
                command: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate"),
                chat: "chat-7",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"),
                worktree: Some(Path::new("/repo")),
                started_at: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            },
        )
    }

    #[test]
    fn the_declaration_registers_the_enforcing_hook() {
        // The part the deny list cannot do: argument-level and per-chat
        // decisions, in every permission mode.
        let v: serde_json::Value =
            serde_json::from_str(&declared().hook_settings).expect("valid json");
        let entry = &v["hooks"]["PreToolUse"][0];
        assert_eq!(entry["matcher"], hook_matcher());
        let command = entry["hooks"][0]["command"].as_str().expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(command.contains(&format!("--chat {}", shell_quote("chat-7"))), "{command}");
        assert!(
            command.contains(&format!("--grants {}", shell_quote("/data/grants.json"))),
            "{command}"
        );
        assert!(command.contains("--since 1700000000"), "{command}");
        // Without this the gate cannot tell that a call is aimed at OxiMux,
        // because it is a different binary and `current_exe()` names itself.
        assert!(
            command.contains(&format!(
                "--host-exe {}",
                shell_quote("/Applications/OxiMux.app/Contents/MacOS/oximux")
            )),
            "{command}"
        );
    }

    /// Windows quoting, which is a different algorithm rather than a different
    /// quote character — see [`shell_quote`].
    #[cfg(windows)]
    #[test]
    fn hook_paths_survive_windows_quoting() {
        let declared = declaration(
            None,
            &HookSpec {
                command: Path::new(r"C:\Program Files\OxiMux\oximux-screen-gate.exe"),
                chat: "chat-1",
                grants: Path::new(r"C:\Users\u\AppData\Roaming\OxiMux\grants.json"),
                host: Path::new(r"C:\Program Files\OxiMux\oximux.exe"),
                // A worktree at a drive-relative root, so the value ends in a
                // backslash — the case that silently eats the closing quote.
                worktree: Some(Path::new(r"C:\repo\")),
                started_at: None,
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");

        // Double quotes, because `cmd` does not know what a single quote is.
        assert!(
            command.contains(r#""C:\Program Files\OxiMux\oximux-screen-gate.exe""#),
            "{command}"
        );
        assert!(!command.contains('\''), "no POSIX quoting: {command}");

        // The trailing backslash is doubled, so the quote still closes.
        assert!(command.contains(r#""C:\repo\\""#), "{command}");

        // And the whole thing still splits back into the arguments the gate
        // expects — the property all of the above is in service of.
        let args = split_windows_command_line(command);
        assert!(
            args.contains(&r"C:\repo\".to_string()),
            "worktree did not round-trip: {args:?}"
        );
        assert!(
            args.contains(&r"C:\Program Files\OxiMux\oximux.exe".to_string()),
            "host exe did not round-trip: {args:?}"
        );
    }

    /// A minimal `CommandLineToArgvW`, so the quoting test asserts a
    /// round-trip rather than a spelling.
    ///
    /// Written out rather than calling the real API because the point is to
    /// check `shell_quote` against the documented rules, and a test that used
    /// the same Windows call the quoting was derived from could agree with it
    /// while both were wrong.
    #[cfg(windows)]
    fn split_windows_command_line(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut backslashes = 0usize;
        let mut started = false;

        let flush_backslashes = |current: &mut String, backslashes: &mut usize| {
            for _ in 0..*backslashes {
                current.push('\\');
            }
            *backslashes = 0;
        };

        for ch in line.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                    started = true;
                }
                '"' => {
                    // Pairs of backslashes are literal; an odd one escapes.
                    for _ in 0..backslashes / 2 {
                        current.push('\\');
                    }
                    if backslashes % 2 == 1 {
                        current.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    backslashes = 0;
                    started = true;
                }
                ' ' if !in_quotes => {
                    flush_backslashes(&mut current, &mut backslashes);
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                _ => {
                    flush_backslashes(&mut current, &mut backslashes);
                    current.push(ch);
                    started = true;
                }
            }
        }
        flush_backslashes(&mut current, &mut backslashes);
        if started {
            args.push(current);
        }
        args
    }

    #[cfg(not(windows))]
    #[test]
    fn hook_paths_survive_a_space_or_an_apostrophe() {
        // A worktree under "~/My Projects" would otherwise split into extra
        // arguments, the gate would reject its own command line, and the chat
        // would run unenforced with nothing to show for it.
        let declared = declaration(
            Some(Path::new("/bin/cua-driver")),
            &HookSpec {
                command: Path::new("/Apps/Oxi Mux.app/gate"),
                chat: "chat-1",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Apps/Oxi Mux.app/oximux"),
                worktree: Some(Path::new("/Users/x/it's mine")),
                started_at: None,
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains(r"'/Apps/Oxi Mux.app/gate'"), "{command}");
        // The POSIX way to get a literal quote inside single quotes: close,
        // emit an escaped quote, reopen.
        assert!(command.contains(r"'/Users/x/it'\''s mine'"), "{command}");
    }

    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn the_declaration_denies_the_tools_the_policy_forbids() {
        let declared = declared();
        // Namespaced, because that is the form the CLI matches on — a bare
        // `replay_trajectory` would silently match nothing.
        assert!(
            declared
                .disallowed_tools
                .contains(&"mcp__oximux-computer-use__replay_trajectory".to_string()),
            "{:?}",
            declared.disallowed_tools
        );
        assert!(declared.disallowed_tools.iter().all(|t| t.starts_with("mcp__oximux-computer-use__")));
        // And every denied name must round-trip back to a forbidden class.
        for name in &declared.disallowed_tools {
            let bare = bare_tool_name(name).expect("namespaced");
            assert!(matches!(
                crate::tools::classify(bare),
                crate::tools::ToolClass::Forbidden(_)
            ));
        }
    }

    #[cfg(any(not(windows), feature = "windows-screen-control"))]
    #[test]
    fn the_declaration_leaves_the_working_tools_alone() {
        // Denying these would break the feature rather than protect it.
        let declared = declared();
        for tool in ["click", "type_text", "get_window_state", "move_cursor"] {
            assert!(
                !declared
                    .disallowed_tools
                    .contains(&format!("mcp__oximux-computer-use__{tool}")),
                "{tool} must stay available"
            );
        }
    }

    /// The case nearly every chat is in: no screen-control tools, and the gate
    /// registered anyway.
    ///
    /// Not a degenerate configuration — it is what protects the road around the
    /// tools. OxiMux holds Accessibility process-wide so an agent's shell
    /// inherits it, which means a chat with no server can still drive the screen
    /// and the gate is the only thing that says no.
    #[test]
    fn a_chat_with_no_driver_still_gets_the_gate() {
        let declared = declaration(
            None,
            &HookSpec {
                command: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate"),
                chat: "chat-7",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"),
                worktree: None,
                started_at: None,
            },
        );
        assert!(declared.server.is_none(), "no tools without an opt-in");
        // Names that would match nothing: the tools do not exist here.
        assert!(declared.disallowed_tools.is_empty());

        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(
            command.contains(&format!("--chat {}", shell_quote("chat-7"))),
            "{command}"
        );
    }

    /// The Windows exclusion, asserted rather than assumed.
    ///
    /// `docs/windows-port-exclusions.md` requires the policing hook to ship in
    /// the same change as any screen-driving capability, and observes that
    /// "nothing in the compiler would say so". This is the runtime half of
    /// making it say so: a caller that passes a perfectly good driver path —
    /// the mistake this is guarding against — gets no server and no tool names
    /// back, and still gets the hook.
    ///
    /// Deleting this test is as much a decision as deleting the `cfg`s, which
    /// is the point. It fails the moment Windows can declare the driver, so
    /// Phase 5 has to come back here deliberately.
    #[cfg(all(windows, not(feature = "windows-screen-control")))]
    #[test]
    fn the_windows_gate_withholds_the_driver_but_not_the_hook() {
        let declared = declaration(
            Some(Path::new(r"C:\Users\u\AppData\Local\Programs\Cua\bin\cua-driver.exe")),
            &HookSpec {
                command: Path::new(r"C:\Program Files\OxiMux\oximux-screen-gate.exe"),
                chat: "chat-7",
                grants: Path::new(r"C:\Users\u\AppData\Roaming\OxiMux\grants.json"),
                host: Path::new(r"C:\Program Files\OxiMux\oximux.exe"),
                worktree: None,
                started_at: None,
            },
        );

        assert!(
            declared.server.is_none(),
            "Windows must not be handed a screen-control server while the \
             driver it points at cannot be verified"
        );
        assert!(
            declared.disallowed_tools.is_empty(),
            "a deny list without a server names tools that do not exist"
        );

        // The half that must still be there: every chat gets the hook.
        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(
            command.contains(&format!("--chat {}", shell_quote("chat-7"))),
            "{command}"
        );
    }

    #[test]
    fn the_matcher_is_the_exact_pattern_measured_against_the_cli() {
        // Pinned, not because the text is precious but because the CLI's matcher
        // semantics are undocumented and this specific string is the one a live
        // run confirmed fires for a `Bash` call. Changing it means re-running
        // `probes/matcher.py`, not just re-reading this file.
        assert_eq!(
            hook_matcher(),
            r"^(Bash|bash|shell|local_shell|run_terminal_cmd|mcp__oximux-computer-use__.*)$"
        );
    }

    #[test]
    fn the_matcher_covers_the_shell_as_well_as_the_tools() {
        // The half that is easy to lose: the policy refuses `osascript` inside a
        // Bash call, and that refusal only ever runs if the gate is asked about
        // Bash in the first place.
        let matcher = regex::Regex::new(&hook_matcher()).expect("a valid regex");
        for shell in crate::policy::SHELL_TOOLS {
            assert!(matcher.is_match(shell), "{shell} must reach the gate");
        }
        for tool in ["click", "type_text", "get_window_state"] {
            let name = format!("mcp__oximux-computer-use__{tool}");
            assert!(matcher.is_match(&name), "{name} must reach the gate");
        }
    }

    #[test]
    fn the_matcher_leaves_every_other_tool_alone() {
        // Each spawns a process per call, so a matcher that over-reaches is a
        // tax on every turn — and puts this in the path of tools it does not own.
        let matcher = regex::Regex::new(&hook_matcher()).expect("a valid regex");
        for tool in [
            "Read",
            "Edit",
            "WebFetch",
            "mcp__github__create_issue",
            // Near misses in both families.
            "BashOutput",
            "mcp__oximux-computer-use-extra__click",
        ] {
            assert!(!matcher.is_match(tool), "{tool} must not reach the gate");
        }
    }

    #[test]
    fn recognises_its_own_namespaced_tools() {
        assert!(is_computer_use_tool("mcp__oximux-computer-use__click"));
        assert!(is_computer_use_tool("mcp__oximux-computer-use__type_text"));
    }

    #[test]
    fn does_not_claim_other_servers_tools() {
        // A policy check keying off this must not capture unrelated tools, nor
        // miss one because a different server merely resembles the name.
        assert!(!is_computer_use_tool("Bash"));
        assert!(!is_computer_use_tool("mcp__other__click"));
        assert!(!is_computer_use_tool("mcp__oximux-computer-use-extra__click"));
        assert!(!is_computer_use_tool("computer-use__click"));
    }

    #[test]
    fn extracts_the_bare_tool_name() {
        assert_eq!(
            bare_tool_name("mcp__oximux-computer-use__type_text"),
            Some("type_text")
        );
        assert_eq!(bare_tool_name("mcp__other__click"), None);
        assert_eq!(bare_tool_name("Bash"), None);
        assert_eq!(bare_tool_name("mcp__oximux-computer-use__"), None);
    }

    #[test]
    fn prefix_matches_the_declared_server_name() {
        assert_eq!(tool_prefix(), format!("mcp__{SERVER_NAME}__"));
    }
}
