//! Exact usage from the primary CLI's account usage API.
//!
//! The CLI's OAuth deployment exposes `GET /api/oauth/usage` returning the
//! account's REAL rate-limit window utilization (the same numbers its own
//! `/usage` panel renders) — exact percentages and reset timestamps,
//! account-wide across devices. This is the meter's only data source: when
//! it can't be reached or authenticated the meter reports "unavailable"
//! with the reason rather than guessing from local logs.
//!
//! Auth: the CLI stores its OAuth credentials as a macOS Keychain generic
//! password (service `Claude Code-credentials`, account = the macOS
//! username) holding JSON `{"claudeAiOauth": {"accessToken": …}}`; some
//! setups use `<home>/.claude/.credentials.json` with the same shape.
//!
//! Transport: `curl` with its config fed via stdin (`-K -`) so the bearer
//! token never appears in process arguments (`ps` would expose it there).
//! Blocking shellouts throughout — callers run on a background executor,
//! same contract as the rest of the probe.
//!
//! Keychain ACL caveat: reading another app's Keychain item prompts the
//! user unless this binary's signing identity was previously allowed. An
//! ad-hoc-signed dev bundle gets a NEW identity every reseal, so the
//! prompt returns after each rebuild; a stable `OXIMUX_SIGN_ID` identity
//! makes "Always Allow" stick. A decline simply fails the fetch and the
//! caller surfaces "unavailable".

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use super::parse_timestamp_ms;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
/// The usage endpoint is the CLI's own contract; matching its user agent
/// keeps us aligned with what that deployment expects to serve.
const USER_AGENT: &str = "claude-code/2.1.0";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const CURL_MAX_TIME_SECS: u32 = 8;

/// One window from the API: exact utilization percent + reset time.
#[derive(Debug, Clone, PartialEq)]
pub struct OauthWindow {
    /// 0–100, straight from the API.
    pub utilization: f64,
    pub resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OauthUsage {
    pub five_hour: OauthWindow,
    pub seven_day: OauthWindow,
}

/// Why a fetch did not return usage — the caller picks its backoff and its
/// user-facing reason from this. The distinction matters: a missing/declined
/// token is a standing condition (back off long so we don't re-prompt the
/// Keychain every tick), while a rejected token or an unreachable endpoint
/// is transient (the CLI refreshes the token on its next authenticated call,
/// so retry soon and let the last-known-good snapshot cover the gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// No usable bearer token (absent credential or a declined Keychain
    /// prompt) — the CLI isn't signed in on this machine.
    NoToken,
    /// A token was sent but the endpoint rejected it (HTTP 401/403). Carries
    /// the API's own error message when the body provides one (e.g.
    /// "Invalid authentication credentials").
    Unauthorized(String),
    /// The request could not complete (offline, timeout, response drift) or
    /// the endpoint returned an unexpected status.
    Unreachable,
}

/// Fetch the account's exact usage windows.
///
/// IMPORTANT (account-safety): this performs ONLY the read-only
/// `GET /api/oauth/usage` using the token the official CLI already minted —
/// it never mints, refreshes, or rotates credentials, and never writes the
/// Keychain. Refreshing an expired token is delegated to the official CLI
/// (which the user runs normally); OxiMux must never call the OAuth token
/// endpoint itself.
pub fn fetch(home: &Path) -> Result<OauthUsage, FetchError> {
    let Some(token) = read_oauth_token(home) else {
        return Err(FetchError::NoToken);
    };
    match curl_usage_endpoint(&token) {
        Some((200, body)) => parse_usage_response(&body).ok_or(FetchError::Unreachable),
        // An expired/revoked token (signed out, rotated away) — surface the
        // API's own message so the meter can name the cause.
        Some((401 | 403, body)) => Err(FetchError::Unauthorized(parse_error_message(&body))),
        _ => Err(FetchError::Unreachable),
    }
}

/// Pull the human-readable error string from a JSON error body
/// (`{"error":{"message":"Invalid authentication credentials"}}`), falling
/// back to a generic auth message when the body lacks one.
fn parse_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "Invalid authentication credentials".to_string())
}

/// Bearer token from the Keychain item, falling back to the on-disk
/// credentials file some setups use.
///
/// Credential layout mirrors the official CLI (and matching reference
/// tools): when `CLAUDE_CONFIG_DIR` is set, CLI 2.1+ scopes its Keychain
/// service by that directory (`Claude Code-credentials-<8 hex of sha256>`),
/// so we try the scoped item first, then the legacy unsuffixed one; the
/// on-disk fallback lives under the same config dir. Read-only throughout.
fn read_oauth_token(home: &Path) -> Option<String> {
    for service in keychain_services() {
        if let Some(raw) = read_keychain_credentials(&service)
            && let Some(token) = token_from_credentials_json(&raw)
        {
            return Some(token);
        }
    }
    let raw = std::fs::read_to_string(claude_config_dir(home).join(".credentials.json")).ok()?;
    token_from_credentials_json(&raw)
}

/// The CLI's config root: `$CLAUDE_CONFIG_DIR` if set, else `<home>/.claude`.
fn claude_config_dir(home: &Path) -> std::path::PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => home.join(".claude"),
    }
}

/// Keychain service names to try, in priority order. With `CLAUDE_CONFIG_DIR`
/// set the CLI writes a scoped item — try it before the legacy name; when
/// unset only the legacy name is used.
fn keychain_services() -> Vec<String> {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => {
            vec![scoped_keychain_service(&dir), KEYCHAIN_SERVICE.to_string()]
        }
        _ => vec![KEYCHAIN_SERVICE.to_string()],
    }
}

/// The CLI 2.1+ per-config-dir Keychain service name:
/// `Claude Code-credentials-<first 8 hex of sha256(config_dir)>`.
fn scoped_keychain_service(config_dir: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(config_dir.as_bytes());
    let mut suffix = String::with_capacity(8);
    for byte in &digest[..4] {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("{KEYCHAIN_SERVICE}-{suffix}")
}

fn read_keychain_credentials(service: &str) -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", &user, "-w"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn token_from_credentials_json(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let token = v
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)?;
    (!token.is_empty()).then(|| token.to_string())
}

/// GET the usage endpoint, returning `(http_status, body)`. The
/// Authorization header travels via curl's stdin config, never argv.
/// `None` only when curl itself could not run; an HTTP error (e.g. 401 from
/// an expired token) returns its status + body so the caller can react. The
/// `fail` flag is intentionally omitted so the status line is observable.
fn curl_usage_endpoint(token: &str) -> Option<(u16, String)> {
    let config = format!(
        "url = \"{USAGE_URL}\"\n\
         silent\n\
         show-error\n\
         max-time = {CURL_MAX_TIME_SECS}\n\
         write-out = \"\\n%{{http_code}}\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         header = \"anthropic-beta: {OAUTH_BETA_HEADER}\"\n\
         header = \"User-Agent: {USER_AGENT}\"\n"
    );
    let mut child = Command::new("/usr/bin/curl")
        .args(["-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(config.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    // `write-out` appended "\n<code>" after the body; split it back off.
    let (body, code) = raw.rsplit_once('\n')?;
    let status: u16 = code.trim().parse().ok()?;
    Some((status, body.to_string()))
}

/// Parse the response body. Pure — unit-tested against a captured fixture.
/// `five_hour` is required; a missing/null `seven_day` degrades to 0 %
/// rather than failing the whole fetch.
fn parse_usage_response(body: &str) -> Option<OauthUsage> {
    let v: Value = serde_json::from_str(body).ok()?;
    let five_hour = parse_window(v.get("five_hour"))?;
    let seven_day = parse_window(v.get("seven_day")).unwrap_or(OauthWindow {
        utilization: 0.0,
        resets_at_ms: None,
    });
    Some(OauthUsage {
        five_hour,
        seven_day,
    })
}

fn parse_window(v: Option<&Value>) -> Option<OauthWindow> {
    let v = v?;
    let utilization = v.get("utilization").and_then(Value::as_f64)?;
    let resets_at_ms = v
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp_ms);
    Some(OauthWindow {
        utilization: utilization.clamp(0.0, 100.0),
        resets_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live response (2026-06-12); extra fields and null
    /// windows must be tolerated.
    const FIXTURE: &str = r#"{
        "five_hour": {"utilization": 12.0, "resets_at": "2026-06-12T21:39:59.932431+00:00"},
        "seven_day": {"utilization": 4.0, "resets_at": "2026-06-19T14:59:59.932464+00:00"},
        "seven_day_oauth_apps": null,
        "seven_day_opus": null,
        "seven_day_sonnet": {"utilization": 0.0, "resets_at": null},
        "extra_usage": {"is_enabled": true, "monthly_limit": 7000, "used_credits": 0.0}
    }"#;

    #[test]
    fn parses_live_fixture() {
        let usage = parse_usage_response(FIXTURE).unwrap();
        assert_eq!(usage.five_hour.utilization, 12.0);
        assert!(usage.five_hour.resets_at_ms.is_some());
        assert_eq!(usage.seven_day.utilization, 4.0);
        assert!(usage.seven_day.resets_at_ms.is_some());
    }

    #[test]
    fn missing_seven_day_degrades_to_zero() {
        let usage = parse_usage_response(r#"{"five_hour": {"utilization": 50.0}}"#).unwrap();
        assert_eq!(usage.seven_day.utilization, 0.0);
        assert!(usage.seven_day.resets_at_ms.is_none());
    }

    #[test]
    fn missing_five_hour_fails() {
        assert!(parse_usage_response(r#"{"seven_day": {"utilization": 4.0}}"#).is_none());
        assert!(parse_usage_response("not json").is_none());
        assert!(parse_usage_response(r#"{"five_hour": {"resets_at": null}}"#).is_none());
    }

    #[test]
    fn utilization_clamped() {
        let usage = parse_usage_response(r#"{"five_hour": {"utilization": 130.5}}"#).unwrap();
        assert_eq!(usage.five_hour.utilization, 100.0);
    }

    #[test]
    fn scoped_keychain_service_matches_cli_derivation() {
        // sha256("/Users/x/.claude") → first 8 hex chars form the suffix.
        // Value cross-checked against the official CLI's scoped item layout.
        assert_eq!(
            scoped_keychain_service("/Users/nguyenhongtien/.claude"),
            "Claude Code-credentials-01fd4c59"
        );
    }

    #[test]
    fn error_message_pulled_from_body_or_defaulted() {
        assert_eq!(
            parse_error_message(
                r#"{"error":{"type":"authentication_error","message":"Invalid authentication credentials"}}"#
            ),
            "Invalid authentication credentials"
        );
        // No usable message → generic auth fallback.
        assert_eq!(parse_error_message("{}"), "Invalid authentication credentials");
        assert_eq!(parse_error_message("not json"), "Invalid authentication credentials");
    }

    #[test]
    fn token_parses_and_rejects_empty() {
        let raw = r#"{"claudeAiOauth": {"accessToken": "tok-123", "refreshToken": "r"}}"#;
        assert_eq!(token_from_credentials_json(raw).as_deref(), Some("tok-123"));
        assert!(token_from_credentials_json(r#"{"claudeAiOauth": {"accessToken": ""}}"#).is_none());
        assert!(token_from_credentials_json("{}").is_none());
    }
}
