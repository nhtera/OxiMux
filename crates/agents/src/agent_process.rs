//! Recognise an agent CLI from a process in a terminal's process tree.
//!
//! The window title and the status sideband only describe an agent that
//! chooses to describe itself: the title is opt-in for several CLIs and absent
//! by default in others, and the sideband exists only where OxiMux installed
//! hooks. A running process is neither optional nor polite — it is there for
//! as long as the agent is, whether or not the agent is producing output.
//! That makes the process tree the one presence signal that covers every CLI
//! and survives an idle agent, which is what this module reads.
//!
//! Identity comes from the argument vector, not the executable name. Two
//! measurements decided that, and both are the difference between working and
//! not:
//!
//! * The kernel names a process for the binary it *resolved*, so a CLI invoked
//!   through a symlink is named for the link's target. Every common agent
//!   installs exactly that way — one of them points at a file named for its
//!   version number, which matches nothing. `argv[0]` keeps the path the user
//!   actually invoked.
//! * The executable name was also seen reporting an agent's name for a process
//!   that was plainly something else. A signal that can say "claude" about a
//!   `grep` is not one to hang a sidebar row on.
//!
//! The argument vector fixes both, and handles the second shape for free: a
//! CLI shipped as a script (`gemini`, `pi`) runs under its interpreter, where
//! only the arguments name it — `node /opt/homebrew/bin/gemini`. The
//! executable name is kept as a fallback for platforms that cannot read
//! arguments, where it is the only signal and the symlink convention does not
//! apply.

/// A CLI this cockpit can name on sight.
struct KnownAgent {
    /// Executable (or script) name, without extension, lowercase.
    bin: &'static str,
    /// Display label, drawn from [`crate::agent_title::AGENT_LABELS`] so a
    /// process-detected and a title-detected agent resolve to the same icon
    /// and adapter slug.
    label: &'static str,
}

const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent { bin: "claude", label: "Claude Code" },
    KnownAgent { bin: "codex", label: "Codex" },
    KnownAgent { bin: "gemini", label: "Gemini CLI" },
    KnownAgent { bin: "aider", label: "Aider" },
    KnownAgent { bin: "opencode", label: "OpenCode" },
    KnownAgent { bin: "copilot", label: "GitHub Copilot" },
    KnownAgent { bin: "grok", label: "Grok" },
    KnownAgent { bin: "cursor-agent", label: "Cursor" },
    KnownAgent { bin: "droid", label: "Droid" },
    KnownAgent { bin: "pi", label: "Pi" },
    KnownAgent { bin: "amp", label: "Amp" },
];

/// Runtimes that execute a script named in their arguments. Only a process
/// running one of these is worth reading an argument vector for; anything else
/// already carries its identity in its executable name.
///
/// Shells are deliberately absent: a shell wrapper either `exec`s the agent
/// (so the process takes the agent's name) or spawns it as a child (so the
/// tree walk finds it directly), and a shell's own arguments (`-zsh`) name
/// nothing.
const INTERPRETERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "ruby", "perl", "npx",
];

/// Script suffixes stripped before matching, so `cli.js` compares as `cli`.
const SCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "py", "rb", "pl", "exe"];

/// Identify the agent CLI a process is, or `None` when it is not one.
///
/// `argv` is the process's argument vector and is the signal that decides.
/// `name` is the executable name as the kernel records it, consulted only when
/// there is no argument vector to read.
pub fn agent_label_for_process(name: &str, argv: Option<&[String]>) -> Option<&'static str> {
    let Some(argv) = argv.filter(|a| !a.is_empty()) else {
        return match_token(name);
    };
    // Only each argument's own base name is compared, never the directories
    // above it, so working inside a directory that happens to share an agent's
    // name cannot conjure an agent.
    if let Some(label) = match_token(base_name(&argv[0])) {
        return Some(label);
    }
    // Beyond `argv[0]`, an argument only names the program when the program is
    // a runtime executing it — `node /opt/homebrew/bin/gemini`. Without that
    // gate, `vim codex.rs` would read as an agent.
    if !is_interpreter(base_name(&argv[0])) {
        return None;
    }
    argv[1..]
        .iter()
        .find_map(|arg| match_token(base_name(arg)))
}

/// True when `name` is a runtime that fronts a script.
fn is_interpreter(name: &str) -> bool {
    let stem = strip_extension(&name.to_ascii_lowercase());
    INTERPRETERS.contains(&stem.as_str())
}

/// The agent whose binary name is `token`, ignoring case and any script or
/// `.exe` extension.
fn match_token(token: &str) -> Option<&'static str> {
    let stem = strip_extension(&token.to_ascii_lowercase());
    KNOWN_AGENTS
        .iter()
        .find(|a| a.bin == stem)
        .map(|a| a.label)
}

/// Everything after the last path separator. Both separators are honoured so
/// a Windows argument vector reads the same as a POSIX one.
fn base_name(arg: &str) -> &str {
    arg.rsplit(['/', '\\']).next().unwrap_or(arg)
}

/// `name` without a trailing known script/executable extension. Only the
/// listed suffixes are stripped, so a binary named `foo.bar` keeps its name.
fn strip_extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if SCRIPT_EXTENSIONS.contains(&ext) && !stem.is_empty() => stem.to_owned(),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an argument vector from a shell-style string.
    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    fn label(name: &str, line: &str) -> Option<&'static str> {
        agent_label_for_process(name, Some(&argv(line)))
    }

    #[test]
    fn a_native_binary_is_named_by_the_path_it_was_invoked_through() {
        assert_eq!(label("claude", "/Users/me/.local/bin/claude"), Some("Claude Code"));
        assert_eq!(label("codex", "codex"), Some("Codex"));
        assert_eq!(label("opencode", "/Users/me/.opencode/bin/opencode"), Some("OpenCode"));
        assert_eq!(label("droid", "droid"), Some("Droid"));
        assert_eq!(label("amp", "/Users/me/.local/bin/amp"), Some("Amp"));
    }

    /// The measured shape that motivates preferring the argument vector: a CLI
    /// installed as a symlink is named by the kernel for the link's *target*,
    /// which for Claude Code is a bare version number. Matching the executable
    /// name would miss the most common agent entirely.
    #[test]
    fn a_symlinked_launcher_is_named_by_its_link_not_its_target() {
        assert_eq!(
            agent_label_for_process("2.1.231", Some(&argv("/Users/me/.local/bin/claude"))),
            Some("Claude Code")
        );
    }

    /// The other measured shape: the executable name was seen reporting an
    /// agent for a process whose arguments said it was a search tool. The
    /// argument vector must overrule it rather than be a tiebreak.
    #[test]
    fn an_executable_name_never_overrules_the_arguments() {
        assert_eq!(
            agent_label_for_process("claude", Some(&argv("ugrep -G --hidden -iE pattern"))),
            None,
            "a process whose arguments are a search tool is not an agent"
        );
    }

    #[test]
    fn a_script_cli_is_named_by_its_interpreters_arguments() {
        // The exact shapes measured from a running `gemini` and `pi`.
        assert_eq!(label("node", "node /opt/homebrew/bin/gemini --help"), Some("Gemini CLI"));
        assert_eq!(
            label("node", "node /Users/x/.nvm/versions/node/v23.4.0/bin/pi --help"),
            Some("Pi")
        );
        // Gemini re-launches itself with heap flags before the script path.
        assert_eq!(
            label("node", "node --max-old-space-size=24576 /opt/homebrew/bin/gemini"),
            Some("Gemini CLI")
        );
    }

    #[test]
    fn a_script_entry_point_keeps_its_package_name() {
        assert_eq!(
            label("node", "node /usr/local/lib/node_modules/aider/cli.js"),
            None,
            "an entry point named cli.js names no agent — only the launcher does"
        );
        assert_eq!(label("node", "node /opt/bin/codex.js"), Some("Codex"));
    }

    #[test]
    fn a_directory_named_after_an_agent_is_not_an_agent() {
        // The false positive a naive substring match would produce: working
        // inside ~/Code/codex must not conjure a Codex agent.
        assert_eq!(label("node", "node /Users/me/Code/codex/scripts/build.js"), None);
        assert_eq!(label("node", "node /home/claude/server.js"), None);
    }

    #[test]
    fn an_argument_only_names_the_program_when_a_runtime_is_running_it() {
        // Opening an agent's source file is not running the agent.
        assert_eq!(label("vim", "vim /Users/me/bin/codex"), None);
        assert_eq!(label("cat", "cat /usr/local/bin/claude"), None);
    }

    #[test]
    fn an_ordinary_program_is_not_an_agent() {
        assert_eq!(label("zsh", "-zsh"), None);
        assert_eq!(label("cargo", "cargo build --release"), None);
        // An interpreter running something unrelated stays unrecognised.
        assert_eq!(label("node", "node ./server.js"), None);
    }

    #[test]
    fn the_executable_name_is_used_only_when_there_is_no_argument_vector() {
        // Windows, where arguments cannot be read but the image name is sound.
        assert_eq!(agent_label_for_process("claude.exe", None), Some("Claude Code"));
        assert_eq!(agent_label_for_process("codex", None), Some("Codex"));
        assert_eq!(agent_label_for_process("zsh", None), None);
        // An empty vector is no evidence, not evidence of nothing.
        assert_eq!(agent_label_for_process("codex", Some(&[])), Some("Codex"));
    }

    #[test]
    fn a_windows_path_separator_is_honoured() {
        assert_eq!(
            label("node.exe", r"node.exe C:\Users\me\AppData\npm\gemini"),
            Some("Gemini CLI")
        );
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(label("Codex", "Codex"), Some("Codex"));
    }

    /// Every label this module can return must be one the title path can also
    /// produce, so a process-detected agent renders with the same icon and
    /// adapter slug as a title-detected one instead of falling through to a
    /// generic row.
    #[test]
    fn every_label_is_shared_with_the_title_vocabulary() {
        for agent in KNOWN_AGENTS {
            assert!(
                crate::agent_title::AGENT_LABELS
                    .iter()
                    .any(|(_, label)| *label == agent.label),
                "`{}` reports label {:?}, which the title vocabulary cannot produce",
                agent.bin,
                agent.label
            );
        }
    }
}
