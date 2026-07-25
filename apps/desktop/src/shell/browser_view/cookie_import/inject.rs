//! Pure helpers for cookie injection — kept out of `native.rs` so they're
//! unit-testable without a live webview. The objc2 `WKHTTPCookieStore` work
//! itself lives in `native.rs` (the webview/objc2 boundary) and calls these.

/// Google "integrity" cookies are cryptographically bound to the source
/// browser's TLS fingerprint; transplanting them makes Google reject the whole
/// session. The rest of the session cookies still log the user in, so we drop
/// only these and keep everything else.
const INTEGRITY_COOKIE_NAMES: &[&str] =
    &["SIDCC", "__Secure-1PSIDCC", "__Secure-3PSIDCC", "__Secure-STRP", "AEC"];

/// Whether a cookie should be injected (false = skip an integrity cookie on a
/// Google host).
pub fn should_import(host: &str, name: &str) -> bool {
    let h = host.trim_start_matches('.').to_lowercase();
    let is_google = h == "google.com" || h.ends_with(".google.com");
    !(is_google && INTEGRITY_COOKIE_NAMES.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_google_integrity_cookies_only() {
        assert!(!should_import(".google.com", "SIDCC"));
        assert!(!should_import("google.com", "__Secure-3PSIDCC"));
        // Same name on a non-Google host is fine.
        assert!(should_import(".example.com", "SIDCC"));
        // Other Google cookies pass.
        assert!(should_import(".google.com", "SID"));
        assert!(should_import(".google.com", "__Secure-1PSID"));
    }
}
