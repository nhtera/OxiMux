//! Parse a browser-extension cookie export (`From File…`).
//!
//! The de-facto interchange format (Cookie-Editor, EditThisCookie, …) is a JSON
//! array of cookie objects: `domain`, `name`, `value` required; `path`,
//! `secure`, `httpOnly`, `sameSite`, `expirationDate` optional. We're lenient
//! about field types (booleans arrive as `true`/`1`; `sameSite` as a string or
//! a Chromium enum number) and skip malformed entries rather than failing the
//! whole import.

use super::{ParsedCookie, SameSite};

fn as_bool(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

/// Normalize `sameSite` from either a string (`"no_restriction"`, `"lax"`,
/// `"strict"`, `"unspecified"`/`"none"`) or a Chromium enum number.
fn parse_samesite(v: Option<&serde_json::Value>) -> SameSite {
    match v {
        Some(serde_json::Value::String(s)) => match s.to_lowercase().as_str() {
            "no_restriction" | "none" => SameSite::None,
            "lax" => SameSite::Lax,
            "strict" => SameSite::Strict,
            _ => SameSite::Unspecified,
        },
        Some(serde_json::Value::Number(n)) => match n.as_i64().unwrap_or(-1) {
            0 => SameSite::None,
            1 => SameSite::Lax,
            2 => SameSite::Strict,
            _ => SameSite::Unspecified,
        },
        _ => SameSite::Unspecified,
    }
}

/// Parse a cookie-export JSON document into importable cookies.
pub fn parse(json: &str) -> Result<Vec<ParsedCookie>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let array = value.as_array().ok_or("expected a JSON array of cookies")?;

    let mut out = Vec::new();
    for entry in array {
        let Some(obj) = entry.as_object() else { continue };
        let domain = obj.get("domain").and_then(|v| v.as_str()).unwrap_or("").trim();
        let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if domain.is_empty() || name.is_empty() {
            continue;
        }
        let value = obj.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let path = obj.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty());
        let expires = obj
            .get("expirationDate")
            .and_then(|v| v.as_f64())
            .filter(|n| *n > 0.0)
            .map(|n| n as i64);
        out.push(ParsedCookie {
            host: domain.to_string(),
            name: name.to_string(),
            value,
            path: path.unwrap_or("/").to_string(),
            secure: as_bool(obj.get("secure")),
            same_site: parse_samesite(obj.get("sameSite")),
            expires_unix_s: expires,
        });
    }
    if out.is_empty() {
        return Err("no valid cookies in file".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_editor_export() {
        let json = r#"[
            {"domain":".github.com","name":"user_session","value":"abc",
             "path":"/","secure":true,"httpOnly":true,"sameSite":"lax",
             "expirationDate":4102444800},
            {"domain":"example.com","name":"x","value":"1","secure":1,"httpOnly":0},
            {"domain":"","name":"bad","value":"skip"},
            {"name":"nodomain","value":"skip"}
        ]"#;
        let cookies = parse(json).unwrap();
        assert_eq!(cookies.len(), 2);
        let gh = &cookies[0];
        assert_eq!(gh.host, ".github.com");
        assert_eq!(gh.name, "user_session");
        assert!(gh.secure);
        assert_eq!(gh.expires_unix_s, Some(4102444800));
        assert!(matches!(gh.same_site, SameSite::Lax));
        // Numeric secure + default path.
        assert!(cookies[1].secure);
        assert_eq!(cookies[1].path, "/");
        assert_eq!(cookies[1].expires_unix_s, None);
    }

    #[test]
    fn rejects_non_array_and_empty() {
        assert!(parse("{}").is_err());
        assert!(parse("[]").is_err());
        assert!(parse("not json").is_err());
    }
}
