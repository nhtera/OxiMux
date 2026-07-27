//! Who a screen-control call is actually aimed at.
//!
//! The policy decides in terms of pids, which is right for enforcement and
//! useless for consent: "allow a click into process 4821?" is not a question a
//! person can answer. This turns a pid into something nameable — an app name, a
//! bundle id, and a note when the target is the kind of app where one click is
//! worth much more than one click.
//!
//! Resolution costs a `codesign` spawn, so it happens on the ask path only,
//! which is by definition a human-speed moment. The [`crate::blocked`] memo
//! makes a repeat lookup of the same path free.

use std::path::{Path, PathBuf};

/// A resolved target, in the terms a consent card needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetApp {
    pub pid: u32,
    pub executable: PathBuf,
    /// `CFBundleIdentifier` when the target is signed. `None` for the ad-hoc
    /// signed binaries this feature mostly drives — an agent's own fresh build
    /// has no stable identity, which is exactly why those are granted by build
    /// provenance rather than by a persisted allowlist.
    pub bundle_id: Option<String>,
    /// What to call it on screen.
    pub name: String,
}

impl TargetApp {
    /// Resolve a pid, or `None` when it names no live process.
    pub fn describe(pid: u32) -> Option<Self> {
        let executable = crate::proc::executable_of_pid(pid)?;
        Some(Self {
            pid,
            bundle_id: crate::verify::signing_identifier(&executable),
            name: display_name(&executable),
            executable,
        })
    }

    /// The extra warning this target's category earns, if any.
    pub fn sentinel(&self) -> Option<Sentinel> {
        self.bundle_id.as_deref().and_then(Sentinel::for_bundle_id)
    }
}

/// Just the on-screen name for a pid, without resolving signing identity.
///
/// [`TargetApp::describe`] spawns `codesign`, which is fine on the ask path —
/// a human is already reading — and wrong for anything that repeats. The
/// indicator re-reads every live target on a timer, so it takes this path: a
/// `proc_pidpath` call and some string work, no process spawn.
pub fn name_of_pid(pid: u32) -> Option<String> {
    crate::proc::executable_of_pid(pid).map(|path| display_name(&path))
}

/// A category where approving one action approves considerably more than it
/// looks like.
///
/// Deliberately *warnings*, not refusals. The blocklist in [`crate::blocked`]
/// is where "never, whatever the user says" lives; this is for targets a user
/// may have a real reason to drive — an agent running its own test suite in a
/// terminal, say — as long as they understand what they are handing over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentinel {
    /// A terminal or shell host: a click plus typing is arbitrary code.
    Shell,
    /// An editor or IDE, which has an integrated terminal and the user's source.
    Editor,
    /// System Settings, where a click can change the machine's security posture
    /// — including the very permissions this feature is gated on.
    SystemSettings,
    /// A file manager: move and delete reach the whole disk.
    FileManager,
}

impl Sentinel {
    /// The line shown on the consent card. Written to say what the *approval*
    /// enables, not what the app is — the user can see the app name already.
    pub fn warning(self) -> &'static str {
        match self {
            Sentinel::Shell => {
                "Typing into a terminal runs commands. This is equivalent to giving the agent a shell."
            }
            Sentinel::Editor => {
                "Editors carry the project's source and an built-in terminal. Approving this is close to full access."
            }
            Sentinel::SystemSettings => {
                "System Settings can change this Mac's security settings, including the permissions screen control depends on."
            }
            Sentinel::FileManager => {
                "A file manager can move and delete anything the user can."
            }
        }
    }

    fn for_bundle_id(bundle_id: &str) -> Option<Self> {
        SENTINELS
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(bundle_id))
            .map(|(_, sentinel)| *sentinel)
    }
}

/// Bundle ids that earn a warning, by category.
///
/// Like the blocklist, a floor rather than a survey: it cannot name every
/// terminal or editor, and an unlisted one simply gets the ordinary card. What
/// it does is make the common cases legible instead of silent.
const SENTINELS: &[(&str, Sentinel)] = {
    use Sentinel::*;
    &[
        ("com.apple.Terminal", Shell),
        ("com.googlecode.iterm2", Shell),
        ("dev.warp.Warp-Stable", Shell),
        ("co.zeit.hyper", Shell),
        ("net.kovidgoyal.kitty", Shell),
        ("com.github.wez.wezterm", Shell),
        ("org.alacritty", Shell),
        ("com.mitchellh.ghostty", Shell),
        ("com.microsoft.VSCode", Editor),
        ("com.microsoft.VSCodeInsiders", Editor),
        ("com.visualstudio.code.oss", Editor),
        ("com.todesktop.230313mzl4w4u92", Editor),
        ("dev.zed.Zed", Editor),
        ("com.apple.dt.Xcode", Editor),
        ("com.jetbrains.intellij", Editor),
        ("com.jetbrains.CLion", Editor),
        ("com.jetbrains.pycharm", Editor),
        ("com.jetbrains.WebStorm", Editor),
        ("com.jetbrains.rustrover", Editor),
        ("com.sublimetext.4", Editor),
        ("com.apple.systempreferences", SystemSettings),
        ("com.apple.finder", FileManager),
    ]
};

/// A human name for an executable path.
///
/// Prefers the enclosing `.app` bundle's name, because that is what the user
/// sees in the Dock — `/Applications/Safari.app/Contents/MacOS/Safari` is
/// "Safari", and so is a bundle whose inner binary was renamed. Falls back to
/// the file name for a bare executable, which is the normal shape of the fresh
/// builds this feature exists to drive.
fn display_name(executable: &Path) -> String {
    if let Some(app) = executable.components().rev().find_map(|component| {
        let name = component.as_os_str().to_str()?;
        name.strip_suffix(".app")
    }) && !app.is_empty()
    {
        return app.to_string();
    }
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("an unknown program")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundled_app_is_named_by_its_bundle() {
        // What the user sees in the Dock, not the inner binary's file name.
        assert_eq!(
            display_name(Path::new("/Applications/Safari.app/Contents/MacOS/Safari")),
            "Safari"
        );
        assert_eq!(
            display_name(Path::new("/Applications/Visual Studio Code.app/Contents/MacOS/Electron")),
            "Visual Studio Code"
        );
    }

    #[test]
    fn a_nested_bundle_is_named_by_the_innermost_one() {
        // Helpers live inside their host's bundle; the inner one is the process
        // actually being driven, so naming the outer app would misattribute it.
        assert_eq!(
            display_name(Path::new(
                "/Applications/Foo.app/Contents/Helpers/Bar.app/Contents/MacOS/Bar"
            )),
            "Bar"
        );
    }

    #[test]
    fn a_bare_executable_is_named_by_its_file() {
        // The normal shape of an agent's own fresh build.
        assert_eq!(
            display_name(Path::new("/repo/target/debug/my-app")),
            "my-app"
        );
    }

    #[test]
    fn an_unnameable_path_still_yields_copy() {
        // The card must always have something to render; an empty name would
        // produce "Let the agent control ?".
        assert_eq!(display_name(Path::new("/")), "an unknown program");
    }

    #[test]
    fn terminals_and_editors_earn_their_warnings() {
        assert_eq!(Sentinel::for_bundle_id("com.apple.Terminal"), Some(Sentinel::Shell));
        assert_eq!(
            Sentinel::for_bundle_id("com.microsoft.VSCode"),
            Some(Sentinel::Editor)
        );
        assert_eq!(
            Sentinel::for_bundle_id("com.apple.systempreferences"),
            Some(Sentinel::SystemSettings)
        );
        assert_eq!(
            Sentinel::for_bundle_id("com.apple.finder"),
            Some(Sentinel::FileManager)
        );
    }

    #[test]
    fn sentinel_matching_ignores_case_like_the_blocklist() {
        // Must agree with `blocked`, which compares the same way — the two
        // disagreeing would mean a warning that shows for one spelling only.
        assert_eq!(
            Sentinel::for_bundle_id("COM.APPLE.TERMINAL"),
            Some(Sentinel::Shell)
        );
    }

    #[test]
    fn an_ordinary_app_earns_no_warning() {
        // Over-warning is its own failure: a card that always warns is a card
        // nobody reads.
        for id in ["com.apple.Safari", "com.figma.Desktop", ""] {
            assert_eq!(Sentinel::for_bundle_id(id), None, "{id}");
        }
    }

    #[test]
    fn every_warning_is_written_and_distinct() {
        let all = [
            Sentinel::Shell,
            Sentinel::Editor,
            Sentinel::SystemSettings,
            Sentinel::FileManager,
        ];
        for sentinel in all {
            assert!(!sentinel.warning().is_empty(), "{sentinel:?}");
        }
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.warning(), b.warning(), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn a_dead_pid_describes_as_nothing() {
        assert_eq!(TargetApp::describe(u32::MAX), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn our_own_process_resolves_end_to_end() {
        // Exercises pid → executable → identity, which is the whole path the
        // consent card depends on.
        let me = TargetApp::describe(std::process::id()).expect("self must resolve");
        assert!(me.executable.is_absolute());
        assert!(!me.name.is_empty());
    }
}
