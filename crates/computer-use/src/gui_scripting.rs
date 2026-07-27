//! Shell commands that can drive the GUI without going near the driver.
//!
//! # Why a shell tool needs a screen-control policy at all
//!
//! macOS attributes an Accessibility grant to the *responsible* process — the
//! GUI app at the head of the chain — and every descendant inherits it. That was
//! measured on this project rather than assumed: a binary spawned from an
//! agent's shell tool reports `AXIsProcessTrusted() == true`, and `osascript`
//! talking to System Events works from it, *through* an intervening helper
//! process whose entire job is to disclaim responsibility.
//!
//! The grant doing that is not the driver's. `CuaDriver` carries its own TCC
//! identity under launchd, so nothing leaks out of it. The grant that leaks is
//! the one OxiMux takes for the **Escape kill switch** — an event tap needs
//! Accessibility. So the safety feature is what opens this door, it opens for
//! every project regardless of which ones opted in, and no wrapper process is
//! going to shut it.
//!
//! # What this is, and is not
//!
//! It is a brake on the obvious reach. An agent that wants to click something
//! and types `osascript -e 'tell application "System Events" to keystroke …'`
//! gets told to use the screen-control tools, which are governed by the grant
//! table, name their target, and raise a consent card.
//!
//! It is **not** a security boundary, and nothing here should be described as
//! one. The same APIs are three lines of `python3 -c`, or a Swift file compiled
//! on the spot, or a script written to disk and run later. A string classifier
//! cannot see any of that. What it can do is stop the accident and make the
//! deliberate version an obviously deliberate act.
//!
//! Because it is a brake and not a boundary, it errs toward catching things: a
//! command that merely *mentions* System Events alongside an `osascript` call is
//! classified. A false refusal costs the agent one turn and says exactly what to
//! do instead; a false pass is silent.

/// Why a shell command counts as driving the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiScripting {
    /// AppleScript or JXA reaching the accessibility API — System Events,
    /// synthetic keystrokes, AX actions.
    AppleEvents,
    /// A command-line tool whose whole purpose is synthesizing input.
    InputSynthesis,
    /// An interpreter invoked with inline code naming the event-synthesis or
    /// accessibility APIs directly.
    NativeApi,
    /// An AppleScript this cannot read, because it lives in a file or arrives on
    /// stdin. Classified on the strength of what it *could* contain.
    OpaqueScript,
}

impl GuiScripting {
    /// The refusal the agent reads. Each says what was seen and what to use
    /// instead — a bare "denied by policy" would leave an agent retrying the
    /// same command with different quoting.
    pub fn reason(self) -> &'static str {
        match self {
            GuiScripting::AppleEvents => {
                "drives the GUI through AppleScript and the accessibility API, which bypasses the \
                 screen-control consent you would otherwise be asked for. Use the screen-control \
                 tools instead — they name their target process and ask once per app"
            }
            GuiScripting::InputSynthesis => {
                "is a tool for synthesizing mouse and keyboard input, which bypasses the \
                 screen-control consent you would otherwise be asked for. Use the screen-control \
                 tools instead — they name their target process and ask once per app"
            }
            GuiScripting::NativeApi => {
                "calls the event-synthesis or accessibility APIs directly, which bypasses the \
                 screen-control consent you would otherwise be asked for. Use the screen-control \
                 tools instead — they name their target process and ask once per app"
            }
            GuiScripting::OpaqueScript => {
                "runs an AppleScript from a file or standard input, so its contents cannot be \
                 checked before it runs. Pass the script inline with -e, or use the \
                 screen-control tools, which name their target process"
            }
        }
    }
}

/// Tools that exist to synthesize input. Matched on the program name alone —
/// there is no benign invocation to distinguish.
const INPUT_SYNTHESIS_TOOLS: &[&str] = &["cliclick", "xdotool", "ydotool"];

/// Interpreters that can reach the native APIs with a line of inline code.
const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "ruby", "perl", "node", "deno", "swift", "bun",
];

/// Programs that run *another* program, so the interesting name is further
/// along the command line.
///
/// Shells are on the list because an agent's shell tool routinely sends
/// `["bash", "-lc", "…"]` rather than a bare command — reading only the first
/// token there would find `bash` every time and see nothing else ever again.
const WRAPPERS: &[&str] = &[
    "env", "sudo", "nohup", "command", "exec", "time", "nice", "doas", "xargs", "stdbuf", "sh",
    "bash", "zsh", "dash", "ksh", "fish",
];

/// AppleScript that is reaching for the accessibility API rather than sending an
/// ordinary Apple event. `display notification` and `get selection` are not
/// automation of another app's UI; these are.
const AX_APPLESCRIPT_MARKERS: &[&str] = &[
    "system events",
    "keystroke",
    "key code",
    "perform action",
    "axpress",
    "click at",
    "ui element",
];

/// Symbols that only appear when code is synthesizing events or walking the
/// accessibility tree itself.
const NATIVE_API_MARKERS: &[&str] = &[
    "cgeventpost",
    "cgeventcreate",
    "axuielement",
    "cgeventtap",
    "cgpostkeyboardevent",
];

/// How is this shell command going to drive the GUI, if it is?
///
/// `None` for the overwhelming majority — callers must treat it as "this is an
/// ordinary shell command, leave it entirely alone".
pub fn classify_command(command: &str) -> Option<GuiScripting> {
    let lowered = command.to_lowercase();
    let programs = programs_in(&lowered);

    if programs.iter().any(|p| INPUT_SYNTHESIS_TOOLS.contains(p)) {
        return Some(GuiScripting::InputSynthesis);
    }

    // Markers are matched against the whole command rather than the segment the
    // program was found in: a script body is one quoted argument and may contain
    // any of the characters segments are split on, so splitting it up would lose
    // the very text worth reading.
    let names_ax = |markers: &[&str]| markers.iter().any(|m| lowered.contains(m));

    if programs.iter().any(|p| matches!(*p, "osascript" | "osacompile")) {
        if names_ax(AX_APPLESCRIPT_MARKERS) {
            return Some(GuiScripting::AppleEvents);
        }
        // No inline script means the body is in a file or on stdin — including
        // the `echo … | osascript` form, which is how an inline script gets
        // written when someone would rather it not be read.
        if !has_inline_source(&lowered) {
            return Some(GuiScripting::OpaqueScript);
        }
        // An inline script that was read and mentions nothing AX-related:
        // `display notification`, `get the name of the front window`, and the
        // rest of ordinary AppleScript.
        return None;
    }

    if programs.iter().any(|p| INTERPRETERS.contains(p))
        && has_inline_source(&lowered)
        && names_ax(NATIVE_API_MARKERS)
    {
        return Some(GuiScripting::NativeApi);
    }

    None
}

/// Every program invoked across the command's segments.
///
/// Split on the shell's own separators so `ls && osascript …` is seen, then take
/// each segment's first real token: skipping `FOO=bar` assignments and stepping
/// through wrappers like `env` and `sudo`, which would otherwise be the answer.
fn programs_in(command: &str) -> Vec<&str> {
    command
        .split(['|', ';', '&', '\n', '(', ')', '`'])
        .filter_map(program_of)
        .collect()
}

fn program_of(segment: &str) -> Option<&str> {
    let mut tokens = segment
        .split_whitespace()
        .map(|token| token.trim_matches(['"', '\'', '\\']))
        .filter(|token| !token.is_empty() && !token.contains('='));
    let mut past_wrapper = false;
    loop {
        let token = tokens.next()?;
        // A wrapper's own flags are not the program it runs — `bash -lc foo`.
        // Only skipped after one, so an ordinary `ls -la` still reads as `ls`.
        if past_wrapper && token.starts_with('-') {
            continue;
        }
        let name = basename(token);
        if !WRAPPERS.contains(&name) {
            return Some(name);
        }
        past_wrapper = true;
    }
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Was the script passed on the command line, where it could be read?
fn has_inline_source(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|token| matches!(token, "-e" | "-c" | "--eval" | "--command"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn assert_flagged(command: &str, expected: GuiScripting) {
        assert_eq!(classify_command(command), Some(expected), "{command}");
    }

    #[track_caller]
    fn assert_clear(command: &str) {
        assert_eq!(classify_command(command), None, "{command}");
    }

    #[test]
    fn the_canonical_bypass_is_caught() {
        // The command the finding is about, and the one an agent reaches for
        // first because it is the documented way to automate macOS.
        assert_flagged(
            r#"osascript -e 'tell application "System Events" to keystroke "rm -rf /"'"#,
            GuiScripting::AppleEvents,
        );
    }

    #[test]
    fn applescript_that_is_not_automating_a_ui_is_left_alone() {
        // Refusing all of AppleScript would be refusing a general-purpose
        // scripting language because some of it is dangerous.
        assert_clear(r#"osascript -e 'display notification "build finished"'"#);
        assert_clear(r#"osascript -e 'tell application "Finder" to get selection'"#);
    }

    #[test]
    fn a_script_that_cannot_be_read_is_flagged_on_what_it_could_contain() {
        assert_flagged("osascript /tmp/whatever.scpt", GuiScripting::OpaqueScript);
        assert_flagged("osascript", GuiScripting::OpaqueScript);
    }

    #[test]
    fn piping_a_script_in_does_not_hide_it() {
        // The obvious way around "read the -e argument": don't use one. The
        // pipeline is split, so the `osascript` end is seen on its own with no
        // inline source.
        assert_flagged(
            r#"echo 'tell application "System Events" to keystroke "x"' | osascript"#,
            GuiScripting::AppleEvents,
        );
        assert_flagged("cat script.scpt | osascript", GuiScripting::OpaqueScript);
    }

    #[test]
    fn a_second_command_in_a_chain_is_still_seen() {
        // Only the first program would be found without splitting, and hiding
        // behind a harmless first command is free.
        assert_flagged(
            r#"cd /tmp && osascript -e 'tell app "System Events" to key code 36'"#,
            GuiScripting::AppleEvents,
        );
        assert_flagged("make build; cliclick c:100,200", GuiScripting::InputSynthesis);
    }

    #[test]
    fn wrappers_do_not_launder_the_program_name() {
        assert_flagged("env FOO=bar cliclick c:10,10", GuiScripting::InputSynthesis);
        assert_flagged("sudo cliclick c:10,10", GuiScripting::InputSynthesis);
        assert_flagged("/usr/local/bin/cliclick c:10,10", GuiScripting::InputSynthesis);
    }

    #[test]
    fn a_command_nested_behind_a_shell_is_still_read() {
        // The everyday shape, not an evasion: agents send `["bash","-lc","…"]`.
        // Reading only the first token would find `bash` forever.
        assert_flagged("bash -lc cliclick c:10,10", GuiScripting::InputSynthesis);
        assert_flagged(
            r#"sh -c "osascript -e 'tell app \"System Events\" to keystroke \"x\"'""#,
            GuiScripting::AppleEvents,
        );
        // And an ordinary command through the same wrapper stays ordinary.
        assert_clear("bash -lc 'cargo test --workspace'");
    }

    #[test]
    fn inline_native_api_calls_are_caught() {
        assert_flagged(
            r#"python3 -c "import Quartz; Quartz.CGEventPost(0, e)""#,
            GuiScripting::NativeApi,
        );
        assert_flagged(
            r#"swift -e 'let e = AXUIElementCreateApplication(pid)'"#,
            GuiScripting::NativeApi,
        );
    }

    #[test]
    fn searching_the_codebase_for_those_symbols_is_not_driving_the_gui() {
        // This repo *contains* an event tap, so an agent working on it will grep
        // for exactly these names. Requiring both an interpreter and inline
        // source is what keeps that from being a refusal.
        assert_clear("rg -n CGEventPost crates/");
        assert_clear("grep -r AXUIElement apps/desktop/src");
        assert_clear("cat apps/desktop/src/platform/escape_tap.rs");
    }

    #[test]
    fn ordinary_commands_are_untouched() {
        // The most important negative by far. Everything an agent does all day
        // must pass through without an opinion.
        for command in [
            "cargo test --workspace",
            "git commit -m 'fix the thing'",
            "ls -la",
            "npm run build && npm test",
            "echo 'keystroke' > notes.txt",
            "python3 script.py",
            "node -e 'console.log(1 + 1)'",
        ] {
            assert_clear(command);
        }
    }

    #[test]
    fn an_interpreter_running_a_file_is_not_classified() {
        // Deliberate, and the clearest statement of the limit: the file could
        // contain anything. Flagging every `python3 foo.py` would refuse most of
        // what an agent does to buy nothing, since the same code can be reached
        // a dozen other ways.
        assert_clear("python3 automate.py");
        assert_clear("./my-compiled-tool");
    }

    #[test]
    fn every_variant_says_what_to_do_instead() {
        // The refusal is the only thing standing between a classified command
        // and an agent retrying it with different quoting.
        for kind in [
            GuiScripting::AppleEvents,
            GuiScripting::InputSynthesis,
            GuiScripting::NativeApi,
            GuiScripting::OpaqueScript,
        ] {
            let reason = kind.reason();
            assert!(
                reason.contains("screen-control tools"),
                "{kind:?} must point somewhere: {reason}"
            );
        }
    }
}
