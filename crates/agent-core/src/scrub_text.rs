//! Secret-shaped values out of free text — a diagnostic, a log line, a crash
//! summary — before it reaches any UI or persisted surface.

/// Strip secret-shaped values from `text`. Agent children inherit OxiMux's
/// full environment (no `env_clear`), so an auth/config error could echo a
/// live token. Redacts (a) the value of every currently-set `*_API_KEY` /
/// `*_TOKEN` / `*_SECRET` env var and (b) known secret-shaped tokens
/// (`sk-ant-…`, `sk-…`).
pub fn scrub_text(text: &str) -> String {
    let mut out = text.to_string();
    for (name, value) in std::env::vars() {
        let upper = name.to_ascii_uppercase();
        let secretish = upper.ends_with("_API_KEY")
            || upper.ends_with("_TOKEN")
            || upper.ends_with("_SECRET");
        // Guard on a minimum length so a short/empty value (e.g. `TOKEN=""` or a
        // one-char flag) can't blanket-replace common substrings.
        if secretish && value.len() >= 8 && out.contains(&value) {
            out = out.replace(&value, "[redacted]");
        }
    }
    redact_prefixed(&mut out, "sk-ant-");
    redact_prefixed(&mut out, "sk-");
    out
}

/// Replace every `prefix`-led token (the prefix plus the following run of
/// non-whitespace) with `[redacted]`. Terminates because the replacement never
/// contains `prefix`.
fn redact_prefixed(text: &mut String, prefix: &str) {
    while let Some(pos) = text.find(prefix) {
        let rest = &text[pos + prefix.len()..];
        let tok_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let end = pos + prefix.len() + tok_len;
        text.replace_range(pos..end, "[redacted]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_text_strips_env_values_and_sk_tokens() {
        // A planted secret-suffixed env var value must be stripped from a
        // diagnostic; so must a bare sk-ant token that isn't sourced from env.
        // SAFETY: single-threaded test, restored immediately after.
        unsafe { std::env::set_var("OXIMUX_TEST_API_KEY", "supersecretvalue12345") };
        let dirty = "auth failed for key supersecretvalue12345 and token sk-ant-abc123XYZ.";
        let clean = scrub_text(dirty);
        unsafe { std::env::remove_var("OXIMUX_TEST_API_KEY") };
        assert!(!clean.contains("supersecretvalue12345"), "env secret leaked: {clean}");
        assert!(!clean.contains("sk-ant-abc123XYZ"), "sk-ant token leaked: {clean}");
        assert!(clean.contains("[redacted]"));
        // A too-short env value must not blanket-replace common substrings.
        unsafe { std::env::set_var("OXIMUX_TEST_TOKEN", "ab") };
        let benign = scrub_text("about the abstract abbey");
        unsafe { std::env::remove_var("OXIMUX_TEST_TOKEN") };
        assert_eq!(benign, "about the abstract abbey", "short value must not redact");
    }
}
