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

    /// What kind of app this is, in the terms the policy reasons about.
    ///
    /// A target with no readable identity — the ad-hoc signed binaries this
    /// feature mostly drives — is [`Category::Other`], the ordinary path. It has
    /// to be: a fresh build has no bundle id, so treating "no id" as anything
    /// else would either refuse the workflow this exists for or warn on every
    /// card until nobody reads them.
    pub fn category(&self) -> Category {
        self.bundle_id.as_deref().map_or(Category::Other, Category::of)
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

/// What kind of app a bundle id names.
///
/// Total rather than `Option`: every id resolves to something, and an
/// unrecognised one resolves to [`Category::Other`]. That is most of the point
/// of the type — a classifier returning `Option` invites `if let Some(..)` at
/// each call site, which silently skips the unknown case, and the unknown case
/// is the common one.
///
/// Two categories are **refusals** and three are **warnings**, and the split is
/// not a matter of degree:
///
/// - A terminal or an editor is not a target with a large blast radius, it is
///   arbitrary code. A keystroke at a shell prompt runs whatever was typed, and
///   an editor carries the project's source plus a built-in terminal of its
///   own. No consent card can put that honestly: someone approving "a click" is
///   not approving that, which is the same reason [`crate::blocked`] exists.
///   Nothing legitimate is lost, either — an agent that needs to run a command
///   has a shell tool for it, and that one is reviewable.
/// - A browser, a file manager, or System Settings are targets a user may have
///   a real reason to hand over, as long as they are told what comes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A terminal or shell host: a click plus typing is arbitrary code.
    Terminal,
    /// An editor or IDE, which has an integrated terminal and the project's
    /// source.
    Editor,
    /// A browser, holding every site the user is signed in to.
    Browser,
    /// A file manager: move and delete reach the whole disk.
    FileManager,
    /// System Settings, where a click can change the machine's security posture
    /// — including the very permissions this feature is gated on.
    SystemSettings,
    /// Everything else, which is every id not in the table below.
    Other,
}

impl Category {
    /// The category `bundle_id` falls into.
    ///
    /// Matched case-insensitively and by equality, exactly like the blocklist.
    /// Substring matching would sweep unrelated apps in on a shared prefix, and
    /// the two lists disagreeing would mean an app refused by one rule and
    /// merely warned about by the other.
    pub fn of(bundle_id: &str) -> Self {
        CATEGORIES
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(bundle_id))
            .map_or(Self::Other, |(_, category)| *category)
    }

    /// Whether an agent may never address this category at all, whatever the
    /// user approves.
    ///
    /// Read by [`crate::blocked`], which is where the refusal is actually
    /// enforced — this only says which categories earn it.
    pub fn is_never_driveable(self) -> bool {
        matches!(self, Self::Terminal | Self::Editor)
    }

    /// The line shown on the consent card. Written to say what the *approval*
    /// enables, not what the app is — the user can see the app name already.
    ///
    /// `None` for the two refused categories, which never reach a card, and for
    /// `Other`, because a card that always warns is a card nobody reads.
    pub fn warning(self) -> Option<&'static str> {
        match self {
            Self::Browser => Some(
                "A browser is signed in to everything you are — mail, bank, source control.",
            ),
            Self::FileManager => {
                Some("A file manager can move and delete anything you can.")
            }
            Self::SystemSettings => Some(
                "System Settings can change this Mac's security settings, including the permissions screen control depends on.",
            ),
            Self::Terminal | Self::Editor | Self::Other => None,
        }
    }
}

/// Bundle ids that are not ordinary apps, by category.
///
/// Like the blocklist, a floor rather than a survey: it cannot name every
/// terminal or editor, and an unlisted one falls through to `Other` and gets
/// the ordinary card. What it does is make the common cases legible instead of
/// silent. A missed terminal is a real bug, not a nicety.
const CATEGORIES: &[(&str, Category)] = {
    use Category::*;
    &[
        ("com.apple.Terminal", Terminal),
        ("com.googlecode.iterm2", Terminal),
        ("dev.warp.Warp-Stable", Terminal),
        ("co.zeit.hyper", Terminal),
        ("net.kovidgoyal.kitty", Terminal),
        ("com.github.wez.wezterm", Terminal),
        ("org.alacritty", Terminal),
        ("com.mitchellh.ghostty", Terminal),
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
        ("com.apple.Safari", Browser),
        ("com.google.Chrome", Browser),
        ("com.google.Chrome.canary", Browser),
        ("com.microsoft.edgemac", Browser),
        ("org.mozilla.firefox", Browser),
        ("com.brave.Browser", Browser),
        ("company.thebrowser.Browser", Browser),
        ("com.operasoftware.Opera", Browser),
        ("com.vivaldi.Vivaldi", Browser),
        ("com.apple.finder", FileManager),
        ("com.apple.systempreferences", SystemSettings),
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
    fn each_named_category_is_recognised() {
        for (id, expected) in [
            ("com.apple.Terminal", Category::Terminal),
            ("com.microsoft.VSCode", Category::Editor),
            ("com.apple.Safari", Category::Browser),
            ("com.apple.finder", Category::FileManager),
            ("com.apple.systempreferences", Category::SystemSettings),
        ] {
            assert_eq!(Category::of(id), expected, "{id}");
        }
    }

    #[test]
    fn category_matching_ignores_case_like_the_blocklist() {
        // Must agree with `blocked`, which compares the same way — the two
        // disagreeing would mean an app refused under one spelling only.
        assert_eq!(Category::of("COM.APPLE.TERMINAL"), Category::Terminal);
    }

    #[test]
    fn an_unrecognised_id_is_other_rather_than_a_named_category() {
        // The default has to be the ordinary path: an agent's own fresh build
        // is ad-hoc signed with no id at all, and it is the target the whole
        // feature exists to drive.
        for id in ["com.figma.Desktop", "com.example.whatever", ""] {
            assert_eq!(Category::of(id), Category::Other, "{id}");
        }
        assert!(!Category::Other.is_never_driveable());
        assert_eq!(Category::Other.warning(), None);
    }

    #[test]
    fn a_near_miss_id_lands_in_other_rather_than_the_category_it_resembles() {
        // Equality, not prefix. A helper process shipped inside an editor is
        // not the editor, and matching loosely would refuse it on a coincidence.
        assert_eq!(Category::of("com.apple.Terminal.helper"), Category::Other);
        assert_eq!(Category::of("com.microsoft.VSCod"), Category::Other);
    }

    #[test]
    fn refusals_and_warnings_are_disjoint() {
        // A category that both refuses and warns would mean copy written for a
        // card the user is never shown — dead text that reads as live policy.
        for category in ALL {
            assert!(
                !(category.is_never_driveable() && category.warning().is_some()),
                "{category:?} is refused and yet carries card copy"
            );
        }
        assert!(Category::Terminal.is_never_driveable());
        assert!(Category::Editor.is_never_driveable());
        assert!(!Category::Browser.is_never_driveable());
    }

    #[test]
    fn every_warning_is_written_and_distinct() {
        let warnings: Vec<&str> = ALL.iter().filter_map(|c| c.warning()).collect();
        assert_eq!(warnings.len(), 3, "one per allowed-but-costly category");
        for warning in &warnings {
            assert!(!warning.is_empty());
        }
        for (i, a) in warnings.iter().enumerate() {
            for b in &warnings[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// Every category, so the property tests above cannot silently skip one a
    /// later commit adds.
    const ALL: [Category; 6] = [
        Category::Terminal,
        Category::Editor,
        Category::Browser,
        Category::FileManager,
        Category::SystemSettings,
        Category::Other,
    ];

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
