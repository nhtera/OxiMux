//! Apps an agent may never drive, whatever the user approves.
//!
//! The grant model asks "may *this chat* drive *this process*". Some processes
//! are not a question of which chat: a click into a password manager can reveal
//! every credential the user owns, and a consent card cannot honestly cover it —
//! someone approving "a click" is not approving that.
//!
//! OxiMux needs this more than a focus-stealing implementation does. When input
//! is delivered by fronting the window, the user watches it happen; the whole
//! point of background delivery is that they do not have to look, which means
//! they also would not see this.
//!
//! **This list is a floor, not a survey.** It cannot enumerate every sensitive
//! app, and it is not a security boundary — an agent with a shell has other
//! routes. It removes the most damaging accident.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Bundle identifiers refused outright.
///
/// Password managers, because the blast radius of one stray click is every
/// credential the user has, and Keychain Access for the same reason.
const BLOCKED_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.1password.safari",
    "com.agilebits.onepassword7",
    "com.apple.keychainaccess",
    "com.bitwarden.desktop",
    "com.dashlane.dashlanephonefinal",
    "com.lastpass.LastPass",
    "com.nordsec.nordpass",
    "me.proton.pass.catalyst",
    "me.proton.pass.electron",
];

/// Memo of executable path → blocked, so the policy does not spawn `codesign`
/// on every click.
///
/// Safe as a process-global, unlike the grant table: this is a pure function of
/// the path with no per-chat dimension, so two chats (or two tests) reading it
/// cannot interfere. A stale entry would need a different program to appear at
/// a path already inspected this run.
static MEMO: LazyLock<Mutex<HashMap<PathBuf, bool>>> = LazyLock::new(Mutex::default);

/// Is the program at `executable` one an agent may never drive?
///
/// Unreadable identity counts as **not** blocked. That is the deliberate choice:
/// this list is a floor on top of the grant model, not the thing standing
/// between an agent and an arbitrary app — an ungranted process still has to be
/// approved. Failing closed here would instead refuse every unsigned binary,
/// including the freshly built ones this feature exists to drive.
pub fn is_blocked(executable: &Path) -> bool {
    if let Some(known) = MEMO.lock().expect("blocked memo poisoned").get(executable) {
        return *known;
    }
    let blocked = bundle_identifier(executable).is_some_and(|id| is_blocked_identifier(&id));
    MEMO.lock()
        .expect("blocked memo poisoned")
        .insert(executable.to_path_buf(), blocked);
    blocked
}

/// Case-insensitive because bundle ids are matched that way by the system, and
/// the published spellings are inconsistent (`LastPass` vs `nordpass`).
fn is_blocked_identifier(identifier: &str) -> bool {
    BLOCKED_BUNDLE_IDS
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(identifier))
}

/// The code-signing identifier of an arbitrary binary, which for an app bundle
/// is its `CFBundleIdentifier`.
///
/// Reuses the signature reader built for driver verification rather than
/// parsing `Info.plist`: that file is often a *binary* plist, which would mean a
/// new dependency, and the signed identifier is the harder one to forge of the
/// two.
fn bundle_identifier(executable: &Path) -> Option<String> {
    crate::verify::signing_identifier(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_password_managers_are_blocked() {
        for id in [
            "com.1password.1password",
            "com.bitwarden.desktop",
            "me.proton.pass.electron",
            "com.apple.keychainaccess",
        ] {
            assert!(is_blocked_identifier(id), "{id}");
        }
    }

    #[test]
    fn matching_ignores_case() {
        // Published spellings are inconsistent, and the system matches bundle
        // ids case-insensitively.
        assert!(is_blocked_identifier("com.lastpass.lastpass"));
        assert!(is_blocked_identifier("COM.NORDSEC.NORDPASS"));
    }

    #[test]
    fn ordinary_apps_are_not_blocked() {
        for id in [
            "com.apple.Safari",
            "com.microsoft.VSCode",
            "com.trycua.driver",
            "",
        ] {
            assert!(!is_blocked_identifier(id), "{id}");
        }
    }

    #[test]
    fn a_near_miss_identifier_is_not_blocked() {
        // Substring matching would catch unrelated apps; the check is equality.
        assert!(!is_blocked_identifier("com.1password.1password.helper"));
        assert!(!is_blocked_identifier("password"));
    }

    #[test]
    fn an_unsigned_or_missing_binary_is_not_blocked() {
        // Deliberate: this list is a floor on top of the grant model. Refusing
        // everything unidentifiable would refuse the freshly built, ad-hoc
        // signed binaries the whole feature exists to drive.
        assert!(!is_blocked(Path::new("/nonexistent/binary")));
    }

    #[test]
    fn a_real_signed_binary_resolves_and_is_allowed() {
        // Exercises the codesign path end to end — a parser change that broke
        // identifier reading would otherwise silently make everything "not
        // blocked" and this list would quietly stop working.
        assert!(!is_blocked(Path::new("/bin/echo")));
        assert_eq!(
            bundle_identifier(Path::new("/bin/echo")).as_deref(),
            Some("com.apple.echo"),
        );
    }

    #[test]
    fn repeat_lookups_are_memoized() {
        let path = Path::new("/bin/echo");
        assert!(!is_blocked(path));
        assert!(MEMO.lock().unwrap().contains_key(path));
        // Second call must agree; a memo that returned a different answer would
        // make the policy nondeterministic.
        assert!(!is_blocked(path));
    }
}
