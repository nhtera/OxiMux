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
    /// A terminal. Typing into one is running commands, and a consent card
    /// cannot name what would be run.
    Shell,
    /// An editor or IDE, which is a terminal plus the project's source.
    Editor,
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
            Blocked::Shell => {
                "a terminal, where typing runs whatever was typed — use the shell tool, which is reviewable"
            }
            Blocked::Editor => {
                "a code editor, which carries the project's source and a terminal of its own"
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
/// `host` is the OxiMux binary this check is being made on behalf of; see
/// [`is_host`] for why it has to be passed in rather than assumed.
///
/// Unreadable identity counts as **not** blocked. That is the deliberate choice:
/// this list is a floor on top of the grant model, not the thing standing
/// between an agent and an arbitrary app — an ungranted process still has to be
/// approved. Failing closed here would instead refuse every unsigned binary,
/// including the freshly built ones this feature exists to drive.
pub fn blocked_reason(executable: &Path, host: Option<&Path>) -> Option<Blocked> {
    // Deliberately ahead of the memo and never stored in it: this answer
    // depends on `host`, which the memo is not keyed on. It is also the cheap
    // half — two `canonicalize` calls against no `codesign` spawn.
    if is_host(executable, host) {
        return Some(Blocked::Host);
    }
    if let Some(known) = MEMO.lock().expect("blocked memo poisoned").get(executable) {
        return *known;
    }
    let verdict = bundle_identifier(executable)
        .as_deref()
        .and_then(classify_identifier);
    MEMO.lock()
        .expect("blocked memo poisoned")
        .insert(executable.to_path_buf(), verdict);
    verdict
}

/// Is the program at `executable` one an agent may never drive?
pub fn is_blocked(executable: &Path, host: Option<&Path>) -> bool {
    blocked_reason(executable, host).is_some()
}

/// The verdict for a bundle id alone.
///
/// OxiMux is checked here as well as by [`is_host`] because the two catch
/// different things: `is_host` catches the binary we are serving, this catches a
/// *second* copy of OxiMux the user is also running.
fn classify_identifier(identifier: &str) -> Option<Blocked> {
    if identifier.eq_ignore_ascii_case(HOST_BUNDLE_ID) {
        return Some(Blocked::Host);
    }
    if is_blocked_identifier(identifier) {
        return Some(Blocked::Credentials);
    }
    match crate::target::Category::of(identifier) {
        crate::target::Category::Terminal => Some(Blocked::Shell),
        crate::target::Category::Editor => Some(Blocked::Editor),
        // Every other category is allowed with the user's say-so. The two that
        // are not are exactly the two that are indistinguishable from handing
        // over a shell; see `Category` for why that is a different kind of
        // question from "this is risky".
        other => {
            debug_assert!(!other.is_never_driveable(), "{other:?} refused but unmapped");
            None
        }
    }
}

/// Case-insensitive because bundle ids are matched that way by the system, and
/// the published spellings are inconsistent (`LastPass` vs `nordpass`).
fn is_blocked_identifier(identifier: &str) -> bool {
    BLOCKED_BUNDLE_IDS
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(identifier))
}

/// Is `executable` the OxiMux this check is being made on behalf of?
///
/// The bundle-id entry covers the shipped app, but a developer build runs from
/// `target/debug/` with an ad-hoc signature and no identifier at all — which is
/// precisely the build being used while this feature is developed, and the one
/// where an agent clicking our own Allow button would be discovered last.
///
/// **`current_exe()` alone is not enough**, and it is worth being exact about
/// why: enforcement lives in a `PreToolUse` hook, which is a *separate binary*
/// spawned per tool call. In that process `current_exe()` is the gate, not
/// OxiMux, so the developer build above would fall through every check here and
/// be driveable. So the gate is told which binary it is protecting, and both
/// candidates are compared — `current_exe()` still covers the in-process caller
/// and the tests, where nothing is passed.
///
/// Compared canonically, so a symlinked or `..`-laden spelling of the same file
/// does not read as a different program.
fn is_host(executable: &Path, declared: Option<&Path>) -> bool {
    let Ok(other) = executable.canonicalize() else {
        // An unreadable path cannot be confirmed as us; the bundle-id check and
        // the grant model still apply to it.
        return false;
    };
    declared
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::current_exe().ok())
        .filter_map(|candidate| candidate.canonicalize().ok())
        .any(|candidate| candidate == other)
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
    fn ordinary_apps_are_not_in_the_credential_list() {
        // Note this is the credential list alone. VSCode *is* refused, on its
        // category rather than on this list, and the two reasons must stay
        // separate — "targeted a password manager" would be untrue of it.
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
    fn terminals_and_editors_are_refused_outright() {
        // Not a warning and not a tier. A keystroke into a shell prompt is a
        // command, and no consent card can say which one, because the card is
        // answered before the agent picks the text.
        assert_eq!(classify_identifier("com.apple.Terminal"), Some(Blocked::Shell));
        assert_eq!(classify_identifier("com.mitchellh.ghostty"), Some(Blocked::Shell));
        assert_eq!(classify_identifier("com.microsoft.VSCode"), Some(Blocked::Editor));
        assert_eq!(classify_identifier("com.apple.dt.Xcode"), Some(Blocked::Editor));
    }

    #[test]
    fn the_categories_that_only_warn_are_still_driveable() {
        // The split is the whole design: these are targets a user may have a
        // real reason to hand over, so they reach a card carrying the warning
        // rather than being refused before anyone is asked.
        for id in [
            "com.apple.Safari",
            "com.apple.finder",
            "com.apple.systempreferences",
        ] {
            assert_eq!(classify_identifier(id), None, "{id}");
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
        assert!(!is_blocked(Path::new("/nonexistent/binary"), None));
    }

    #[test]
    fn a_real_signed_binary_resolves_and_is_allowed() {
        // Exercises the codesign path end to end — a parser change that broke
        // identifier reading would otherwise silently make everything "not
        // blocked" and this list would quietly stop working.
        assert!(!is_blocked(Path::new("/bin/echo"), None));
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
        assert!(is_host(&me, None), "the running binary must recognise itself");
        assert_eq!(blocked_reason(&me, None), Some(Blocked::Host));
        // And a second copy of OxiMux — a different process, so `is_host` says
        // nothing about it — is refused on identity alone.
        assert_eq!(classify_identifier(HOST_BUNDLE_ID), Some(Blocked::Host));
    }

    #[test]
    fn a_declared_host_is_recognised_from_a_process_that_is_not_it() {
        // The case that matters in production and that `current_exe()` alone
        // gets wrong: the gate is its own binary, so unless it is told which
        // one OxiMux is, a development build — ad-hoc signed, no bundle id —
        // reads as an ordinary app and becomes driveable.
        let elsewhere = Path::new("/bin/echo");
        assert!(!is_blocked(elsewhere, None), "ordinarily not us");
        assert_eq!(
            blocked_reason(elsewhere, Some(elsewhere)),
            Some(Blocked::Host),
            "declared as the host, it must be refused"
        );
    }

    #[test]
    fn every_refusal_names_its_own_reason() {
        // Collapsed to a bool, a self-drive would have been reported to the
        // agent as "targeted a password manager", which is simply untrue. Each
        // of these lands in the transcript as the whole explanation the user
        // gets, so no two may read alike.
        let all = [
            Blocked::Host,
            Blocked::Credentials,
            Blocked::Shell,
            Blocked::Editor,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.reason().is_empty(), "{a:?}");
            for b in &all[i + 1..] {
                assert_ne!(a.reason(), b.reason(), "{a:?} vs {b:?}");
            }
        }
        assert!(Blocked::Host.reason().contains("OxiMux"));
        assert_eq!(
            classify_identifier("com.1password.1password"),
            Some(Blocked::Credentials)
        );
    }

    #[test]
    fn host_matching_sees_through_a_relative_spelling() {
        // Canonicalization, not string equality: `target/debug/../debug/oximux`
        // is the same program and must not read as a different one.
        let me = std::env::current_exe().expect("test binary path");
        let parent = me.parent().expect("a parent dir");
        let file = me.file_name().expect("a file name");
        let indirect = parent.join("..").join(
            parent.file_name().expect("a dir name"),
        ).join(file);
        assert!(is_host(&indirect, None), "{indirect:?}");
    }

    #[test]
    fn another_program_is_not_us() {
        assert!(!is_host(Path::new("/bin/echo"), None));
    }

    #[test]
    fn repeat_lookups_are_memoized() {
        let path = Path::new("/bin/echo");
        assert!(!is_blocked(path, None));
        assert!(MEMO.lock().unwrap().contains_key(path));
        // Second call must agree; a memo that returned a different answer would
        // make the policy nondeterministic.
        assert!(!is_blocked(path, None));
    }
}
