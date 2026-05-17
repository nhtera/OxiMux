//! Conventional-commit prefix table + message assembly. Pure functions so
//! unit tests run without GPUI.
//!
//! The dialog cycles through this fixed list when the user clicks the prefix
//! button. `None` is the first option — selecting it produces a message with
//! no prefix (useful for prose-only subjects like "WIP" or "merge fixup").

/// 11 standard conventional-commit prefixes + the empty (no-prefix) slot at
/// index 0. Order taken from the conventional-commits spec, with `feat` and
/// `fix` first because they're the most common.
pub const PREFIXES: &[Option<&str>] = &[
    None,
    Some("feat"),
    Some("fix"),
    Some("docs"),
    Some("refactor"),
    Some("test"),
    Some("chore"),
    Some("style"),
    Some("perf"),
    Some("build"),
    Some("ci"),
    Some("revert"),
];

/// Subject-line soft warning ceiling. Conventional commits says ≤ 50 char
/// subject; we surface a colored ✱ when exceeded but never block.
pub const SUBJECT_WARN_LEN: usize = 50;

/// Cycle the prefix index forward by one, wrapping at the end.
pub fn next_prefix_index(current: usize) -> usize {
    (current + 1) % PREFIXES.len()
}

/// Label shown on the prefix button. `None` slot → `"(no prefix)"`.
pub fn prefix_label(idx: usize) -> &'static str {
    match PREFIXES.get(idx).copied().flatten() {
        Some(s) => s,
        None => "(no prefix)",
    }
}

/// Glue prefix + subject + body into the final commit message.
///
/// Format:
///   `<prefix>: <subject>\n\n<body>`  (when both prefix and body present)
///   `<prefix>: <subject>`            (no body)
///   `<subject>\n\n<body>`            (no prefix, body present)
///   `<subject>`                      (neither)
///
/// Subject and body are trimmed; empty body becomes no body (no trailing
/// blank line). Empty subject returns `None` — the caller surfaces a UI
/// error rather than committing junk.
pub fn assemble_message(prefix_idx: usize, subject: &str, body: &str) -> Option<String> {
    let subject = subject.trim();
    if subject.is_empty() {
        return None;
    }
    let body = body.trim();
    let prefix = PREFIXES.get(prefix_idx).copied().flatten();
    let subject_line = match prefix {
        Some(p) => format!("{p}: {subject}"),
        None => subject.to_string(),
    };
    if body.is_empty() {
        Some(subject_line)
    } else {
        Some(format!("{subject_line}\n\n{body}"))
    }
}

/// True when subject exceeds the soft warning ceiling.
pub fn subject_over_warn(subject: &str) -> bool {
    subject.chars().count() > SUBJECT_WARN_LEN
}
