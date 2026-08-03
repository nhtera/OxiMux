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
//! # On Windows the reasoning above does not apply, and the door is wider
//!
//! Everything in that section is about a *leaking grant*. Windows has no such
//! grant to leak: there is no TCC, nothing to inherit, and no permission
//! attached to synthesizing input. Any unelevated process in the interactive
//! session can call `SendInput`, post window messages, or drive UI Automation
//! against any same-integrity window, always, with nothing asked and nothing
//! recorded.
//!
//! So screen control on Windows is **ambient rather than delegated**, and the
//! conclusion inverts: this brake is not less necessary there because there is
//! no grant, it is *more* necessary, because the reach it slows down was never
//! gated on anything in the first place. An agent's shell has it whether or not
//! the user ever opted in, whether or not the driver is installed, and whether
//! or not this crate exists.
//!
//! The one thing Windows *does* enforce is the integrity boundary — UIPI stops
//! input crossing into a higher-integrity process, in the kernel. That raises
//! the floor under elevated apps and says nothing about the ones that matter
//! here, which run at the same level as OxiMux. See [`crate::blocked`].
//!
//! ## What that costs in precision
//!
//! macOS has `osascript`: a binary whose whole purpose is automating other
//! apps, so naming it is already most of the signal. Windows has no equivalent.
//! PowerShell is the general-purpose shell *and* the way to P/Invoke
//! `user32.dll`, so the same program name covers `cargo test` and
//! `[User32]::SendInput(…)`.
//!
//! That makes the classifier's job strictly harder and its false-positive cost
//! strictly higher, because Windows agents reach for `pwsh -Command` routinely
//! where macOS agents run a bare binary. The negative tests below are therefore
//! the ones that matter most: a brake that refuses ordinary work is worse than
//! no brake, since it gets switched off.
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
//!
//! # `-EncodedCommand`, and why it is decoded rather than refused
//!
//! `powershell -EncodedCommand <base64>` hands the shell a UTF-16LE script as
//! one opaque blob. A string classifier cannot read it, and the obvious
//! responses are both bad: refusing every encoded command breaks a legitimate
//! and common way to get past `cmd`'s quoting rules, while ignoring it leaves a
//! trivially-reachable blind spot.
//!
//! So it is neither — the payload is decoded and classified as if it had been
//! written out, which is what it is. Only an *undecodable* payload falls back to
//! [`GuiScripting::OpaqueScript`], and the recursion is depth-limited so a
//! command encoding a command encoding a command terminates.
//!
//! This closes the specific hole. It does not make the module a boundary: the
//! same code still reaches the same APIs from a `.ps1` file, a compiled
//! executable, or a string assembled at runtime from pieces no substring
//! matches. Those are the same limits the macOS half has always had, restated.

/// Why a shell command counts as driving the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiScripting {
    /// AppleScript or JXA reaching the accessibility API — System Events,
    /// synthetic keystrokes, AX actions. Produced on macOS only.
    AppleEvents,
    /// Scripted UI automation through a scripting host rather than a native
    /// call: `SendKeys`, `AppActivate`, `WScript.Shell`,
    /// `System.Windows.Forms`. Produced on Windows only.
    ///
    /// The counterpart to [`Self::AppleEvents`] and a separate variant rather
    /// than a reuse of it, because the refusal text is read verbatim by the
    /// agent and there are no Apple events on Windows. Both variants exist on
    /// every platform so the type stays one shape and `match` arms stay
    /// exhaustive; only the producing code is `cfg`-ed.
    ScriptedInput,
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
            GuiScripting::ScriptedInput => {
                "drives the GUI by scripting keystrokes and window activation, which bypasses the \
                 screen-control consent you would otherwise be asked for. Use the screen-control \
                 tools instead — they name their target process and ask once per app"
            }
            #[cfg(not(windows))]
            GuiScripting::OpaqueScript => {
                "runs an AppleScript from a file or standard input, so its contents cannot be \
                 checked before it runs. Pass the script inline with -e, or use the \
                 screen-control tools, which name their target process"
            }
            #[cfg(windows)]
            GuiScripting::OpaqueScript => {
                "runs a script whose contents cannot be read before it runs, so what it would \
                 drive cannot be checked. Pass the script inline with -Command, or use the \
                 screen-control tools, which name their target process"
            }
        }
    }
}

/// Tools that exist to synthesize input. Matched on the program name alone —
/// there is no benign invocation to distinguish.
#[cfg(not(windows))]
const INPUT_SYNTHESIS_TOOLS: &[&str] = &["cliclick", "xdotool", "ydotool"];

/// The Windows set. `nircmd` and AutoHotkey are the two an agent reaches for
/// first, and neither has a use that is not driving the machine.
///
/// `xdotool` stays on the list: under WSLg it is installed inside the Linux
/// distribution and drives the Windows desktop through the Wayland/X bridge, so
/// a command naming it on Windows means exactly what it means elsewhere.
#[cfg(windows)]
const INPUT_SYNTHESIS_TOOLS: &[&str] = &[
    "autohotkey",
    "autohotkey64",
    "autohotkeyu64",
    "autohotkeyu32",
    "autoit3",
    "nircmd",
    "nircmdc",
    "xdotool",
    "ydotool",
];

/// Interpreters that can reach the native APIs with a line of inline code.
#[cfg(not(windows))]
const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "ruby", "perl", "node", "deno", "swift", "bun",
];

/// The Windows set: the same language runtimes, plus the shells and scripting
/// hosts that can P/Invoke `user32.dll` or reach `WScript.Shell` directly.
///
/// PowerShell being here *and* in [`WRAPPERS`] is deliberate and is why
/// [`programs_in`] reports the wrappers it steps over. It is genuinely both:
/// `pwsh -Command "nircmd …"` is a wrapper around another program, and
/// `pwsh -Command "[User32]::SendInput(…)"` is an interpreter running inline
/// code. Treating it as only one of the two loses half the cases.
#[cfg(windows)]
const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "ruby", "perl", "node", "deno", "bun", "powershell", "pwsh",
    "powershell_ise", "cscript", "wscript",
];

/// Scripting hosts dedicated to automation, whose script files are treated the
/// way `osascript`'s are: unreadable, so classified on what they could contain.
///
/// Only the Windows Script Host pair. PowerShell is deliberately absent —
/// `pwsh -File build.ps1` is ordinary work, and refusing it would be the
/// false-positive cliff this module cannot afford. That leaves the same hole
/// macOS leaves for `python3 automate.py`, knowingly.
#[cfg(windows)]
const SCRIPT_HOSTS: &[&str] = &["cscript", "wscript"];

/// Programs that run *another* program, so the interesting name is further
/// along the command line.
///
/// Shells are on the list because an agent's shell tool routinely sends
/// `["bash", "-lc", "…"]` rather than a bare command — reading only the first
/// token there would find `bash` every time and see nothing else ever again.
#[cfg(not(windows))]
const WRAPPERS: &[&str] = &[
    "env", "sudo", "nohup", "command", "exec", "time", "nice", "doas", "xargs", "stdbuf", "sh",
    "bash", "zsh", "dash", "ksh", "fish",
];

/// The Windows set. Keeps the POSIX shells — Git Bash and WSL are ordinary
/// here — and adds `cmd`, `start`, and the PowerShell pair.
#[cfg(windows)]
const WRAPPERS: &[&str] = &[
    "env", "sudo", "nohup", "command", "exec", "time", "nice", "xargs", "stdbuf", "sh", "bash",
    "zsh", "dash", "ksh", "fish", "cmd", "start", "start-process", "powershell", "pwsh",
    "powershell_ise", "wsl", "wt",
];

/// AppleScript that is reaching for the accessibility API rather than sending an
/// ordinary Apple event. `display notification` and `get selection` are not
/// automation of another app's UI; these are.
#[cfg(not(windows))]
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
#[cfg(not(windows))]
const NATIVE_API_MARKERS: &[&str] = &[
    "cgeventpost",
    "cgeventcreate",
    "axuielement",
    "cgeventtap",
    "cgpostkeyboardevent",
];

/// The Windows set: `user32.dll` input synthesis, UI Automation, and the two
/// Python automation libraries.
///
/// `sendmessage` and `postmessage` are deliberately **absent**. Both are
/// ordinary Win32 vocabulary that appears in any discussion of window handling,
/// and this repository now contains the words in several files — including this
/// one. Matching them would refuse an agent reading its own codebase through an
/// interpreter, which is the exact failure `searching_the_codebase_…` guards
/// against on the macOS side.
///
/// `pyautogui` and `pywinauto` are matched **bare**, unlike the Win32 call names
/// below, and the difference is which words have an innocent reading. `SendInput`
/// is a Win32 symbol that appears in this very file, so a bare mention has to
/// stay legal. `pyautogui` appears nowhere in this codebase and exists for one
/// purpose; naming it at all is the signal, exactly as for `wscript.shell`.
///
/// Found by live-testing the shipped binary: `from pywinauto import Application`
/// puts the name in neither call position, so the call-position rule read the
/// one line that states the intent as a mention.
///
/// The bare match is cheap because it is reached only from
/// [`classify_script_source`], which already requires an interpreter running
/// inline source. `pip install pyautogui` names no interpreter and stays clear —
/// see `installing_a_library_is_not_running_it`.
#[cfg(windows)]
const NATIVE_API_MARKERS: &[&str] = &[
    "user32.dll",
    "uiautomationclient",
    "windows.ui.automation",
    "pyautogui",
    "pywinauto",
];

/// Native input calls, matched only in **call position** — see [`names_call`].
///
/// These are the names an agent working on this repository will grep for, so a
/// bare mention has to stay legal: `rg -n SendInput crates\` through a
/// `pwsh -Command` wrapper is reading, not driving, and refusing it is how a
/// brake earns a reputation for being wrong and gets removed.
///
/// `[Win32]::SendInput(…)` and `$w.SendKeys(…)` are not bare mentions.
#[cfg(windows)]
const NATIVE_CALL_MARKERS: &[&str] =
    &["sendinput", "keybd_event", "mouse_event", "setcursorpos"];

/// Scripted-input calls, matched in call position for the same reason.
#[cfg(windows)]
const SCRIPTED_CALL_MARKERS: &[&str] = &["sendkeys", "sendwait", "appactivate"];

/// The scripting-host *types* whose only purpose here is driving the UI,
/// matched bare because naming them is already the signal.
///
/// The methods called on them — `SendKeys`, `AppActivate` — live in
/// [`SCRIPTED_CALL_MARKERS`] instead, because those words also appear in
/// documentation an agent might legitimately be reading.
#[cfg(windows)]
const SCRIPTED_INPUT_MARKERS: &[&str] = &["wscript.shell", "system.windows.forms"];

/// How is this shell command going to drive the GUI, if it is?
///
/// `None` for the overwhelming majority — callers must treat it as "this is an
/// ordinary shell command, leave it entirely alone".
pub fn classify_command(command: &str) -> Option<GuiScripting> {
    classify_inner(command, 0)
}

/// `depth` counts `-EncodedCommand` payloads already unwrapped; see
/// [`MAX_DECODE_DEPTH`].
fn classify_inner(command: &str, depth: u8) -> Option<GuiScripting> {
    let lowered = command.to_lowercase();
    let programs = programs_in(&lowered);

    if programs.iter().any(|p| INPUT_SYNTHESIS_TOOLS.contains(p)) {
        return Some(GuiScripting::InputSynthesis);
    }

    #[cfg(not(windows))]
    {
        let _ = depth;

        // Markers are matched against the whole command rather than the segment
        // the program was found in: a script body is one quoted argument and may
        // contain any of the characters segments are split on, so splitting it
        // up would lose the very text worth reading.
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

    #[cfg(windows)]
    {
        // Before anything else: an encoded payload *is* the command, so read it
        // rather than reasoning about the wrapper around it. Decoded from the
        // raw string because base64 is case-sensitive.
        if let Some(payload) = encoded_command_payload(command) {
            let Some(decoded) = decode_encoded_command(&payload) else {
                // The one case where there is genuinely nothing to read.
                return Some(GuiScripting::OpaqueScript);
            };
            // Unbounded recursion on attacker-supplied input is its own
            // problem; past the limit, judge it on what it could contain.
            if depth >= MAX_DECODE_DEPTH {
                return Some(GuiScripting::OpaqueScript);
            }
            // Read it as a command first — it may invoke `nircmd`, or nest
            // another encoded payload.
            if let Some(kind) = classify_inner(&decoded, depth + 1) {
                return Some(kind);
            }
            // Then as what it also is: bare script source. There is no
            // interpreter *name* inside a payload, because the interpreter is
            // the thing that was handed it, so the marker checks have to run
            // without one.
            return classify_script_source(&decoded.to_lowercase());
        }

        if !programs.iter().any(|p| INTERPRETERS.contains(p)) {
            // No interpreter in the command at all. This is the arm that keeps
            // `rg -n SendInput crates\` — an agent reading this very repository
            // — from being refused.
            return None;
        }

        if has_inline_source(&lowered) {
            return classify_script_source(&lowered);
        }

        // No inline source. Only the dedicated automation hosts are classified
        // on that alone — see `SCRIPT_HOSTS` for why PowerShell is not.
        if programs.iter().any(|p| SCRIPT_HOSTS.contains(p)) {
            return Some(GuiScripting::OpaqueScript);
        }

        None
    }
}

/// Classify a body of script source that is already known to be script source.
///
/// Scripted input is checked first: it is the more specific claim, and a
/// command doing both should be named by the mechanism a user would recognise.
#[cfg(windows)]
fn classify_script_source(lowered: &str) -> Option<GuiScripting> {
    let names = |markers: &[&str]| markers.iter().any(|m| lowered.contains(m));

    if names(SCRIPTED_INPUT_MARKERS) || names_call(lowered, SCRIPTED_CALL_MARKERS) {
        return Some(GuiScripting::ScriptedInput);
    }
    if names(NATIVE_API_MARKERS) || names_call(lowered, NATIVE_CALL_MARKERS) {
        return Some(GuiScripting::NativeApi);
    }
    None
}

/// Does `lowered` name one of `markers` in a position that reads as a *call*
/// rather than a mention?
///
/// The distinction is what separates `[Win32]::SendInput(…)` from
/// `rg -n SendInput crates\`. A call is preceded by a member or namespace
/// separator, or followed by an argument list or a further member access:
///
/// - `::sendinput(` — preceded by `:`
/// - `$w.sendkeys(` — preceded by `.`, followed by `(`
/// - `pyautogui.click(` — followed by `.`
/// - `-pattern sendinput` — neither, so not a call
///
/// Crude, and deliberately so: this is a brake, and the cost of the two
/// mistakes is not symmetric. Missing an unusual spelling costs one silent
/// pass on a module that never claimed to be a fence; refusing an agent's grep
/// costs the module its credibility.
#[cfg(windows)]
fn names_call(lowered: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(marker) {
            let start = from + offset;
            let end = start + marker.len();
            let before = lowered[..start].chars().next_back();
            let after = lowered[end..].chars().next();
            if matches!(after, Some('(' | '.')) || matches!(before, Some(':' | '.')) {
                return true;
            }
            from = end;
        }
        false
    })
}

/// How many nested `-EncodedCommand` payloads to unwrap before giving up.
///
/// Two is already one more than any honest command uses. The limit exists so a
/// command that encodes a command that encodes a command terminates in a
/// refusal rather than in a stack overflow.
#[cfg(windows)]
const MAX_DECODE_DEPTH: u8 = 2;

/// The base64 blob following `-EncodedCommand`, if the command has one.
///
/// PowerShell accepts any unambiguous prefix of a parameter name, so `-enc`,
/// `-encoded`, and `-EncodedCommand` are all the same switch and all appear in
/// the wild. Three characters is the shortest unambiguous prefix; matching
/// shorter would collide with `-ExecutionPolicy`.
#[cfg(windows)]
fn encoded_command_payload(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        let flag = token.trim_start_matches(['-', '/']).to_ascii_lowercase();
        if flag.len() >= 3 && "encodedcommand".starts_with(&flag) {
            let payload = tokens.next()?.trim_matches(['"', '\'']);
            return (!payload.is_empty()).then(|| payload.to_string());
        }
    }
    None
}

/// Decode one `-EncodedCommand` payload back to the script it stands for.
///
/// PowerShell specifies base64-encoded **UTF-16LE**. Both steps are allowed to
/// fail and both mean the same thing to the caller: this is not something we
/// can read, so judge it on what it could contain rather than guessing.
#[cfg(windows)]
fn decode_encoded_command(payload: &str) -> Option<String> {
    use base64::Engine as _;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Every program invoked across the command's segments.
///
/// Split on the shell's own separators so `ls && osascript …` is seen, then take
/// each segment's first real token: skipping `FOO=bar` assignments and stepping
/// through wrappers like `env` and `sudo`, which would otherwise be the answer.
///
/// The wrappers stepped over are **kept**, not discarded. On macOS that changes
/// nothing — no wrapper name appears in any table below — but on Windows
/// PowerShell is both a wrapper and an interpreter, and dropping it would hide
/// every `pwsh -Command "<inline code>"` from the marker checks.
fn programs_in(command: &str) -> Vec<&str> {
    command
        .split(['|', ';', '&', '\n', '(', ')', '`'])
        .flat_map(programs_of_segment)
        .collect()
}

/// The wrapper chain and the program at the end of it, in order.
fn programs_of_segment(segment: &str) -> Vec<&str> {
    let mut tokens = segment
        .split_whitespace()
        .map(|token| token.trim_matches(['"', '\'', '\\']))
        .filter(|token| !token.is_empty() && !token.contains('='));
    let mut found = Vec::new();
    let mut past_wrapper = false;
    loop {
        let Some(token) = tokens.next() else {
            return found;
        };
        // A wrapper's own flags are not the program it runs — `bash -lc foo`.
        // Only skipped after one, so an ordinary `ls -la` still reads as `ls`.
        if past_wrapper && (token.starts_with('-') || is_slash_flag(token)) {
            continue;
        }
        let name = basename(token);
        found.push(name);
        if !WRAPPERS.contains(&name) {
            return found;
        }
        past_wrapper = true;
    }
}

/// Is this a Windows-style switch — `cmd /c`, `start /min`, `cmd /v:on`?
///
/// Needed because [`basename`] splits on `/` and so reduces `/c` to `c`, which
/// then reads as the program the wrapper runs and ends the chain one token
/// early. That made `cmd /c powershell -c "…SendKeys…"` classify as nothing at
/// all: a one-token prefix that disabled this entire module. Live-tested, not
/// hypothesised — the unit tests missed it because every `cmd /c` case they
/// carried was benign, so the truncated chain and an honest verdict agreed.
///
/// Deliberately narrow, because on POSIX a leading `/` means an absolute path
/// and `sudo /usr/bin/foo` must keep naming `foo`. A switch has no further
/// separator and is short; `/usr/bin/foo` and Git Bash's `/c/Users/…` have
/// both, and fail the test on either count.
fn is_slash_flag(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty() && !rest.contains('/') && rest.len() <= 4
}

/// The last path component, under either separator.
///
/// Windows accepts both `\` and `/` in paths, and an agent writing
/// `C:\Windows\System32\cmd.exe` must not read as one long program name.
fn basename(token: &str) -> &str {
    let token = token.rsplit('/').next().unwrap_or(token);
    #[cfg(windows)]
    let token = {
        let name = token.rsplit('\\').next().unwrap_or(token);
        // `cmd.exe` and `cmd` are the same program to every table here.
        name.strip_suffix(".exe").unwrap_or(name)
    };
    token
}

/// Was the script passed on the command line, where it could be read?
fn has_inline_source(command: &str) -> bool {
    command.split_whitespace().any(|token| {
        #[cfg(windows)]
        {
            // PowerShell's own spellings, plus the POSIX ones that still arrive
            // through Git Bash and WSL. `-command` covers `-Command` because the
            // caller lowercases first, and any unambiguous prefix of it is the
            // same switch to PowerShell.
            let flag = token.trim_start_matches(['-', '/']);
            if !flag.is_empty() && flag.len() >= 3 && "command".starts_with(flag) {
                return true;
            }
        }
        matches!(token, "-e" | "-c" | "--eval" | "--command")
    })
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

    #[cfg(windows)]
    mod windows_brake {
        use super::*;

        /// Encode a script the way PowerShell does, so the tests exercise the
        /// real decoder rather than a fixture someone typed.
        fn encode(script: &str) -> String {
            use base64::Engine as _;
            let utf16: Vec<u8> = script
                .encode_utf16()
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            base64::engine::general_purpose::STANDARD.encode(utf16)
        }

        // ---- The negatives. These decide whether the brake is usable. ----

        #[test]
        fn ordinary_windows_work_is_untouched() {
            // Everything an agent does all day on Windows, including through
            // the `pwsh -Command` wrapper it reaches for constantly. A brake
            // that refuses these gets switched off, and then protects nothing.
            for command in [
                "cargo test --workspace",
                "cargo clippy --workspace --all-targets",
                r#"git commit -m "fix the thing""#,
                "pwsh -Command 'cargo test --workspace'",
                r#"powershell -Command "git status --short""#,
                r#"cmd /c "dir /b""#,
                "npm run build",
                r"C:\Windows\System32\cmd.exe /c echo hello",
                "python -m pytest",
                "wsl ls -la",
            ] {
                assert_clear(command);
            }
        }

        #[test]
        fn reading_this_repository_is_not_driving_the_gui() {
            // This crate now contains every one of these words. An agent
            // working on the file you are reading must be able to search for
            // them — including through an interpreter, which is the arm the
            // macOS side gets for free by not having one.
            for command in [
                r"rg -n SendInput crates\",
                r"findstr /s /n keybd_event crates\computer-use\src\*.rs",
                "grep -r pyautogui crates/",
                r"cat crates\computer-use\src\gui_scripting.rs",
                r#"pwsh -Command "Select-String -Path *.rs -Pattern SendInput""#,
            ] {
                assert_clear(command);
            }
        }

        #[test]
        fn a_mention_is_not_a_call() {
            // The heuristic the two above rest on, stated directly. Same
            // symbol, same interpreter, same inline source — only the syntax
            // around it differs, and that is the whole signal.
            assert_clear(r#"pwsh -Command "echo SendInput""#);
            assert_clear(r#"pwsh -Command "Get-Help about_SendKeys""#);
            assert_flagged(
                r#"pwsh -Command "[W]::SendInput(1,$i,$s)""#,
                GuiScripting::NativeApi,
            );
            assert_flagged(
                r#"pwsh -Command "$w.SendKeys('x')""#,
                GuiScripting::ScriptedInput,
            );
        }

        #[test]
        fn a_wrapper_switch_is_not_the_program_it_runs() {
            // Regression for a complete bypass of this module: `/c` reduced to
            // `c` through `basename`, which then read as the program `cmd` runs
            // and ended the wrapper chain before `powershell` was ever seen. One
            // token in front of any command below turned a deny into silence.
            //
            // Found by running the shipped gate binary, not by unit tests: every
            // `cmd /c` case above is benign, so a truncated chain and a correct
            // verdict were indistinguishable.
            let driving = r#"powershell -c "[System.Windows.Forms.SendKeys]::SendWait('x')""#;
            assert_flagged(driving, GuiScripting::ScriptedInput);
            for prefix in ["cmd /c ", "cmd.exe /c ", "cmd /k ", "start /min ", "cmd /v:on /c "] {
                assert_flagged(&format!("{prefix}{driving}"), GuiScripting::ScriptedInput);
            }
        }

        #[test]
        fn a_posix_absolute_path_is_still_a_program() {
            // The other half of the fix above. `/c` is a switch; `/usr/bin/env`
            // is not, and neither is Git Bash's `/c/…`. Both have a second
            // separator, which is what the test keys on.
            assert_eq!(
                programs_of_segment("sudo /usr/bin/python3 -c x"),
                vec!["sudo", "python3"]
            );
            assert_clear("bash /c/Users/me/build.sh");
        }

        #[test]
        fn naming_a_gui_automation_library_is_the_signal() {
            // `pyautogui` is not a word with an innocent reading the way
            // `SendInput` is — it appears nowhere in this codebase and does one
            // thing. The import line states the intent, and requiring call
            // position let the clearest statement of intent through.
            for command in [
                r#"python -c "from pywinauto import Application""#,
                r#"python -c "import pyautogui""#,
                r#"python3 -c "import pyautogui; pyautogui.click(10,10)""#,
                r#"python -c "from pywinauto.application import Application""#,
            ] {
                assert_flagged(command, GuiScripting::NativeApi);
            }
        }

        #[test]
        fn installing_a_library_is_not_running_it() {
            // The bare match above is confined to inline script source, so the
            // package-manager commands that merely name the library stay clear.
            // This is what makes the bare match affordable.
            for command in [
                "pip install pyautogui",
                "python -m pip install pywinauto",
                "grep -r pyautogui crates/",
            ] {
                assert_clear(command);
            }
        }

        #[test]
        fn a_powershell_script_file_is_not_classified() {
            // The same deliberate hole macOS leaves for `python3 automate.py`.
            // Refusing every `-File` would refuse most of what an agent does
            // and buy nothing, since the code is reachable a dozen other ways.
            assert_clear("pwsh -File build.ps1");
            assert_clear(r"powershell -ExecutionPolicy Bypass -File .\scripts\ci.ps1");
        }

        // ---- The positives. ----

        #[test]
        fn native_input_synthesis_through_powershell_is_caught() {
            // The canonical Windows bypass: P/Invoke `user32.dll` and post
            // input directly. No permission is involved, which is the whole
            // reason this module matters more here than on macOS.
            assert_flagged(
                r#"powershell -Command "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")]public static extern void mouse_event(int a,int b,int c,int d,int e);' -Name U -Namespace W; [W.U]::mouse_event(2,0,0,0,0)""#,
                GuiScripting::NativeApi,
            );
            assert_flagged(
                r#"pwsh -Command "[Win32]::SendInput(1, $inputs, $size)""#,
                GuiScripting::NativeApi,
            );
        }

        #[test]
        fn scripted_keystrokes_are_caught_and_named_as_such() {
            // The other half, and a different mechanism: no native call, just
            // a scripting host typing into whatever holds focus.
            assert_flagged(
                r#"powershell -Command "$w = New-Object -ComObject WScript.Shell; $w.AppActivate('Notepad'); $w.SendKeys('hello')""#,
                GuiScripting::ScriptedInput,
            );
            assert_flagged(
                r#"pwsh -Command "[System.Windows.Forms.SendKeys]::SendWait('{ENTER}')""#,
                GuiScripting::ScriptedInput,
            );
        }

        #[test]
        fn the_python_automation_libraries_are_caught() {
            assert_flagged(
                r#"python -c "import pyautogui; pyautogui.click(100, 200)""#,
                GuiScripting::NativeApi,
            );
            assert_flagged(
                r#"python3 -c "import pywinauto; pywinauto.Application().connect(title='x')""#,
                GuiScripting::NativeApi,
            );
        }

        #[test]
        fn dedicated_input_tools_are_caught_by_name_alone() {
            assert_flagged("nircmd sendkeypress ctrl+c", GuiScripting::InputSynthesis);
            assert_flagged(
                r"C:\tools\AutoHotkey64.exe script.ahk",
                GuiScripting::InputSynthesis,
            );
            // And through the wrapper, which is why `programs_in` keeps the
            // wrappers it steps over.
            assert_flagged(
                r#"pwsh -Command "nircmd movecursor 10 10""#,
                GuiScripting::InputSynthesis,
            );
        }

        #[test]
        fn a_windows_script_host_file_is_flagged_on_what_it_could_contain() {
            // `wscript foo.vbs` is the `osascript foo.scpt` case: a host whose
            // whole purpose is automation, running something unreadable.
            assert_flagged("wscript automate.vbs", GuiScripting::OpaqueScript);
            assert_flagged("cscript //nologo drive.vbs", GuiScripting::OpaqueScript);
        }

        // ---- `-EncodedCommand`, the hole the plan expected to concede. ----

        #[test]
        fn an_encoded_command_is_decoded_and_judged_on_its_contents() {
            // The blind spot, closed. Base64 defeats a substring classifier
            // only if the classifier declines to decode.
            let payload = encode("[Win32]::SendInput(1, $i, $s)");
            assert_flagged(
                &format!("powershell -EncodedCommand {payload}"),
                GuiScripting::NativeApi,
            );

            let keys = encode("$w = New-Object -ComObject WScript.Shell; $w.SendKeys('x')");
            assert_flagged(
                &format!("pwsh -enc {keys}"),
                GuiScripting::ScriptedInput,
            );
        }

        #[test]
        fn an_encoded_ordinary_command_stays_ordinary() {
            // The reason decoding beats refusing: encoding is a legitimate way
            // past `cmd`'s quoting rules, and most encoded commands are dull.
            let payload = encode("Get-ChildItem -Path . -Recurse | Measure-Object");
            assert_clear(&format!("powershell -EncodedCommand {payload}"));
        }

        #[test]
        fn every_spelling_of_the_switch_is_read() {
            // PowerShell takes any unambiguous prefix, and all of these appear
            // in the wild. Reading only the long form would leave the short
            // ones as the blind spot the long one no longer is.
            let payload = encode("[Win32]::SendInput(1, $i, $s)");
            for flag in ["-EncodedCommand", "-encodedcommand", "-enc", "-Enc", "/enc"] {
                assert_flagged(
                    &format!("powershell {flag} {payload}"),
                    GuiScripting::NativeApi,
                );
            }
        }

        #[test]
        fn an_undecodable_payload_is_refused_rather_than_ignored() {
            // Failing open here would make "send something that is not valid
            // base64" the new bypass.
            assert_flagged(
                "powershell -EncodedCommand not-valid-base64!!",
                GuiScripting::OpaqueScript,
            );
        }

        #[test]
        fn nested_encoding_terminates_instead_of_recursing_forever() {
            // Attacker-supplied input drives this recursion, so it is bounded.
            // Two layers still resolve; the point is that it stops.
            let inner = encode("[Win32]::SendInput(1, $i, $s)");
            let outer = encode(&format!("powershell -EncodedCommand {inner}"));
            let result = classify_command(&format!("powershell -EncodedCommand {outer}"));
            assert!(result.is_some(), "a doubly-encoded payload must not pass");
        }

        #[test]
        fn the_limit_this_cannot_close_is_stated_by_a_test() {
            // Not an aspiration — the honest boundary, pinned so nobody reads
            // the module as a fence. A script assembled at runtime from pieces
            // matches no substring, and a compiled binary matches nothing at
            // all. Both reach the same APIs.
            assert_clear(r#"pwsh -Command "& ([scriptblock]::Create($fromDisk))""#);
            assert_clear(r".\my-automation-tool.exe");
        }
    }

    /// The `osascript` half. macOS-only because every command in it names a
    /// binary that does not exist on Windows; the counterpart is
    /// [`windows_brake`].
    #[cfg(not(windows))]
    mod macos_brake {
    use super::*;

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
            GuiScripting::ScriptedInput,
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
