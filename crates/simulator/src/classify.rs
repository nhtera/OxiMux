//! Classifies a tool call as "runs a command that talks to, or targets, the
//! iOS Simulator" — used to route Computer Use-style consent and gating for
//! agents that reach the simulator through a plain shell tool rather than the
//! panel's own verbs (moved here from the P9 plan per this crate's file
//! table).
//!
//! Detection is deliberately conservative: it looks at each shell
//! *statement*'s leading command word, not a substring match over the whole
//! command line. `echo simctl` and `echo "xcrun simctl boot"` both name
//! `echo` as the statement's leading word, so neither is classified as a
//! simulator command — they print text, they don't run one. A substring scan
//! would get both of those wrong.

use serde_json::Value;

/// Tool names treated as "runs a shell command", case-insensitively. Agent
/// SDKs spell the same capability differently.
const SHELL_TOOL_NAMES: &[&str] = &["bash", "shell", "run_terminal_cmd", "exec_command"];

/// True when `tool_name(input)` runs a command that boots, drives, or builds
/// for the iOS Simulator: `xcrun simctl …`, an `xcodebuild` invocation
/// targeting a simulator destination or SDK, `open -a Simulator`,
/// `oximux sim …`, `idb …`, `maestro …`, or an Expo/React Native/Flutter run
/// aimed at an iOS simulator device.
pub fn is_simulator_command(tool_name: &str, input: &Value) -> bool {
    if !SHELL_TOOL_NAMES.iter().any(|n| n.eq_ignore_ascii_case(tool_name)) {
        return false;
    }
    let Some(command) = extract_command(input) else { return false };
    statements(&command).iter().any(|stmt| statement_matches(stmt))
}

/// Pulls a single command-line string out of the tool input, handling the
/// shapes this repo's agent adapters actually send:
/// - `{"command": "xcrun simctl boot ..."}`
/// - `{"cmd": ["xcodebuild", "-scheme", ...]}` (argv array, space-joined)
/// - `{"command": ["bash", "-lc", "xcrun simctl boot ..."]}` (a shell
///   launcher wrapper — the real command is the trailing argument, not the
///   joined argv, so it's unwrapped rather than joined)
fn extract_command(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let raw = obj.get("command").or_else(|| obj.get("cmd"))?;
    match raw {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let strs: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if strs.is_empty() {
                return None;
            }
            if strs.len() >= 3 && is_shell_launcher(strs[0]) && strs[1].starts_with('-') {
                return Some(strs[strs.len() - 1].to_owned());
            }
            Some(strs.join(" "))
        }
        _ => None,
    }
}

fn is_shell_launcher(program: &str) -> bool {
    matches!(basename(program), "bash" | "sh" | "zsh")
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Splits a command line into shell statements on `&&`, `||`, `;`, and `|`.
/// This is a rough split — it doesn't unwind `$(...)`, here-docs, or quoting
/// — adequate for classification, not for execution.
fn statements(command: &str) -> Vec<String> {
    command
        .replace("&&", ";")
        .replace("||", ";")
        .split(['|', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn statement_matches(stmt: &str) -> bool {
    let tokens: Vec<&str> = stmt.split_whitespace().collect();
    let Some(&head) = tokens.first() else { return false };
    match basename(head) {
        "xcrun" => tokens.get(1).copied() == Some("simctl"),
        "xcodebuild" => {
            let lower = stmt.to_lowercase();
            // `-destination`'s value is a quoted, comma-separated string
            // (`platform=iOS Simulator,name=...`), so it doesn't tokenize
            // cleanly on whitespace — substring-match the whole statement
            // instead of trying to isolate the flag's argument.
            lower.contains("-sdk iphonesimulator") || (lower.contains("-destination") && lower.contains("simulator"))
        }
        "open" => tokens.windows(2).any(|w| w[0] == "-a" && w[1].to_lowercase().starts_with("simulator")),
        "oximux" => tokens.get(1).copied() == Some("sim"),
        "idb" => true,
        "maestro" => true,
        "npx" => tokens.get(1).copied() == Some("expo") && tokens.get(2).copied() == Some("run:ios"),
        "expo" => tokens.get(1).copied() == Some("run:ios"),
        "react-native" => tokens.get(1).copied() == Some("run-ios"),
        "flutter" => tokens.get(1).copied() == Some("run") && flutter_targets_ios_sim(&tokens),
        _ => false,
    }
}

/// `flutter run -d <device>` counts only when `<device>` looks like an iOS
/// Simulator target (`iPhone …`, `iPad …`, the literal `ios`, or a name
/// containing "simulator") — `flutter run -d macos` or a bare `flutter run`
/// (physical/default device) must not match.
fn flutter_targets_ios_sim(tokens: &[&str]) -> bool {
    tokens.iter().position(|&t| t == "-d").and_then(|i| tokens.get(i + 1)).is_some_and(|d| {
        let lower = d.to_lowercase();
        lower.contains("iphone") || lower.contains("ipad") || lower == "ios" || lower.contains("simulator")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Table-driven: `(tool_name, input, expected, why)`.
    fn cases() -> Vec<(&'static str, Value, bool, &'static str)> {
        vec![
            ("Bash", json!({"command": "xcrun simctl boot 1234"}), true, "direct simctl boot"),
            ("bash", json!({"command": "cd /tmp && idb list-targets"}), true, "idb after a chained cd"),
            (
                "shell",
                json!({"cmd": ["xcodebuild", "-scheme", "App", "-destination", "platform=iOS Simulator,name=iPhone 15"]}),
                true,
                "argv array, simulator destination",
            ),
            (
                "run_terminal_cmd",
                json!({"command": ["bash", "-lc", "open -a Simulator"]}),
                true,
                "bash -lc wrapper is unwrapped to its trailing arg",
            ),
            ("exec_command", json!({"command": "oximux sim boot iPhone-15"}), true, "our own CLI verb"),
            ("Bash", json!({"command": "npx expo run:ios"}), true, "expo run:ios"),
            ("Bash", json!({"command": "react-native run-ios --simulator=\"iPhone 15\""}), true, "RN run-ios"),
            ("Bash", json!({"command": "flutter run -d \"iPhone 15 Pro\""}), true, "flutter targeting an iPhone"),
            ("Bash", json!({"command": "maestro test flow.yaml"}), true, "maestro"),
            ("Bash", json!({"command": "xcodebuild -scheme App -sdk iphonesimulator build"}), true, "sdk flag"),
            // Negatives.
            ("Bash", json!({"command": "echo simctl"}), false, "printing the word, not running it"),
            ("Bash", json!({"command": "echo \"xcrun simctl boot\""}), false, "quoted text, echo is the leading word"),
            ("Bash", json!({"command": "xcrun xctrace record"}), false, "xcrun but not simctl"),
            ("Bash", json!({"command": "open -a Xcode"}), false, "open, but not Simulator"),
            ("Bash", json!({"command": "oximux status"}), false, "our CLI, but not the sim subcommand"),
            ("Bash", json!({"command": "flutter run -d macos"}), false, "flutter targeting macOS, not an iOS sim"),
            ("Bash", json!({"command": "flutter run"}), false, "flutter with no -d at all"),
            ("Bash", json!({"command": "xcodebuild -scheme App -sdk macosx build"}), false, "macOS sdk"),
            ("Read", json!({"command": "xcrun simctl boot 1234"}), false, "not a shell-like tool"),
            ("Bash", json!({"foo": "bar"}), false, "no command/cmd field"),
            ("Bash", json!("not an object"), false, "input is not an object"),
        ]
    }

    #[test]
    fn table_driven_classification() {
        for (tool_name, input, expected, why) in cases() {
            assert_eq!(is_simulator_command(tool_name, &input), expected, "{tool_name} / {input} — {why}");
        }
    }

    #[test]
    fn tool_name_matching_is_case_insensitive() {
        let input = json!({"command": "xcrun simctl boot 1"});
        assert!(is_simulator_command("BASH", &input));
        assert!(is_simulator_command("Shell", &input));
    }
}
