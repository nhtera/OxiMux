//! Recognising a tool call that captured the iOS Simulator's screen, so
//! [`crate::redact`] can keep it off surfaces that leave this machine.
//!
//! Two shapes, whatever the agent's tool is called (Claude's `Bash`, Codex's
//! `shell`/`exec_command` with an argv array, an ACP `execute`, a `Read`):
//!
//! - a command that runs `oximux sim screenshot` or `oximux sim ax` — the
//!   screen's contents come back as its output (the AX tree is the screen's
//!   text);
//! - anything that names a file under an `oximux-sim/` folder — where
//!   `oximux sim screenshot` saves its PNGs, so a `Read` of one is the
//!   screenshot itself.
//!
//! Matched on every string in the tool's input rather than a known field, so a
//! new agent's input shape is covered without a change here.
//!
//! Not covered, by design: a screenshot saved elsewhere with `--out` and read
//! back from there (nothing ties that path to the simulator), and terminal
//! tabs, whose mirroring shows whatever the terminal shows. The skill guide
//! says so.

use serde_json::Value;

/// The folder `oximux sim screenshot` saves into (under `$TMPDIR`).
pub const CAPTURE_DIR: &str = "oximux-sim";

/// The `oximux sim` verbs whose output is the screen's contents.
const CAPTURE_VERBS: [&str; 2] = ["screenshot", "ax"];

/// Does this tool input capture the simulator's screen?
pub fn is_simulator_capture(input: &Value) -> bool {
    let mut strings = Vec::new();
    collect(input, &mut strings);
    strings.iter().any(|s| names_capture_file(s)) || runs_capture_verb(&strings.join(" "))
}

/// Keys that name the program being run: read first, so the words of a
/// command come out in order whatever order the object's keys sort in
/// (`{"args": ["sim", "ax"], "command": "oximux"}` is `oximux sim ax`).
const PROGRAM_KEYS: [&str; 4] = ["command", "cmd", "program", "executable"];

fn collect<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        Value::Object(map) => {
            let program = |k: &str| PROGRAM_KEYS.contains(&k);
            map.iter().filter(|(k, _)| program(k)).for_each(|(_, v)| collect(v, out));
            map.iter().filter(|(k, _)| !program(k)).for_each(|(_, v)| collect(v, out));
        }
        _ => {}
    }
}

/// A path with an `oximux-sim` directory component.
fn names_capture_file(s: &str) -> bool {
    s.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '='))
        .any(|word| word.split('/').rev().skip(1).any(|part| part == CAPTURE_DIR))
}

/// Flags of the CLI that take a value, so the value is not read as the verb.
const VALUE_FLAGS: [&str; 4] = ["--worktree", "--timeout", "--host", "--dir"];

/// `oximux … sim … screenshot|ax` as words of one command.
fn runs_capture_verb(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '"' | '\''))
        .filter(|w| !w.is_empty())
        .collect();
    (0..words.len()).any(|i| {
        if words[i].rsplit('/').next() != Some("oximux") {
            return false;
        }
        let mut at = i + 1;
        next_word(&words, &mut at) == Some("sim")
            && next_word(&words, &mut at).is_some_and(|verb| CAPTURE_VERBS.contains(&verb))
    })
}

/// The next word from `*at` that is not a flag (or a flag's value).
fn next_word<'a>(words: &[&'a str], at: &mut usize) -> Option<&'a str> {
    while let Some(word) = words.get(*at) {
        *at += 1;
        if word.starts_with('-') {
            if VALUE_FLAGS.contains(word) {
                *at += 1;
            }
            continue;
        }
        return Some(word);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capture_commands_in_every_agents_shape() {
        for input in [
            json!({"command": "oximux sim screenshot"}),
            json!({"command": "cd app && oximux --json sim ax --flat"}),
            json!({"command": "oximux sim --worktree . screenshot --full"}),
            json!({"command": "/usr/local/bin/oximux --timeout 30 sim ax"}),
            json!({"command": ["bash", "-lc", "oximux sim screenshot --out /tmp/a.png"]}),
            json!({"command": ["oximux", "sim", "ax"]}),
            json!({"cmd": "sleep 1; oximux sim screenshot"}),
            json!({"args": ["sim", "ax"], "command": "oximux"}),
        ] {
            assert!(is_simulator_capture(&input), "{input}");
        }
    }

    #[test]
    fn reading_a_saved_screenshot_is_a_capture() {
        for input in [
            json!({"file_path": "/var/folders/xy/T/oximux-sim/screenshot-1.png"}),
            json!({"path": "/tmp/oximux-sim/screenshot-2.png"}),
            json!({"command": "open \"/private/tmp/oximux-sim/x.png\""}),
        ] {
            assert!(is_simulator_capture(&input), "{input}");
        }
    }

    #[test]
    fn other_simulator_work_and_lookalikes_are_not() {
        for input in [
            json!({"command": "oximux sim tap 10 20"}),
            json!({"command": "oximux sim status"}),
            json!({"command": "oximux ls"}),
            json!({"command": "xcrun simctl io booted screenshot shot.png"}),
            json!({"command": "echo oximux-sim"}),
            json!({"file_path": "/work/oximux-simulator/notes.md"}),
            json!({"file_path": "/work/app/screenshot.png"}),
            json!({"command": "grep -r 'sim ax' docs/"}),
        ] {
            assert!(!is_simulator_capture(&input), "{input}");
        }
    }
}
