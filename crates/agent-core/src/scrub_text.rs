//! Secret-shaped values out of free text — a diagnostic, a log line, a crash
//! summary — before it reaches any UI or persisted surface.

/// Strip secret-shaped values from `text`. Agent children inherit OxiMux's
/// full environment (no `env_clear`), so an auth/config error could echo a
/// live token; an app's log can print one too. Redacts (a) the value of
/// every currently-set `*_API_KEY` / `*_TOKEN` / `*_SECRET` env var, (b)
/// known secret-shaped tokens (`sk-ant-…`, `sk-…`, a JWT, an AWS access key,
/// a `Bearer` credential) and (c) the value of a `token=`, `key=`, `code=`
/// (and the like) parameter.
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
    redact_prefixed(&mut out, "Bearer ");
    redact_jwts(&mut out);
    redact_aws_keys(&mut out);
    redact_secret_params(&mut out);
    out
}

const REDACTED: &str = "[redacted]";

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// The end of the run of `is_b64url` bytes starting at `from`.
fn b64url_end(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..].iter().take_while(|b| is_b64url(**b)).count()
}

/// `eyJ….….…`: a JSON Web Token (its header always starts `{"`, which is
/// `eyJ` in base64url), three dot-separated base64url parts.
fn redact_jwts(text: &mut String) {
    let mut from = 0;
    while let Some(found) = text[from..].find("eyJ") {
        let start = from + found;
        let bytes = text.as_bytes();
        let header = b64url_end(bytes, start);
        let token = (bytes.get(header) == Some(&b'.') && header - start >= 10)
            .then(|| b64url_end(bytes, header + 1))
            .filter(|payload| payload - header > 10 && bytes.get(*payload) == Some(&b'.'))
            .map(|payload| b64url_end(bytes, payload + 1));
        match token {
            Some(end) => {
                text.replace_range(start..end, REDACTED);
                from = start + REDACTED.len();
            }
            None => from = start + 3,
        }
    }
}

/// `AKIA…`/`ASIA…` and 16 more capitals or digits: an AWS access key id.
fn redact_aws_keys(text: &mut String) {
    let mut from = 0;
    while from < text.len() {
        let bytes = text.as_bytes();
        let Some(found) = (from..bytes.len().saturating_sub(19)).find(|&i| {
            (bytes[i..].starts_with(b"AKIA") || bytes[i..].starts_with(b"ASIA"))
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && bytes[i + 4..i + 20].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && bytes.get(i + 20).is_none_or(|b| !b.is_ascii_alphanumeric())
        }) else {
            return;
        };
        text.replace_range(found..found + 20, REDACTED);
        from = found + REDACTED.len();
    }
}

/// A parameter name whose value is a credential: `token`, `key`, `code`, …,
/// or one ending like them (`access_token`, `apiKey`, `client-secret`).
fn names_a_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const EXACT: [&str; 9] = ["key", "token", "code", "secret", "password", "passwd", "auth", "sig", "signature"];
    const ENDS: [&str; 7] = ["_key", "-key", "apikey", "token", "_secret", "-secret", "password"];
    EXACT.contains(&name.as_str()) || ENDS.iter().any(|end| name.ends_with(end))
}

/// The value after `name=` for every [`names_a_secret`] name: a URL's query,
/// a form body, a `key=value` log line.
fn redact_secret_params(text: &mut String) {
    let name_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let value_end = |b: &u8| b.is_ascii_whitespace() || matches!(b, b'&' | b'"' | b'\'' | b',' | b';' | b')' | b'>' | b'#');
    let mut from = 0;
    while let Some(found) = text[from..].find('=') {
        let eq = from + found;
        let bytes = text.as_bytes();
        let start = (0..eq).rev().take_while(|&i| name_byte(bytes[i])).last().unwrap_or(eq);
        // A quoted value (`password="…"`) runs to its closing quote.
        let quote = bytes.get(eq + 1).copied().filter(|b| matches!(b, b'"' | b'\''));
        let at = eq + 1 + usize::from(quote.is_some());
        let end = at + match quote {
            Some(q) => bytes[at..].iter().take_while(|b| **b != q && **b != b'\n').count(),
            None => bytes[at..].iter().take_while(|b| !value_end(b)).count(),
        };
        let value = &text[at..end];
        // `code=1` is an exit code; an OAuth code is long.
        let long_enough = value.len() >= if text[start..eq].eq_ignore_ascii_case("code") { 8 } else { 1 };
        if start < eq && names_a_secret(&text[start..eq]) && long_enough && value != REDACTED {
            text.replace_range(at..end, REDACTED);
            from = at + REDACTED.len();
        } else {
            from = eq + 1;
        }
    }
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

    #[test]
    fn a_jwt_an_aws_key_and_a_bearer_credential_are_redacted() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let clean = scrub_text(&format!("auth header {jwt} rejected"));
        assert_eq!(clean, "auth header [redacted] rejected");
        assert_eq!(scrub_text("key AKIAIOSFODNN7EXAMPLE, done"), "key [redacted], done");
        assert_eq!(scrub_text("Authorization: Bearer abc.def.ghi"), "Authorization: [redacted]");
    }

    #[test]
    fn a_secret_parameter_keeps_its_name_and_loses_its_value() {
        assert_eq!(
            scrub_text("GET https://api.example.com/cb?code=4/0AX8f2kQ&state=ok&access_token=ya29.a0 HTTP/1.1"),
            "GET https://api.example.com/cb?code=[redacted]&state=ok&access_token=[redacted] HTTP/1.1"
        );
        assert_eq!(scrub_text("login apiKey=abc123 user=me"), "login apiKey=[redacted] user=me");
        assert_eq!(scrub_text(r#"{"password":"x"} pass password="hunter2""#), r#"{"password":"x"} pass password="[redacted]""#);
    }

    #[test]
    fn prose_and_look_alikes_are_left_alone() {
        for text in [
            "The app crashed: Index out of range at line 42",
            "monkey business; x = 3; a==b",
            "AKIA is not a key on its own, nor is AKIAshort",
            "eyJust a word.eyJnot.a token",
            "Fatal error: Unexpectedly found nil (state=Foreground)",
            "process exited with code=1",
        ] {
            assert_eq!(scrub_text(text), text);
        }
    }
}
