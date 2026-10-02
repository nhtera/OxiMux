//! One indexable unit of a transcript, and the shaping every unit goes through
//! before it reaches the index: the tool-output cap, long-prose chunking, and the identifier split that lets
//! `resolveTerminalPath` answer to `terminal path`.

/// Who produced a turn. Each FTS row fills exactly one text column by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    User,
    Assistant,
    /// A tool call (`"<name>: <arg>"`) or its output.
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

/// A parsed transcript turn, before shaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Record timestamp in unix ms, when the line carried one.
    pub ts_ms: Option<i64>,
}

/// Session metadata gathered from one read of a transcript (a whole file, or
/// just its appended tail). The writer merges it into the stored row:
/// first-seen fields keep the old value, last-wins fields take the new one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaDelta {
    /// Codex: the `session_meta` id (Claude's is the file stem).
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// Last wins.
    pub custom_title: Option<String>,
    /// Last wins.
    pub ai_title: Option<String>,
    /// First wins (matches the history picker's title choice).
    pub last_prompt: Option<String>,
    /// First wins.
    pub first_prompt: Option<String>,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    /// User + assistant records seen in this read.
    pub message_count: i64,
}

impl MetaDelta {
    pub fn note_ts(&mut self, ts: Option<i64>) {
        if let Some(ts) = ts {
            self.first_ts_ms.get_or_insert(ts);
            self.last_ts_ms = Some(self.last_ts_ms.map_or(ts, |t| t.max(ts)));
        }
    }
}

/// Tool rows keep only their head: command output is long and repetitive.
pub const TOOL_CAP_CHARS: usize = 3072;
/// User/assistant prose is split into rows of about this size so one huge
/// message does not dominate bm25 length normalization.
pub const CHUNK_CHARS: usize = 8000;
/// Longest tool argument kept in a `"<name>: <arg>"` row.
pub const TOOL_ARG_CHARS: usize = 2000;
/// Identifier pieces per row.
const MAX_IDENTIFIER_TERMS: usize = 4000;

/// A shaped row ready for the writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub role: Role,
    pub text: String,
    pub identifiers: String,
    pub ts_ms: Option<i64>,
}

/// Shape one turn into one or more index rows (empty text → none).
pub fn shape(turn: Turn) -> Vec<Row> {
    if turn.text.trim().is_empty() {
        return Vec::new();
    }
    let pieces = match turn.role {
        Role::Tool => vec![head_chars(&turn.text, TOOL_CAP_CHARS)],
        Role::User | Role::Assistant => chunk(&turn.text, CHUNK_CHARS),
    };
    pieces
        .into_iter()
        .map(|text| Row { role: turn.role, identifiers: identifiers(&text), text, ts_ms: turn.ts_ms })
        .collect()
}

/// A tool call as one searchable line: `"<name>: <arg>"`, where `arg` is the
/// first argument that says what the call did. A command given as an argv
/// array is joined (a `bash -lc <script>` wrapper keeps only the script).
pub fn tool_call_text(name: &str, args: &serde_json::Value) -> String {
    use serde_json::Value;
    const KEYS: [&str; 7] = ["command", "cmd", "file_path", "path", "pattern", "query", "description"];
    let arg = KEYS.iter().find_map(|k| match args.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [] => None,
                [shell, flag, script] if shell.ends_with("sh") && flag.starts_with('-') => Some(script.to_string()),
                _ => Some(words.join(" ")),
            }
        }
        _ => None,
    });
    match arg {
        Some(a) => format!("{name}: {}", head_chars(&a, TOOL_ARG_CHARS)),
        None => name.to_string(),
    }
}

/// First `max` chars of `s`.
pub fn head_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Split `s` into pieces of at most ~`max` chars, breaking on whitespace when
/// one is available in the back half of the window.
fn chunk(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some((cut, _)) = rest.char_indices().nth(max) else {
            out.push(rest.to_string());
            break;
        };
        let window = &rest[..cut];
        let at = window
            .rfind(char::is_whitespace)
            .filter(|i| *i > cut / 2)
            .unwrap_or(cut);
        out.push(rest[..at].to_string());
        rest = rest[at..].trim_start();
    }
    out
}

/// The tokenizer's word characters: letters, digits, marks, and `_ . - / +`
/// (`tokenchars` in the FTS table definition). Query planning uses the same
/// class so a quoted query term always tokenizes the way the index did.
pub fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | '+') || is_mark(c)
}

fn is_mark(c: char) -> bool {
    // Combining marks (accents written as separate code points).
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Pieces of compound identifiers in `text` — camelCase humps and
/// `_ . - / +` separated parts — lowercased, deduped, space-joined. Plain
/// words contribute nothing (they are already in the text column).
///
/// Separators are word characters to the tokenizer (so `cli.mjs` stays one
/// term), which also glues sentence punctuation on: `done.` indexes as
/// `done.`. The trimmed word is emitted here so prose still matches.
pub fn identifiers(text: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut emit = |p: String| {
        if out.len() < MAX_IDENTIFIER_TERMS && p.chars().count() >= 2 && seen.insert(p.clone()) {
            out.push(p);
        }
    };
    for token in text.split(|c: char| !is_token_char(c)) {
        let len = token.chars().count();
        if !(2..=120).contains(&len) {
            continue;
        }
        let trimmed = token.trim_matches(is_separator);
        if trimmed.len() != token.len() && !trimmed.is_empty() {
            emit(trimmed.to_lowercase());
        }
        if len < 3 {
            continue;
        }
        let parts = split_identifier(trimmed);
        if parts.len() >= 2 {
            parts.into_iter().for_each(&mut emit);
        }
    }
    out.join(" ")
}

fn is_separator(c: char) -> bool {
    matches!(c, '_' | '.' | '-' | '/' | '+')
}

/// `resolveTerminalPath` → `[resolve, terminal, path]`; `src/foo_bar.rs` →
/// `[src, foo, bar, rs]`; `HTTPServer` → `[http, server]`.
fn split_identifier(token: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for seg in token.split(is_separator) {
        let chars: Vec<char> = seg.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let (prev, cur) = (chars[i - 1], chars[i]);
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (prev.is_lowercase() && cur.is_uppercase())
                || (prev.is_uppercase() && cur.is_uppercase() && next_lower)
                || (prev.is_alphabetic() != cur.is_alphabetic());
            if boundary {
                parts.push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        if start < chars.len() {
            parts.push(chars[start..].iter().collect::<String>().to_lowercase());
        }
    }
    parts.retain(|p| !p.is_empty());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, text: &str) -> Turn {
        Turn { role, text: text.into(), ts_ms: None }
    }

    #[test]
    fn identifiers_split_camel_case_and_paths() {
        assert_eq!(identifiers("call resolveTerminalPath now"), "resolve terminal path");
        assert_eq!(identifiers("open src/foo_bar.rs"), "src foo bar rs");
        assert_eq!(identifiers("HTTPServer"), "http server");
        assert_eq!(identifiers("plain words only"), "");
        // Sentence punctuation the tokenizer keeps is trimmed off here.
        assert_eq!(identifiers("all done. see ./notes"), "done notes");
    }

    #[test]
    fn tool_rows_are_capped_and_prose_is_chunked() {
        let long = "word ".repeat(5000);
        let rows = shape(turn(Role::Tool, &long));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.chars().count(), TOOL_CAP_CHARS);
        let rows = shape(turn(Role::User, &long));
        assert!(rows.len() >= 3);
        assert!(rows.iter().all(|r| r.text.chars().count() <= CHUNK_CHARS));
        // Breaks land on whitespace, so no word is split.
        assert!(rows.iter().all(|r| r.text.split_whitespace().all(|w| w == "word")));
    }

    #[test]
    fn blank_turns_are_dropped() {
        assert!(shape(turn(Role::User, "  \n ")).is_empty());
    }

    #[test]
    fn chunk_handles_text_without_whitespace() {
        let s = "x".repeat(CHUNK_CHARS * 2 + 5);
        let parts = chunk(&s, CHUNK_CHARS);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), s);
    }
}
