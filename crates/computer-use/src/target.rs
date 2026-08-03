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
//!
//! # Windows identifies apps by path, and that is weaker
//!
//! There is no `CFBundleIdentifier` on Windows. cua's own permission model says
//! the same thing and resolves it the same way: on Windows and Linux an app is
//! identified by its **canonical absolute executable path**.
//!
//! So [`Category`] is keyed on the executable's *file name* there, and the
//! honesty problem has to be stated rather than glossed. A file name is
//! attacker-controlled:
//!
//! - Renaming a binary to `1Password.exe` gets it **refused**. Harmless — the
//!   failure is in the safe direction.
//! - Renaming `cmd.exe` to `helper.exe` gets it **allowed**, dodging the
//!   `Terminal` refusal. That one is a real bypass.
//!
//! This does not make the module useless, but it does bound what it claims.
//! [`crate::blocked`] already documents itself as "a floor, not a survey" and
//! explicitly not a security boundary — an agent with a shell has other routes
//! regardless. The floor still removes the most damaging *accident*, which is
//! what it is for. What it must not do is read as a fence.
//!
//! The stronger key is the Authenticode subject, which an attacker cannot
//! forge without a matching certificate. Wiring it in belongs with the trust
//! gate (Phase 2 of `plans/260801-0157-windows-computer-use/`), because it
//! needs the same signature reader; see [`Category::of_executable`] for the
//! seam it will slot into.

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
    ///
    /// Always `None` on Windows, where no such identifier exists. It is kept in
    /// the struct rather than `cfg`-ed away so the consent card and the
    /// transcript keep one shape across platforms; the classification that
    /// actually happens there is keyed on [`Self::executable`].
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
    /// On Windows there is no bundle id to consult, so this keys on the
    /// executable instead — see the module docs for what that costs.
    pub fn category(&self) -> Category {
        #[cfg(windows)]
        {
            Category::of_executable(&self.executable)
        }
        #[cfg(not(windows))]
        {
            self.bundle_id.as_deref().map_or(Category::Other, Category::of)
        }
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

    /// The category the program at `path` falls into, keyed on its file name.
    ///
    /// The Windows counterpart to [`Self::of`]. Matched on the file name rather
    /// than the full path because install locations vary wildly — per-user vs.
    /// machine-wide, Store vs. MSI vs. portable — while the executable name is
    /// stable across all of them.
    ///
    /// # The seam for a stronger key
    ///
    /// When the trust gate lands, this should consult the Authenticode subject
    /// *first* and fall back to the file name only when the binary is unsigned:
    /// a subject cannot be forged by renaming a file, and every app in the
    /// tables below is signed by a publisher who is not the user. The fallback
    /// has to stay, because the freshly built binaries this feature exists to
    /// drive are unsigned by definition — refusing to classify them would
    /// refuse the workflow.
    #[cfg(windows)]
    pub fn of_executable(path: &Path) -> Self {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Self::Other;
        };
        WINDOWS_CATEGORIES
            .iter()
            .find(|(exe, _)| exe.eq_ignore_ascii_case(name))
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
            // The machine noun differs per platform; the rest of the sentence
            // does not. Split here rather than at the call site so there is one
            // place where this copy is written.
            #[cfg(windows)]
            Self::SystemSettings => Some(
                "Settings can change this PC's security settings, including the ones screen control depends on.",
            ),
            #[cfg(not(windows))]
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

/// Executable names that are not ordinary apps, by category.
///
/// The Windows counterpart to [`CATEGORIES`], and the same kind of floor: it
/// cannot name every terminal, and an unlisted one falls through to `Other`.
///
/// Two entries deserve their reasoning written down, because both look like
/// over-reach and neither is:
///
/// - **`conhost.exe`** hosts the console window for a classic console app.
///   Driving it is driving whatever shell it is hosting.
/// - **`explorer.exe`** is both the file manager *and* the shell process that
///   owns the desktop, taskbar, and Start menu. `FileManager` is the weaker of
///   the two readings, so it warns rather than refuses — but a click there can
///   reach considerably more than a folder.
///
/// `powershell_ise.exe` is listed as an editor rather than a terminal only
/// because the refusal reason reads better; both categories refuse outright.
#[cfg(windows)]
const WINDOWS_CATEGORIES: &[(&str, Category)] = {
    use Category::*;
    &[
        ("WindowsTerminal.exe", Terminal),
        ("OpenConsole.exe", Terminal),
        ("conhost.exe", Terminal),
        ("cmd.exe", Terminal),
        ("powershell.exe", Terminal),
        ("pwsh.exe", Terminal),
        ("wt.exe", Terminal),
        ("bash.exe", Terminal),
        ("wsl.exe", Terminal),
        ("ubuntu.exe", Terminal),
        ("mintty.exe", Terminal),
        ("putty.exe", Terminal),
        ("alacritty.exe", Terminal),
        ("wezterm-gui.exe", Terminal),
        ("Hyper.exe", Terminal),
        ("Code.exe", Editor),
        ("Code - Insiders.exe", Editor),
        ("VSCodium.exe", Editor),
        ("Cursor.exe", Editor),
        ("Zed.exe", Editor),
        ("devenv.exe", Editor),
        ("idea64.exe", Editor),
        ("rider64.exe", Editor),
        ("clion64.exe", Editor),
        ("pycharm64.exe", Editor),
        ("webstorm64.exe", Editor),
        ("rustrover64.exe", Editor),
        ("sublime_text.exe", Editor),
        ("notepad++.exe", Editor),
        ("powershell_ise.exe", Editor),
        ("msedge.exe", Browser),
        ("chrome.exe", Browser),
        ("firefox.exe", Browser),
        ("brave.exe", Browser),
        ("opera.exe", Browser),
        ("vivaldi.exe", Browser),
        ("arc.exe", Browser),
        ("iexplore.exe", Browser),
        ("explorer.exe", FileManager),
        ("SystemSettings.exe", SystemSettings),
        ("control.exe", SystemSettings),
        ("mmc.exe", SystemSettings),
        ("secpol.msc", SystemSettings),
    ]
};

/// A human name for an executable path.
///
/// Prefers the enclosing `.app` bundle's name, because that is what the user
/// sees in the Dock — `/Applications/Safari.app/Contents/MacOS/Safari` is
/// "Safari", and so is a bundle whose inner binary was renamed. Falls back to
/// the file name for a bare executable, which is the normal shape of the fresh
/// builds this feature exists to drive.
/// On Windows there is no bundle to look up to, so the file name is all there
/// is — minus the `.exe`, which Explorer hides and which reads as noise on a
/// consent card ("Let the agent control chrome.exe"). The extension is dropped
/// only for display; every comparison elsewhere keeps it.
pub(crate) fn display_name(executable: &Path) -> String {
    #[cfg(not(windows))]
    if let Some(app) = executable.components().rev().find_map(|component| {
        let name = component.as_os_str().to_str()?;
        name.strip_suffix(".app")
    }) && !app.is_empty()
    {
        return app.to_string();
    }

    let Some(name) = executable.file_name().and_then(|name| name.to_str()) else {
        return "an unknown program".to_string();
    };

    #[cfg(windows)]
    if let Some(stem) = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE"))
        && !stem.is_empty()
    {
        return stem.to_string();
    }

    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `.app` bundles are a macOS shape; the Windows naming rule is covered by
    // `windows_identity::a_display_name_drops_the_extension_but_a_comparison_does_not`.
    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
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

    #[cfg(windows)]
    mod windows_identity {
        use super::*;

        #[test]
        fn each_named_category_is_recognised_by_executable() {
            for (exe, expected) in [
                (r"C:\Program Files\WindowsApps\WindowsTerminal.exe", Category::Terminal),
                (r"C:\Windows\System32\cmd.exe", Category::Terminal),
                (r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\Code.exe", Category::Editor),
                (r"C:\Program Files\Google\Chrome\Application\chrome.exe", Category::Browser),
                (r"C:\Windows\explorer.exe", Category::FileManager),
                (r"C:\Windows\ImmersiveControlPanel\SystemSettings.exe", Category::SystemSettings),
            ] {
                assert_eq!(Category::of_executable(Path::new(exe)), expected, "{exe}");
            }
        }

        #[test]
        fn the_install_location_does_not_change_the_verdict() {
            // Windows apps land wherever the installer chose — per-user,
            // machine-wide, Store, or portable off a USB stick. Keying on the
            // directory would refuse a terminal in one location and allow the
            // same binary in another.
            for path in [
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                r"C:\Users\u\Downloads\portable\pwsh.exe",
                r"D:\tools\pwsh.exe",
            ] {
                assert_eq!(Category::of_executable(Path::new(path)), Category::Terminal, "{path}");
            }
        }

        #[test]
        fn executable_matching_ignores_case() {
            // Windows paths are case-insensitive, so `CMD.EXE` and `cmd.exe`
            // are the same file. Matching case-sensitively would let a
            // differently-cased spelling walk past the refusal.
            assert_eq!(Category::of_executable(Path::new(r"C:\W\CMD.EXE")), Category::Terminal);
            assert_eq!(Category::of_executable(Path::new(r"C:\W\code.EXE")), Category::Editor);
        }

        #[test]
        fn an_unrecognised_executable_is_other() {
            // Must stay the default: an agent's own fresh build is exactly this
            // case, and it is what the feature exists to drive.
            for path in [r"C:\repo\target\debug\my-app.exe", r"C:\x\notepad.exe", ""] {
                assert_eq!(Category::of_executable(Path::new(path)), Category::Other, "{path}");
            }
        }

        #[test]
        fn a_near_miss_name_lands_in_other() {
            // Equality on the whole file name, not a prefix or a substring. A
            // helper shipped beside an editor is not the editor.
            for path in [r"C:\x\Code Helper.exe", r"C:\x\cmd-wrapper.exe", r"C:\x\chrome_proxy.exe"] {
                assert_eq!(Category::of_executable(Path::new(path)), Category::Other, "{path}");
            }
        }

        #[test]
        fn a_display_name_drops_the_extension_but_a_comparison_does_not() {
            // The card says "chrome"; the table still matches "chrome.exe".
            let path = Path::new(r"C:\Program Files\Google\Chrome\Application\chrome.exe");
            assert_eq!(display_name(path), "chrome");
            assert_eq!(Category::of_executable(path), Category::Browser);

            // A name that is nothing but the extension keeps it, rather than
            // rendering as an empty consent card.
            assert_eq!(display_name(Path::new(r"C:\x\.exe")), ".exe");
        }

        #[test]
        fn a_renamed_terminal_escapes_the_refusal() {
            // Not an aspiration — a statement of the known limit, pinned so it
            // cannot be quietly assumed away. Path identity is what Windows
            // offers and it is forgeable by anyone who can rename a file.
            //
            // If this test ever fails, the classifier gained a stronger key
            // (an Authenticode subject) and the module docs saying "a floor,
            // not a fence" should be revisited to match.
            assert_eq!(
                Category::of_executable(Path::new(r"C:\x\totally-not-a-shell.exe")),
                Category::Other,
                "renaming cmd.exe defeats a file-name classifier"
            );
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
