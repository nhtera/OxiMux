//! Normalize the primary CLI's user-turn *scaffolding* into readable text.
//!
//! A slash-command invocation is journaled wrapped as
//! `<command-message>..</command-message><command-name>/x</command-name><command-args>..</command-args>`,
//! and machine plumbing (`<local-command-*>`, `<system-reminder>`) is injected
//! into otherwise-normal user turns. Rendered verbatim, that XML leaks into the
//! conversation. This module unwraps a command to `"/x args"` and removes the
//! known wrapper blocks.
//!
//! Deliberately conservative for transcript use: [`strip_scaffolding`] only
//! touches an allowlist of machine wrappers and never blind-strips arbitrary
//! `<…>`, so a real message containing code, generics (`Vec<String>`), or HTML
//! survives intact. (The one-line *preview* helpers elsewhere are lossier on
//! purpose — a preview can drop stray tags; a rendered message cannot.)

/// A parsed slash-command invocation. `name` keeps the leading slash
/// (`/research`); `args` is the remainder the user passed (may be multi-line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub args: String,
}

impl SlashCommand {
    /// `"/name args"`, outer-trimmed. Empty args collapses to just `"/name"`.
    pub fn normalized(&self) -> String {
        format!("{} {}", self.name, self.args).trim().to_string()
    }
}

/// Parse the `<command-name>`/`<command-args>` envelope, if present. Requires a
/// non-empty `<command-name>`; `<command-args>` defaults to empty when absent.
pub fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let name = tag_inner(text, "command-name")?;
    if name.is_empty() {
        return None;
    }
    let args = tag_inner(text, "command-args").unwrap_or_default();
    Some(SlashCommand { name, args })
}

/// The known machine-wrapper blocks removed by [`strip_scaffolding`]: the
/// command envelope pieces, local-command output, injected reminders, and the
/// background-task completion notice the harness injects as a plain user turn.
/// `<ide-context/>` and other injected turns arrive flagged `isMeta` and are
/// dropped upstream, so they need no entry here.
const SCAFFOLDING_BLOCKS: &[(&str, &str)] = &[
    ("<command-message>", "</command-message>"),
    ("<command-args>", "</command-args>"),
    ("<command-name>", "</command-name>"),
    ("<local-command-caveat>", "</local-command-caveat>"),
    ("<local-command-stdout>", "</local-command-stdout>"),
    ("<local-command-stderr>", "</local-command-stderr>"),
    ("<system-reminder>", "</system-reminder>"),
    ("<task-notification>", "</task-notification>"),
];

/// Remove the known scaffolding blocks (wrapper tag + inner content) from a user
/// turn, preserving everything else verbatim. Outer whitespace is trimmed; inner
/// formatting and newlines are kept.
pub fn strip_scaffolding(text: &str) -> String {
    let mut s = text.to_string();
    for (open, close) in SCAFFOLDING_BLOCKS {
        s = remove_blocks(&s, open, close);
    }
    s.trim().to_string()
}

/// Normalize a raw user-turn body for transcript display: a slash command → its
/// `"/name args"` form; otherwise the body with scaffolding blocks removed.
pub fn normalize_user_text(text: &str) -> String {
    if let Some(cmd) = parse_slash_command(text) {
        return cmd.normalized();
    }
    strip_scaffolding(text)
}

/// Inner text of the first `<tag>…</tag>`, trimmed. `None` if either side absent.
fn tag_inner(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_string())
}

/// Remove every `open`…`close` span (inclusive) from `text`. An unterminated
/// `open` drops the remainder, so half a wrapper never renders.
fn remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_name_and_args() {
        let raw = "<command-message>research</command-message>\n\
                   <command-name>/research</command-name>\n\
                   <command-args>find the bug</command-args>";
        let cmd = parse_slash_command(raw).unwrap();
        assert_eq!(cmd.name, "/research");
        assert_eq!(cmd.args, "find the bug");
        assert_eq!(normalize_user_text(raw), "/research find the bug");
    }

    #[test]
    fn command_with_empty_args_is_just_the_name() {
        let raw = "<command-name>/clear</command-name><command-args></command-args>";
        assert_eq!(normalize_user_text(raw), "/clear");
    }

    #[test]
    fn multiline_args_are_preserved() {
        let raw = "<command-name>/plan</command-name><command-args>line one\nline two</command-args>";
        let cmd = parse_slash_command(raw).unwrap();
        assert_eq!(cmd.args, "line one\nline two");
    }

    #[test]
    fn no_command_name_is_not_a_command() {
        assert!(parse_slash_command("just a normal message").is_none());
        assert!(parse_slash_command("<command-message>x</command-message>").is_none());
    }

    #[test]
    fn strips_system_reminder_but_keeps_real_text() {
        let raw = "Please fix this.\n<system-reminder>injected context here</system-reminder>";
        assert_eq!(strip_scaffolding(raw), "Please fix this.");
    }

    #[test]
    fn strips_local_command_scaffolding() {
        let raw = "<local-command-caveat>note</local-command-caveat>real ask\n\
                   <local-command-stdout>output</local-command-stdout>";
        assert_eq!(strip_scaffolding(raw), "real ask");
    }

    #[test]
    fn preserves_angle_brackets_in_real_content() {
        // The conservative contract: never blind-strip `<…>`. Code and generics
        // in a genuine user message must survive.
        let raw = "Change `Vec<String>` to `Vec<u8>` and wrap it in <div>.";
        assert_eq!(strip_scaffolding(raw), raw);
        assert_eq!(normalize_user_text(raw), raw);
    }

    #[test]
    fn unterminated_wrapper_drops_remainder_not_prior_text() {
        let raw = "keep this<system-reminder>unterminated tail";
        assert_eq!(strip_scaffolding(raw), "keep this");
    }

    #[test]
    fn task_notification_turn_strips_to_empty() {
        // A background-task completion notice is injected as a whole user turn
        // (not meta) — stripping it leaves nothing, so the caller drops the turn.
        let raw = "<task-notification>\n<task-id>abc</task-id>\n<summary>done</summary>\n\
                   <usage><subagent_tokens>146567</subagent_tokens></usage>\n</task-notification>";
        assert_eq!(strip_scaffolding(raw), "");
        assert_eq!(normalize_user_text(raw), "");
    }
}
