//! What never leaves the host from a React Native app's traffic: secret
//! headers, every URL query value (and userinfo), and the values of known
//! secret JSON keys. The desktop's `scrub_text` runs over everything after.

/// What a redacted value reads as.
pub const REDACTED: &str = "[redacted]";

/// The longest header value kept (longer ones are cut, with `…`).
pub const MAX_HEADER_VALUE: usize = 200;

/// A header's value as it may be shown.
pub fn header(name: &str, value: &str) -> String {
    let name = name.to_ascii_lowercase();
    let secret = matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
        || ["token", "secret", "api-key", "apikey", "session", "password"].iter().any(|w| name.contains(w));
    if secret {
        return REDACTED.into();
    }
    // `Location`, `Referer`, `Link: <…>; rel=…`: a URL's query carries
    // secrets too, whatever its scheme, relative or not.
    cut(&urls_in(value), MAX_HEADER_VALUE)
}

/// `text` with every URL in it redacted ([`url`]): a word (between spaces,
/// `<>`, quotes, `;` and `,`) with a scheme (`://`), or a `?` query.
pub fn urls_in(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let is_url = word.contains("://") || (word.contains('?') && word.contains('='));
        out.push_str(&if is_url { url(word) } else { std::mem::take(word) });
        word.clear();
    };
    for c in text.chars() {
        if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | ';' | ',') {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// `url` with its userinfo and every query value redacted (keys kept).
pub fn url(url: &str) -> String {
    let (base, fragment) = url.split_once('#').map_or((url, None), |(b, f)| (b, Some(f)));
    let (path, query) = base.split_once('?').map_or((base, None), |(p, q)| (p, Some(q)));
    let mut out = without_userinfo(path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(&pairs(query));
    }
    // A fragment can carry a token too (OAuth's implicit flow).
    if fragment.is_some_and(|f| !f.is_empty()) {
        out.push_str("#[redacted]");
    }
    out
}

/// `k=v&k2=v2` with every value redacted.
fn pairs(text: &str) -> String {
    text.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) if !value.is_empty() => format!("{key}={REDACTED}"),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn without_userinfo(path: &str) -> String {
    if let Some((scheme, rest)) = path.split_once("://") {
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            return format!("{scheme}://{REDACTED}@{}", &rest[at + 1..]);
        }
    }
    path.to_owned()
}

/// Whether a JSON key names a secret.
fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    ["token", "secret", "password", "passwd", "session", "private_key"].iter().any(|w| key.contains(w))
        || matches!(key.as_str(), "apikey" | "api_key" | "authorization" | "code" | "otp" | "pin")
}

/// A request or response body: JSON with its secret keys' values redacted,
/// a form with every value redacted, anything else as it is (`scrub_text`
/// still runs over it).
pub fn body(text: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) {
        redact_json(&mut value);
        return value.to_string();
    }
    let form = !text.is_empty() && text.contains('=') && !text.chars().any(char::is_whitespace);
    if form { pairs(text) } else { text.to_owned() }
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        // A string can carry its own secrets: JSON (an echoed body), or a
        // URL with a query.
        serde_json::Value::String(text) => {
            let trimmed = text.trim_start();
            if trimmed.starts_with(['{', '[']) {
                if let Ok(mut inner) = serde_json::from_str::<serde_json::Value>(text) {
                    redact_json(&mut inner);
                    *text = inner.to_string();
                }
            } else {
                *text = urls_in(text);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if secret_key(key) && !v.is_null() {
                    *v = serde_json::Value::String(REDACTED.into());
                } else {
                    redact_json(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

/// `text` cut to at most `max` bytes (on a character boundary), `…` added.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_headers_are_redacted_and_long_ones_cut() {
        for name in ["Authorization", "cookie", "Set-Cookie", "X-Auth-Token", "x-api-key", "X-Session-Id"] {
            assert_eq!(header(name, "abc"), REDACTED, "{name}");
        }
        assert_eq!(header("Content-Type", "application/json"), "application/json");
        assert_eq!(header("Location", "https://app.example/cb?state=s1&email=a@b.co"), "https://app.example/cb?state=[redacted]&email=[redacted]");
        assert_eq!(header("Location", "/reset?code=abcdef"), "/reset?code=[redacted]", "relative");
        assert_eq!(
            header("Link", r#"<https://a.test/reset?code=abcdef>; rel="next", <https://a.test/p?page=2>; rel="prev""#),
            r#"<https://a.test/reset?code=[redacted]>; rel="next", <https://a.test/p?page=[redacted]>; rel="prev""#
        );
        assert_eq!(header("Location", "wss://a.test/live?token=T"), "wss://a.test/live?token=[redacted]");
        assert_eq!(header("Accept", "text/html,*/*;q=0.8"), "text/html,*/*;q=0.8", "no URL, nothing changed");
        let long = header("Accept", &"x".repeat(500));
        assert!(long.len() <= MAX_HEADER_VALUE + '…'.len_utf8() && long.ends_with('…'));
    }

    #[test]
    fn every_query_value_is_redacted_and_keys_kept() {
        assert_eq!(url("https://a.b/c?user=alice&token=XYZ&flag"), "https://a.b/c?user=[redacted]&token=[redacted]&flag");
        assert_eq!(url("https://u:p@a.b/c?x="), "https://[redacted]@a.b/c?x=");
        assert_eq!(url("https://a.b/cb#access_token=XYZ"), "https://a.b/cb#[redacted]");
        assert_eq!(url("http://127.0.0.1:8081/index.bundle//&platform=ios"), "http://127.0.0.1:8081/index.bundle//&platform=ios");
    }

    #[test]
    fn known_json_secrets_are_redacted_anywhere() {
        let out = body(r#"{"user":{"email":"a@b.co","password":"hunter2"},"items":[{"refresh_token":"R","n":1}],"code":"123","apiKey":"K"}"#);
        for secret in ["hunter2", "\"R\"", "123", "\"K\""] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        assert!(out.contains("a@b.co") && out.contains("\"n\":1"), "{out}");
        assert_eq!(body("grant_type=password&password=hunter2"), "grant_type=[redacted]&password=[redacted]");
        // An echo (httpbin): the request again, inside strings.
        let echo = body(r#"{"data":"{\"password\":\"P\",\"note\":\"hi\"}","url":"https://a.b/x?token=Q","args":{"user":"u"}}"#);
        assert!(!echo.contains("\\\"P\\\"") && !echo.contains("=Q"), "{echo}");
        assert!(echo.contains("hi") && echo.contains("token=[redacted]"), "{echo}");
        let said = body(r#"{"next":"go to ws://a.test/s?ticket=K now","n":"plain"}"#);
        assert!(said.contains("ticket=[redacted] now") && said.contains("plain"), "{said}");
        assert_eq!(body("plain text, not a form"), "plain text, not a form");
    }
}
