//! Classifies a tool call as "runs a command that boots, drives, or builds
//! for the iOS Simulator", so the desktop can open the Simulator panel the
//! moment an agent starts that work (P9 auto-open), with no command needed.
//!
//! Detection is deliberately conservative — a false positive opens a panel
//! the user did not want. It looks at each shell *statement*'s leading
//! command word, not a substring of the whole command line: `echo simctl` and
//! `echo "xcrun simctl boot"` both lead with `echo`, so neither counts. And it
//! only counts verbs that do something to a simulator (`simctl boot`, not
//! `simctl list`; `xcodebuild` aimed at a simulator, not `xcodebuild -list`).
//!
//! Agents spell "run a shell command" differently, and this has to see through
//! each (the shapes come from the adapters in `crates/agents/src/thread/`):
//!
//! - Claude, Codex (`commandExecution`), Pi and omp: a tool named `Bash`/`bash`
//!   with `{"command": "…"}`. Codex's string is the launcher line
//!   (`/bin/zsh -lc '…'`), unwrapped here.
//! - ACP agents: a free-text *title* as the name and the agent's raw input,
//!   with the tool's kind (`execute`) in a follow-up event — the desktop keeps
//!   the input until that kind arrives and asks [`is_simulator_input`].
//! - Others: `shell`, `exec_command`, `execute`, `terminal`,
//!   `run_shell_command`, `run_terminal_cmd`, with a string or an argv array.

use serde_json::Value;

/// Tool names that run a shell command, compared case-insensitively.
const SHELL_TOOL_NAMES: &[&str] =
    &["bash", "shell", "exec_command", "execute", "terminal", "run_shell_command", "run_terminal_cmd"];

/// `simctl` verbs that act on a device. Reads (`list`, `help`, `getenv`) do
/// not open anything.
const SIMCTL_VERBS: &[&str] = &["boot", "install", "launch", "openurl", "io"];

/// Whether `tool_name` is one of the shell-running tools above.
pub fn is_shell_tool(tool_name: &str) -> bool {
    SHELL_TOOL_NAMES.iter().any(|n| n.eq_ignore_ascii_case(tool_name))
}

/// True when `tool_name(input)` is a shell tool running a simulator command.
pub fn is_simulator_command(tool_name: &str, input: &Value) -> bool {
    is_shell_tool(tool_name) && is_simulator_input(input)
}

/// True when a shell tool's `input` runs a simulator command: `xcrun simctl
/// boot|install|launch|openurl|io`, `xcodebuild` aimed at a simulator
/// destination or SDK, `open -a Simulator`, `oximux sim …`, `idb` (not its
/// listings), or an Expo / React Native / Flutter run on an iOS simulator.
/// For a caller that already knows the tool runs commands (an ACP `execute`).
pub fn is_simulator_input(input: &Value) -> bool {
    let Some(command) = extract_command(input) else { return false };
    statements(&command).iter().any(|stmt| statement_matches(stmt))
}

/// One command-line string out of the tool input:
/// - `{"command": "xcrun simctl boot …"}`
/// - `{"cmd": ["xcodebuild", "-scheme", …]}` (an argv array, space-joined)
/// - `{"command": ["bash", "-lc", "xcrun simctl boot …"]}` or the string
///   `/bin/zsh -lc '…'` (a shell launcher: the real command is its script)
fn extract_command(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let raw = obj.get("command").or_else(|| obj.get("cmd"))?;
    match raw {
        Value::String(s) => Some(unwrap_launcher_line(s)),
        Value::Array(items) => {
            let strs: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if strs.is_empty() {
                return None;
            }
            if strs.len() >= 3 && is_shell_launcher(strs[0]) && is_script_flag(strs[1]) {
                return Some(strs[strs.len() - 1].to_owned());
            }
            Some(strs.join(" "))
        }
        _ => None,
    }
}

/// `/bin/zsh -lc 'xcrun simctl boot X'` → `xcrun simctl boot X`; anything
/// else is returned as is.
fn unwrap_launcher_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let mut parts = trimmed.splitn(3, char::is_whitespace);
    let (Some(program), Some(flag), Some(script)) = (parts.next(), parts.next(), parts.next()) else {
        return line.to_owned();
    };
    if !is_shell_launcher(program) || !is_script_flag(flag) {
        return line.to_owned();
    }
    let script = script.trim();
    let quoted = script.len() >= 2
        && ((script.starts_with('\'') && script.ends_with('\'')) || (script.starts_with('"') && script.ends_with('"')));
    if quoted { script[1..script.len() - 1].to_owned() } else { script.to_owned() }
}

fn is_shell_launcher(program: &str) -> bool {
    matches!(basename(program), "bash" | "sh" | "zsh")
}

/// `-c`, `-lc`, `-ic`: the flags that make the next argument a script.
fn is_script_flag(flag: &str) -> bool {
    flag.starts_with('-') && !flag.starts_with("--") && flag.ends_with('c')
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Splits a command line into shell statements on `&&`, `||`, `;`, `|` and
/// newlines outside quotes (so `git commit -m "a; xcrun simctl boot"` stays one
/// `git` statement). A rough split — it does not unwind `$(...)` or
/// here-docs — adequate for classification, not for execution.
fn statements(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let split = match (quote, c) {
            (Some(q), _) => {
                if c == q {
                    quote = None;
                }
                false
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                false
            }
            (None, ';' | '\n') => true,
            (None, '|') => {
                chars.next_if_eq(&'|');
                true
            }
            (None, '&') => chars.next_if_eq(&'&').is_some(),
            _ => false,
        };
        if split {
            out.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    out.push(current);
    out.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

/// Words that run the command after them.
const PREFIXES: &[&str] = &["env", "sudo", "time", "exec", "command", "nice", "nohup"];

/// The statement's words after any `VAR=value` assignments and prefixes
/// (`env`, `sudo`, `time`…), each without surrounding quotes.
fn command_words(stmt: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = stmt.split_whitespace().map(|t| t.trim_matches(['"', '\''])).collect();
    let skip = tokens.iter().take_while(|t| is_assignment(t) || PREFIXES.contains(t)).count();
    tokens.drain(..skip);
    tokens
}

fn is_assignment(token: &str) -> bool {
    token.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn statement_matches(stmt: &str) -> bool {
    let tokens = command_words(stmt);
    let Some(&head) = tokens.first() else { return false };
    let arg = |i: usize| tokens.get(i).copied();
    match basename(head) {
        "xcrun" => arg(1) == Some("simctl") && arg(2).is_some_and(|verb| SIMCTL_VERBS.contains(&verb)),
        "xcodebuild" => {
            let lower = stmt.to_lowercase();
            // `-destination`'s value is a quoted, comma-separated string
            // (`platform=iOS Simulator,name=…`) that does not tokenize on
            // whitespace, so the whole statement is searched.
            lower.contains("-sdk iphonesimulator") || (lower.contains("-destination") && lower.contains("simulator"))
        }
        "open" => tokens.windows(2).any(|w| w[0] == "-a" && w[1].to_lowercase().starts_with("simulator")),
        "oximux" => arg(1) == Some("sim"),
        "idb" => arg(1).is_some_and(|verb| !verb.starts_with("list") && !verb.starts_with('-')),
        // A package runner: its flags (`-y`) and verbs (`exec`, `dlx`) first.
        "npx" | "bunx" | "pnpx" | "yarn" | "pnpm" => {
            let skip = tokens[1..].iter().take_while(|t| t.starts_with('-') || matches!(**t, "exec" | "dlx")).count();
            expo_or_rn_ios(&tokens[1 + skip..])
        }
        "expo" | "react-native" => expo_or_rn_ios(&tokens),
        "flutter" => arg(1) == Some("run") && flutter_targets_ios_sim(&tokens),
        _ => false,
    }
}

/// `expo run:ios`, `expo start --ios` (or `-i`), `react-native run-ios`.
fn expo_or_rn_ios(tokens: &[&str]) -> bool {
    match tokens {
        ["expo", "run:ios", ..] | ["react-native", "run-ios", ..] => true,
        ["expo", "start", rest @ ..] => rest.iter().any(|t| *t == "--ios" || *t == "-i"),
        _ => false,
    }
}

/// `flutter run -d <device>` counts only when `<device>` is an iOS simulator:
/// a simulator udid, `iPhone …`/`iPad …`, `ios`, or a name containing
/// "simulator". `flutter run -d macos` and a bare `flutter run` do not.
fn flutter_targets_ios_sim(tokens: &[&str]) -> bool {
    tokens.iter().position(|&t| t == "-d" || t == "--device-id").and_then(|i| tokens.get(i + 1)).is_some_and(|d| {
        let d = d.trim_matches(['"', '\'']);
        let lower = d.to_lowercase();
        is_udid(d) || lower.contains("iphone") || lower.contains("ipad") || lower == "ios" || lower.contains("simulator")
    })
}

/// A simulator udid: 8-4-4-4-12 hex digits.
fn is_udid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `(tool_name, input, expected, why)`, in the shapes the adapters send.
    fn cases() -> Vec<(&'static str, Value, bool, &'static str)> {
        vec![
            // Claude: `Bash{command}`.
            ("Bash", json!({"command": "xcrun simctl boot 1234"}), true, "simctl boot"),
            ("Bash", json!({"command": "xcrun simctl install booted build/App.app"}), true, "simctl install"),
            ("Bash", json!({"command": "xcrun simctl launch booted com.example.app"}), true, "simctl launch"),
            ("Bash", json!({"command": "xcrun simctl openurl booted https://example.com"}), true, "simctl openurl"),
            ("Bash", json!({"command": "xcrun simctl io booted screenshot /tmp/s.png"}), true, "simctl io"),
            ("Bash", json!({"command": "cd ios && xcodebuild -scheme App -destination 'platform=iOS Simulator,name=iPhone 17' build"}), true, "xcodebuild at a simulator, after a cd"),
            ("Bash", json!({"command": "xcodebuild -scheme App -sdk iphonesimulator build"}), true, "simulator SDK"),
            ("Bash", json!({"command": "npx expo run:ios"}), true, "expo run:ios"),
            ("Bash", json!({"command": "npx expo start --ios"}), true, "expo start --ios"),
            ("Bash", json!({"command": "npx react-native run-ios --simulator=\"iPhone 17\""}), true, "RN run-ios through npx"),
            ("Bash", json!({"command": "react-native run-ios"}), true, "RN run-ios"),
            ("Bash", json!({"command": "flutter run -d \"iPhone 17 Pro\""}), true, "flutter on an iPhone"),
            ("Bash", json!({"command": "flutter run -d 81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3"}), true, "flutter on a simulator udid"),
            ("Bash", json!({"command": "oximux sim screenshot"}), true, "our own verbs"),
            ("Bash", json!({"command": "open -a Simulator"}), true, "Simulator.app"),
            ("Bash", json!({"command": "idb ui tap 100 200"}), true, "idb driving a device"),
            ("Bash", json!({"command": "DEVELOPER_DIR=/Applications/Xcode.app xcrun simctl boot X"}), true, "env prefix"),
            ("Bash", json!({"command": "xcodebuild build 2>&1 | tail\nxcrun simctl boot X"}), true, "second line"),
            // Codex: `Bash{command}` with the launcher line.
            ("Bash", json!({"command": "/bin/zsh -lc 'xcrun simctl boot X'", "cwd": "/w"}), true, "codex launcher line"),
            ("Bash", json!({"command": "/bin/bash -c \"npx expo run:ios\""}), true, "double-quoted script"),
            // Pi / omp: lower-case `bash{command}`.
            ("bash", json!({"command": "xcrun simctl launch booted com.x"}), true, "pi"),
            // Other shells, argv arrays.
            ("shell", json!({"command": ["bash", "-lc", "xcrun simctl boot X"]}), true, "argv launcher"),
            ("exec_command", json!({"cmd": ["xcodebuild", "-sdk", "iphonesimulator"]}), true, "argv joined"),
            ("execute", json!({"command": "oximux sim tap --label Continue"}), true, "execute"),
            ("terminal", json!({"command": "xcrun simctl boot X"}), true, "terminal"),
            ("Bash", json!({"command": "sudo xcrun simctl boot X"}), true, "sudo prefix"),
            ("Bash", json!({"command": "time xcodebuild -sdk iphonesimulator build"}), true, "time prefix"),
            ("Bash", json!({"command": "npx -y expo run:ios"}), true, "npx flag"),
            ("Bash", json!({"command": "yarn expo run:ios"}), true, "yarn"),
            ("Bash", json!({"command": "pnpm exec react-native run-ios"}), true, "pnpm exec"),
            ("Bash", json!({"command": "open -a \"Simulator\""}), true, "quoted app name"),
            ("Bash", json!({"command": "xcodebuild build | xcpretty && xcrun simctl launch booted com.x"}), true, "after a pipe"),
            // Negatives.
            ("Bash", json!({"command": "xcrun simctl list devices -j"}), false, "a listing"),
            ("Bash", json!({"command": "xcrun simctl help"}), false, "help"),
            ("Bash", json!({"command": "xcodebuild -list"}), false, "listing schemes"),
            ("Bash", json!({"command": "xcodebuild -scheme App -sdk macosx build"}), false, "macOS SDK"),
            ("Bash", json!({"command": "swift build"}), false, "swift build"),
            ("Bash", json!({"command": "echo simctl"}), false, "printing the word"),
            ("Bash", json!({"command": "echo \"xcrun simctl boot\""}), false, "quoted text"),
            ("Bash", json!({"command": "grep -r 'xcrun simctl boot' docs"}), false, "searching for it"),
            ("Bash", json!({"command": "git commit -m \"fix; xcrun simctl boot X\""}), false, "a separator inside quotes"),
            ("Bash", json!({"command": "echo 'a && open -a Simulator'"}), false, "&& inside quotes"),
            ("Bash", json!({"command": "yarn install"}), false, "yarn, unrelated"),
            ("Bash", json!({"command": "xcrun xctrace record"}), false, "xcrun, not simctl"),
            ("Bash", json!({"command": "open -a Xcode"}), false, "open another app"),
            ("Bash", json!({"command": "oximux status"}), false, "our CLI, not sim"),
            ("Bash", json!({"command": "idb list-targets"}), false, "idb listing"),
            ("Bash", json!({"command": "npx expo start"}), false, "expo without --ios"),
            ("Bash", json!({"command": "npx expo run:android"}), false, "android"),
            ("Bash", json!({"command": "flutter run -d macos"}), false, "flutter on macOS"),
            ("Bash", json!({"command": "flutter run"}), false, "flutter, default device"),
            ("Bash", json!({"command": "/bin/zsh -lc 'ls -la'"}), false, "codex, unrelated"),
            ("Read", json!({"command": "xcrun simctl boot 1234"}), false, "not a shell tool"),
            ("Edit", json!({"file_path": "a.sh", "new_string": "xcrun simctl boot X"}), false, "writing it, not running it"),
            ("Bash", json!({"foo": "bar"}), false, "no command"),
            ("Bash", json!("not an object"), false, "not an object"),
        ]
    }

    #[test]
    fn table_driven_classification() {
        for (tool_name, input, expected, why) in cases() {
            assert_eq!(is_simulator_command(tool_name, &input), expected, "{tool_name} / {input} — {why}");
        }
    }

    #[test]
    fn tool_names_match_case_insensitively() {
        let input = json!({"command": "xcrun simctl boot 1"});
        for name in ["BASH", "Shell", "Execute", "run_shell_command"] {
            assert!(is_simulator_command(name, &input), "{name}");
        }
    }

    /// ACP: the name is a title; the desktop asks about the input once the
    /// call's `execute` kind arrives.
    #[test]
    fn an_acp_call_is_judged_by_its_input() {
        assert!(!is_shell_tool("Run xcrun simctl boot X"));
        assert!(is_simulator_input(&json!({"command": "xcrun simctl boot X", "description": "Boot"})));
        assert!(!is_simulator_input(&json!({"command": "cargo test"})));
    }
}
