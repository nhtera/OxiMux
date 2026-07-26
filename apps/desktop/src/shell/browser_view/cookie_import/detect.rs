//! Installed-browser detection + per-browser profile enumeration.
//!
//! A browser counts as "installed" only when at least one of its profiles has
//! a cookie DB on disk — app presence alone isn't enough (a never-launched
//! browser has no cookies to import). Chromium profiles come from the
//! `Local State` → `profile.info_cache` name map; Firefox profiles are the
//! directories under `…/Firefox/Profiles`.

use std::path::{Path, PathBuf};

use super::catalog::{BROWSERS, BrowserDescriptor, BrowserFamily};
use super::{DetectedBrowser, SourceProfile};

/// Reject profile-directory names that could escape the data root before we
/// join them as a path segment (defense against a poisoned `Local State`).
fn is_safe_profile_dir(dir: &str) -> bool {
    !dir.is_empty()
        && dir != "."
        && dir != ".."
        && !dir.contains('/')
        && !dir.contains('\\')
        && !dir.contains('\0')
}

/// Resolve the cookie SQLite path for one source profile, or `None` if absent.
/// Chromium moved cookies under `Network/` in v96; we probe that first, then
/// the legacy root, then (for Opera's single-profile layout) the data root.
pub fn resolve_cookies_path(
    home: &Path,
    desc: &BrowserDescriptor,
    dir: &str,
) -> Option<PathBuf> {
    let root = home.join(desc.data_root);
    match desc.family {
        BrowserFamily::Chromium => {
            let candidates = [
                root.join(dir).join("Network").join("Cookies"),
                root.join(dir).join("Cookies"),
                root.join("Network").join("Cookies"),
                root.join("Cookies"),
            ];
            candidates.into_iter().find(|p| p.exists())
        }
        BrowserFamily::Firefox => {
            let p = root.join("Profiles").join(dir).join("cookies.sqlite");
            p.exists().then_some(p)
        }
    }
}

/// Read a Chromium `Local State` and return its `(directory, display name)`
/// profile entries — falling back to a lone `Default` when the file is missing
/// or malformed (a fresh install with no named profiles still has cookies).
fn chromium_profiles(home: &Path, desc: &BrowserDescriptor) -> Vec<SourceProfile> {
    let root = home.join(desc.data_root);
    let mut out = Vec::new();
    let parsed = std::fs::read_to_string(root.join("Local State"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    if let Some(cache) = parsed
        .as_ref()
        .and_then(|v| v.get("profile"))
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
    {
        for (dir, meta) in cache {
            if !is_safe_profile_dir(dir) {
                continue;
            }
            if resolve_cookies_path(home, desc, dir).is_none() {
                continue;
            }
            let name = meta
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(dir)
                .to_string();
            out.push(SourceProfile { name, directory: dir.clone() });
        }
    }

    // Fallback: a `Default` directory with cookies but no info_cache entry.
    if out.is_empty() && resolve_cookies_path(home, desc, "Default").is_some() {
        out.push(SourceProfile { name: "Default".into(), directory: "Default".into() });
    }

    // Default profile first, then alphabetical by display name.
    out.sort_by(|a, b| {
        let rank = |p: &SourceProfile| (p.directory != "Default", p.name.to_lowercase());
        rank(a).cmp(&rank(b))
    });
    out
}

/// Enumerate Firefox profile directories that carry a `cookies.sqlite`.
/// Profile dirs are named `<random>.<label>`; we surface the label.
fn firefox_profiles(home: &Path, desc: &BrowserDescriptor) -> Vec<SourceProfile> {
    let profiles_root = home.join(desc.data_root).join("Profiles");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&profiles_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let dir = entry.file_name().to_string_lossy().to_string();
        if !is_safe_profile_dir(&dir) {
            continue;
        }
        if resolve_cookies_path(home, desc, &dir).is_none() {
            continue;
        }
        // "abc123.default-release" → "default-release".
        let name = dir.split_once('.').map(|(_, n)| n).unwrap_or(&dir).to_string();
        out.push(SourceProfile { name, directory: dir });
    }
    // default-release first, then default, then alphabetical.
    out.sort_by(|a, b| {
        let rank = |p: &SourceProfile| {
            let n = p.name.to_lowercase();
            (n != "default-release", n != "default", n)
        };
        rank(a).cmp(&rank(b))
    });
    out
}

/// Detect every catalog browser that has at least one cookie-bearing profile.
pub fn detect_installed_browsers(home: &Path) -> Vec<DetectedBrowser> {
    BROWSERS
        .iter()
        .filter_map(|desc| {
            let profiles = match desc.family {
                BrowserFamily::Chromium => chromium_profiles(home, desc),
                BrowserFamily::Firefox => firefox_profiles(home, desc),
            };
            if profiles.is_empty() {
                None
            } else {
                Some(DetectedBrowser { descriptor: desc, profiles })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_unsafe_profile_dirs() {
        assert!(is_safe_profile_dir("Default"));
        assert!(is_safe_profile_dir("Profile 1"));
        assert!(!is_safe_profile_dir(""));
        assert!(!is_safe_profile_dir(".."));
        assert!(!is_safe_profile_dir("../../etc"));
        assert!(!is_safe_profile_dir("a/b"));
    }

    #[test]
    fn chromium_profiles_from_local_state_with_cookie_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let desc = super::super::catalog::by_id("chrome").unwrap();
        let root = home.join(desc.data_root);
        // Two named profiles; only "Default" + "Profile 1" get cookie files.
        fs::create_dir_all(root.join("Default").join("Network")).unwrap();
        fs::write(root.join("Default").join("Network").join("Cookies"), b"x").unwrap();
        fs::create_dir_all(root.join("Profile 1")).unwrap();
        fs::write(root.join("Profile 1").join("Cookies"), b"x").unwrap();
        // "Profile 2" is named but has no cookie DB → excluded.
        fs::write(
            root.join("Local State"),
            r#"{"profile":{"info_cache":{
                "Default":{"name":"Tien"},
                "Profile 1":{"name":"Work"},
                "Profile 2":{"name":"Ghost"}
            }}}"#,
        )
        .unwrap();

        let profiles = chromium_profiles(home, desc);
        assert_eq!(profiles.len(), 2);
        // Default sorts first regardless of name.
        assert_eq!(profiles[0].directory, "Default");
        assert_eq!(profiles[0].name, "Tien");
        assert_eq!(profiles[1].directory, "Profile 1");
        assert_eq!(profiles[1].name, "Work");
    }

    #[test]
    fn chromium_falls_back_to_default_without_local_state() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let desc = super::super::catalog::by_id("brave").unwrap();
        let root = home.join(desc.data_root);
        fs::create_dir_all(root.join("Default")).unwrap();
        fs::write(root.join("Default").join("Cookies"), b"x").unwrap();

        let profiles = chromium_profiles(home, desc);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].directory, "Default");
        assert_eq!(profiles[0].name, "Default");
    }

    #[test]
    fn detect_skips_browsers_without_cookie_dbs() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty home → nothing detected.
        assert!(detect_installed_browsers(tmp.path()).is_empty());
    }
}
