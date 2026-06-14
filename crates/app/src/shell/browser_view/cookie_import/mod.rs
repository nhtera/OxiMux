//! Cookie import — read an installed browser's cookies and inject them into the
//! inline browser's **active** profile, so the user is logged into sites without
//! re-authenticating. User-initiated only (a profile-menu pick); cookie values
//! are never logged. Modeled on the reference cockpits' import engines.
//!
//! Layering: this module + `catalog`/`detect`/`read`/`decrypt`/`from_file`/
//! `inject` are the engine (reading, decrypting, pure helpers); the objc2
//! `WKHTTPCookieStore` write lives in `native.rs` (the webview boundary), which
//! consumes the [`ParsedCookie`]s collected here.

pub mod catalog;
pub mod decrypt;
pub mod detect;
pub mod from_file;
pub mod inject;
pub mod read;

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub use catalog::{BrowserDescriptor, BrowserFamily};

/// One source profile inside an installed browser (e.g. Chrome's "Tien").
#[derive(Debug, Clone)]
pub struct SourceProfile {
    /// Display name for the menu row.
    pub name: String,
    /// On-disk profile directory (`Default`, `Profile 1`, the Firefox dir).
    pub directory: String,
}

/// An installed, cookie-bearing source browser plus its profiles.
#[derive(Debug, Clone)]
pub struct DetectedBrowser {
    pub descriptor: &'static BrowserDescriptor,
    pub profiles: Vec<SourceProfile>,
}

/// A cookie's same-site policy (carried through; injection currently relies on
/// domain/secure/expiry, which is what logs a session in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Unspecified,
    None,
    Lax,
    Strict,
}

/// One importable cookie, normalized across source families.
#[derive(Debug, Clone)]
pub struct ParsedCookie {
    /// Cookie host (may carry a leading `.` for domain cookies).
    pub host: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub secure: bool,
    pub same_site: SameSite,
    /// Expiry in Unix seconds; `None` = a session cookie.
    pub expires_unix_s: Option<i64>,
}

/// Cookies collected from one source, ready to inject.
pub struct Collected {
    pub cookies: Vec<ParsedCookie>,
    /// Rows we couldn't import (e.g. undecryptable / expired).
    pub skipped: usize,
}

/// Why a collect failed before injection.
#[derive(Debug)]
pub enum ImportError {
    NoHome,
    UnknownBrowser,
    CookiesMissing,
    Read(String),
    FileRead(String),
    Parse(String),
    Empty,
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::NoHome => write!(f, "could not locate the home directory"),
            ImportError::UnknownBrowser => write!(f, "unknown source browser"),
            ImportError::CookiesMissing => write!(f, "no cookie database for that profile"),
            ImportError::Read(e) => write!(f, "{e}"),
            ImportError::FileRead(e) => write!(f, "could not read file: {e}"),
            ImportError::Parse(e) => write!(f, "{e}"),
            ImportError::Empty => write!(f, "no importable cookies"),
        }
    }
}

/// Every installed browser with at least one cookie-bearing profile.
pub fn detect_installed_browsers() -> Vec<DetectedBrowser> {
    dirs::home_dir().map(|h| detect::detect_installed_browsers(&h)).unwrap_or_default()
}

fn now_unix_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Read + decrypt cookies for one `(browser, profile)` source.
pub fn collect_from_source(browser_id: &str, profile_dir: &str) -> Result<Collected, ImportError> {
    let home = dirs::home_dir().ok_or(ImportError::NoHome)?;
    let desc = catalog::by_id(browser_id).ok_or(ImportError::UnknownBrowser)?;
    let db = detect::resolve_cookies_path(&home, desc, profile_dir)
        .ok_or(ImportError::CookiesMissing)?;

    let outcome = match desc.family {
        BrowserFamily::Chromium => {
            // Keychain denial isn't fatal: any plaintext rows still import; the
            // encrypted ones are counted as skipped by the reader.
            let key = decrypt::derive_key(desc.keychain_service, desc.keychain_account);
            read::read_chromium(&db, key).map_err(ImportError::Read)?
        }
        BrowserFamily::Firefox => {
            read::read_firefox(&db, now_unix_s()).map_err(ImportError::Read)?
        }
    };
    if outcome.cookies.is_empty() {
        return Err(ImportError::Empty);
    }
    Ok(Collected { cookies: outcome.cookies, skipped: outcome.skipped })
}

/// Parse cookies from a browser-extension JSON export (`From File…`).
pub fn collect_from_file(path: &Path) -> Result<Collected, ImportError> {
    let json = std::fs::read_to_string(path).map_err(|e| ImportError::FileRead(e.to_string()))?;
    let cookies = from_file::parse(&json).map_err(ImportError::Parse)?;
    Ok(Collected { cookies, skipped: 0 })
}
