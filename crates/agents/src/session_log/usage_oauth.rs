//! Exact usage from the primary CLI's account usage API.
//!
//! The CLI's OAuth deployment exposes `GET /api/oauth/usage` returning the
//! account's REAL rate-limit window utilization (the same numbers its own
//! `/usage` panel renders) — exact percentages and reset timestamps,
//! account-wide across devices. This beats any local session-log estimate,
//! so the probe tries this first and falls back to the tally.
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
//! caller backs off to the estimate path.

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

/// Fetch the account's exact usage windows. `None` on ANY failure (no
/// credentials, Keychain declined, offline, drifted response shape) — the
/// caller falls back to the local estimate.
pub fn fetch(home: &Path) -> Option<OauthUsage> {
    let token = read_oauth_token(home)?;
    let body = curl_usage_endpoint(&token)?;
    parse_usage_response(&body)
}

/// Bearer token from the Keychain item, falling back to the on-disk
/// credentials file some setups use.
fn read_oauth_token(home: &Path) -> Option<String> {
    if let Some(raw) = read_keychain_credentials() {
        if let Some(token) = token_from_credentials_json(&raw) {
            return Some(token);
        }
    }
    let raw = std::fs::read_to_string(home.join(".claude").join(".credentials.json")).ok()?;
    token_from_credentials_json(&raw)
}

fn read_keychain_credentials() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &user,
            "-w",
        ])
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

/// GET the usage endpoint. The Authorization header travels via curl's
/// stdin config, never argv.
fn curl_usage_endpoint(token: &str) -> Option<String> {
    let config = format!(
        "url = \"{USAGE_URL}\"\n\
         silent\n\
         show-error\n\
         fail\n\
         max-time = {CURL_MAX_TIME_SECS}\n\
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
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
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
    fn token_parses_and_rejects_empty() {
        let raw = r#"{"claudeAiOauth": {"accessToken": "tok-123", "refreshToken": "r"}}"#;
        assert_eq!(token_from_credentials_json(raw).as_deref(), Some("tok-123"));
        assert!(token_from_credentials_json(r#"{"claudeAiOauth": {"accessToken": ""}}"#).is_none());
        assert!(token_from_credentials_json("{}").is_none());
    }
}
