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

use crate::HOST_BUNDLE_ID;

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

/// Why a target is off-limits. Carried rather than collapsed to a bool so the
/// refusal the agent reads says something true — "targeted a password manager"
/// is actively misleading when the target was OxiMux itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// OxiMux. An agent driving us can answer its own consent cards.
    Host,
    /// A password manager or the keychain.
    Credentials,
}

impl Blocked {
    /// The noun phrase the refusal message is built from. Reads as
    /// "`click` targeted {this}. Agents are never allowed to drive it."
    pub fn reason(self) -> &'static str {
        match self {
            Blocked::Host => "OxiMux itself, which would let an agent approve its own actions",
            Blocked::Credentials => {
                "a password manager or keychain, where one click can expose every credential you have"
            }
        }
    }
}

/// Memo of executable path → verdict, so the policy does not spawn `codesign`
/// on every click.
///
/// Safe as a process-global, unlike the grant table: this is a pure function of
/// the path with no per-chat dimension, so two chats (or two tests) reading it
/// cannot interfere. A stale entry would need a different program to appear at
/// a path already inspected this run.
static MEMO: LazyLock<Mutex<HashMap<PathBuf, Option<Blocked>>>> = LazyLock::new(Mutex::default);

/// Why the program at `executable` may never be driven, or `None` if it may.
///
/// Unreadable identity counts as **not** blocked. That is the deliberate choice:
/// this list is a floor on top of the grant model, not the thing standing
/// between an agent and an arbitrary app — an ungranted process still has to be
/// approved. Failing closed here would instead refuse every unsigned binary,
/// including the freshly built ones this feature exists to drive.
pub fn blocked_reason(executable: &Path) -> Option<Blocked> {
    if let Some(known) = MEMO.lock().expect("blocked memo poisoned").get(executable) {
        return *known;
    }
    let verdict = classify(executable);
    MEMO.lock()
        .expect("blocked memo poisoned")
        .insert(executable.to_path_buf(), verdict);
    verdict
}

/// Is the program at `executable` one an agent may never drive?
pub fn is_blocked(executable: &Path) -> bool {
    blocked_reason(executable).is_some()
}

fn classify(executable: &Path) -> Option<Blocked> {
    if is_self(executable) {
        return Some(Blocked::Host);
    }
    classify_identifier(&bundle_identifier(executable)?)
}

/// The verdict for a bundle id alone.
///
/// OxiMux is checked here as well as by [`is_self`] because the two catch
/// different things: `is_self` catches the process we are, this catches a
/// *second* copy of OxiMux the user is also running.
fn classify_identifier(identifier: &str) -> Option<Blocked> {
    if identifier.eq_ignore_ascii_case(HOST_BUNDLE_ID) {
        return Some(Blocked::Host);
    }
    is_blocked_identifier(identifier).then_some(Blocked::Credentials)
}

/// Case-insensitive because bundle ids are matched that way by the system, and
/// the published spellings are inconsistent (`LastPass` vs `nordpass`).
fn is_blocked_identifier(identifier: &str) -> bool {
    BLOCKED_BUNDLE_IDS
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(identifier))
}

/// Is `executable` the program we are running inside?
///
/// The bundle-id entry covers the shipped app, but a developer build runs from
/// `target/debug/` with an ad-hoc signature and no identifier at all — which is
/// precisely the build being used while this feature is developed, and the one
/// where an agent clicking our own Allow button would be discovered last.
///
/// Compared canonically, so a symlinked or `..`-laden spelling of the same file
/// does not read as a different program.
fn is_self(executable: &Path) -> bool {
    let Ok(me) = std::env::current_exe() else {
        return false;
    };
    match (me.canonicalize(), executable.canonicalize()) {
        (Ok(me), Ok(other)) => me == other,
        // An unreadable path cannot be confirmed as us; the bundle-id check and
        // the grant model still apply to it.
        _ => false,
    }
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
    fn oximux_may_not_be_driven_by_its_own_agents() {
        // The consent model rests on the user answering the card. An agent that
        // can click our window answers it for them.
        let me = std::env::current_exe().expect("test binary path");
        assert!(is_self(&me), "the running binary must recognise itself");
        assert_eq!(blocked_reason(&me), Some(Blocked::Host));
        // And a second copy of OxiMux — a different process, so `is_self` says
        // nothing about it — is refused on identity alone.
        assert_eq!(classify_identifier(HOST_BUNDLE_ID), Some(Blocked::Host));
    }

    #[test]
    fn the_refusal_names_the_right_reason() {
        // Collapsed to a bool, a self-drive would have been reported to the
        // agent as "targeted a password manager", which is simply untrue.
        assert_ne!(Blocked::Host.reason(), Blocked::Credentials.reason());
        assert!(Blocked::Host.reason().contains("OxiMux"));
        assert_eq!(
            classify_identifier("com.1password.1password"),
            Some(Blocked::Credentials)
        );
        assert_eq!(classify_identifier("com.apple.Safari"), None);
    }

    #[test]
    fn self_matching_sees_through_a_relative_spelling() {
        // Canonicalization, not string equality: `target/debug/../debug/oximux`
        // is the same program and must not read as a different one.
        let me = std::env::current_exe().expect("test binary path");
        let parent = me.parent().expect("a parent dir");
        let file = me.file_name().expect("a file name");
        let indirect = parent.join("..").join(
            parent.file_name().expect("a dir name"),
        ).join(file);
        assert!(is_self(&indirect), "{indirect:?}");
    }

    #[test]
    fn another_program_is_not_us() {
        assert!(!is_self(Path::new("/bin/echo")));
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
