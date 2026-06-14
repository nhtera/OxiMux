//! Static catalog of importable source browsers (macOS paths).
//!
//! v1 covers the Chromium family + Firefox — the realistically-importable set.
//! Safari is excluded: its cookies live in the `Cookies.binarycookies` binary
//! format, not a SQLite DB. Each Chromium entry carries the macOS Keychain
//! coordinates (`<Browser> Safe Storage`) holding the AES key that decrypts its
//! `encrypted_value` cookie column; Firefox cookies are plaintext, so it has
//! none.

/// Cookie storage layout of a source browser — decides which reader + decrypt
/// path runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFamily {
    /// Chromium-based: SQLite `cookies` table, AES-encrypted `encrypted_value`.
    Chromium,
    /// Firefox: `moz_cookies` table, plaintext values.
    Firefox,
}

/// One importable browser: where its data lives and how to unlock its cookies.
#[derive(Debug, Clone, Copy)]
pub struct BrowserDescriptor {
    /// Stable slug used in menu-action routing (e.g. `"chrome"`).
    pub id: &'static str,
    /// Human label for the menu row (e.g. `"Google Chrome"`).
    pub label: &'static str,
    pub family: BrowserFamily,
    /// Data root relative to `$HOME` (e.g.
    /// `Library/Application Support/Google/Chrome`).
    pub data_root: &'static str,
    /// Keychain generic-password service holding the AES key, empty for
    /// Firefox (`"Chrome Safe Storage"`).
    pub keychain_service: &'static str,
    /// Keychain account for that item (`"Chrome"`).
    pub keychain_account: &'static str,
}

/// The importable browsers, highest-traffic first. Opera/Vivaldi keep the
/// standard Chromium `Local State` + profile-dir layout; the cookie-path
/// resolver also probes the data root directly for the rare single-profile
/// layout (Opera).
pub const BROWSERS: &[BrowserDescriptor] = &[
    BrowserDescriptor {
        id: "chrome",
        label: "Google Chrome",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Google/Chrome",
        keychain_service: "Chrome Safe Storage",
        keychain_account: "Chrome",
    },
    BrowserDescriptor {
        id: "brave",
        label: "Brave",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/BraveSoftware/Brave-Browser",
        keychain_service: "Brave Safe Storage",
        keychain_account: "Brave",
    },
    BrowserDescriptor {
        id: "edge",
        label: "Microsoft Edge",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Microsoft Edge",
        keychain_service: "Microsoft Edge Safe Storage",
        keychain_account: "Microsoft Edge",
    },
    BrowserDescriptor {
        id: "arc",
        label: "Arc",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Arc/User Data",
        keychain_service: "Arc Safe Storage",
        keychain_account: "Arc",
    },
    BrowserDescriptor {
        id: "comet",
        label: "Comet",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Comet",
        keychain_service: "Comet Safe Storage",
        keychain_account: "Comet",
    },
    BrowserDescriptor {
        id: "vivaldi",
        label: "Vivaldi",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Vivaldi",
        keychain_service: "Vivaldi Safe Storage",
        keychain_account: "Vivaldi",
    },
    BrowserDescriptor {
        id: "opera",
        label: "Opera",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/com.operasoftware.Opera",
        keychain_service: "Opera Safe Storage",
        keychain_account: "Opera",
    },
    BrowserDescriptor {
        id: "chromium",
        label: "Chromium",
        family: BrowserFamily::Chromium,
        data_root: "Library/Application Support/Chromium",
        keychain_service: "Chromium Safe Storage",
        keychain_account: "Chromium",
    },
    BrowserDescriptor {
        id: "firefox",
        label: "Firefox",
        family: BrowserFamily::Firefox,
        data_root: "Library/Application Support/Firefox",
        keychain_service: "",
        keychain_account: "",
    },
];

/// Look up a descriptor by its stable slug (menu-action routing).
pub fn by_id(id: &str) -> Option<&'static BrowserDescriptor> {
    BROWSERS.iter().find(|b| b.id == id)
}
