//! `Open in ▸` — hand a worktree directory to an external application.
//!
//! The list the submenu offers comes from `git.toml` (`[[open_in]]`), and when
//! that is empty from a built-in list resolved here at menu-open time: the
//! platform's file manager, plus whichever known editors are installed. Nothing
//! is cached, so an editor installed after the app started shows up on the
//! next right-click.
//!
//! **Binary resolution is the process environment's job, not this module's.**
//! A Dock-launched app inherits launchd's four-directory `PATH`, under which
//! `code` and `zed` do not exist — and `platform::login_path` repairs that
//! once, at the top of `main`, for every `Command::new` in the process. This
//! module spawns by name and lets that repair do its work; a second resolver
//! here would be a second place for the rule to drift.

use std::path::Path;

use oximux_settings::OpenInApp;
use oximux_settings::git::GitSettings;

/// The apps the submenu should offer: the configured list, or the built-in
/// one when nothing usable is configured.
///
/// `git.toml` is meant to be hand-edited and `[[open_in]]` loads a missing
/// key as an empty string, so a configured entry can have no name (a blank
/// menu row) or no launchable command (a row that always fails). Those are
/// dropped here rather than offered; a list that is nothing but those falls
/// back to the built-in one, the same as an empty list.
pub fn effective_apps(settings: &GitSettings) -> Vec<OpenInApp> {
    let usable: Vec<OpenInApp> = settings
        .open_in
        .iter()
        .filter(|app| !app.name.trim().is_empty() && parse_command(&app.command).is_some())
        .cloned()
        .collect();
    if usable.is_empty() { default_apps() } else { usable }
}

/// The built-in list: the platform opener first, then the editors we can see.
///
/// On macOS an editor is "installed" when its bundle sits in `/Applications`
/// or `~/Applications`, and it is launched through `open -a`, which needs no
/// shell integration. Elsewhere an editor is offered when its command is on
/// the (repaired) `PATH`.
pub fn default_apps() -> Vec<OpenInApp> {
    let mut apps = vec![platform_opener()];
    apps.extend(installed_editors());
    apps
}

/// The platform's file manager, always offered — a worktree the user cannot
/// find on disk is the defect Phase 6 exists to close.
fn platform_opener() -> OpenInApp {
    #[cfg(target_os = "macos")]
    {
        OpenInApp { name: "Finder".into(), command: "open".into() }
    }
    #[cfg(windows)]
    {
        OpenInApp { name: "Explorer".into(), command: "explorer".into() }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        OpenInApp { name: "File manager".into(), command: "xdg-open".into() }
    }
}

/// Editors worth checking for, in menu order. Mirrors the editor header's
/// own list so a file and a worktree open in the same set of apps.
#[cfg(target_os = "macos")]
const MAC_EDITORS: &[(&str, &str)] = &[
    ("Cursor", "Cursor"),
    ("VS Code", "Visual Studio Code"),
    ("Windsurf", "Windsurf"),
    ("Zed", "Zed"),
];

#[cfg(target_os = "macos")]
fn mac_editor(name: &str, bundle: &str) -> OpenInApp {
    OpenInApp { name: name.to_string(), command: format!("open -a \"{bundle}\"") }
}

#[cfg(target_os = "macos")]
fn installed_editors() -> Vec<OpenInApp> {
    let mut roots = vec![std::path::PathBuf::from("/Applications")];
    roots.extend(dirs::home_dir().map(|h| h.join("Applications")));
    MAC_EDITORS
        .iter()
        .filter(|(_, bundle)| roots.iter().any(|r| r.join(format!("{bundle}.app")).exists()))
        .map(|(name, bundle)| mac_editor(name, bundle))
        .collect()
}

/// Every editor this module knows how to launch, installed or not — the
/// settings pane's preset picker, so adding one is a click rather than
/// knowing that VS Code is `open -a "Visual Studio Code"`. The command is
/// the platform's shape for that editor; whether it can actually start is
/// for the click to find out, which is the same contract a typed command has.
#[cfg(target_os = "macos")]
pub fn presets() -> Vec<OpenInApp> {
    MAC_EDITORS.iter().map(|(name, bundle)| mac_editor(name, bundle)).collect()
}

/// Editors reached by their CLI name off-macOS.
#[cfg(not(target_os = "macos"))]
const CLI_EDITORS: &[(&str, &str)] =
    &[("Cursor", "cursor"), ("VS Code", "code"), ("Windsurf", "windsurf"), ("Zed", "zed")];

/// The stored command is the path `which` found, not the bare name: on
/// Windows `which` honours `PATHEXT` and matches `code.cmd`, which
/// `std::process::Command` — resolving by appending `.exe` only — would then
/// fail to start. Quoted, since an install path routinely contains spaces.
/// See the macOS `presets`. Off-macOS the preset is the bare CLI name, which
/// `launch` resolves through the repaired `PATH` like any typed command.
#[cfg(not(target_os = "macos"))]
pub fn presets() -> Vec<OpenInApp> {
    CLI_EDITORS
        .iter()
        .map(|(name, bin)| OpenInApp { name: (*name).to_string(), command: (*bin).to_string() })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn installed_editors() -> Vec<OpenInApp> {
    CLI_EDITORS
        .iter()
        .filter_map(|(name, bin)| {
            let path = which::which(bin).ok()?;
            Some(OpenInApp {
                name: (*name).to_string(),
                command: format!("\"{}\"", path.display()),
            })
        })
        .collect()
}

/// Split a configured command into program + fixed arguments.
///
/// Whitespace-separated, with double quotes grouping an argument that
/// contains spaces (`open -a "Visual Studio Code"`). No shell, so no
/// expansion, no escapes beyond the quote pair. `None` when the command is
/// blank, when a quote is left open, or when the program itself is empty
/// (`""` or `" "` — a quoted nothing) — each is a setting the pane should
/// have refused, and none can be launched.
pub fn parse_command(command: &str) -> Option<(String, Vec<String>)> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quoted_word = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                // An empty `""` is still a word; remember we saw quotes.
                quoted_word = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() || quoted_word {
                    words.push(std::mem::take(&mut current));
                    quoted_word = false;
                }
            }
            c => current.push(c),
        }
    }
    if in_quotes {
        return None;
    }
    if !current.is_empty() || quoted_word {
        words.push(current);
    }
    let mut it = words.into_iter();
    let program = it.next()?;
    if program.trim().is_empty() {
        return None;
    }
    Some((program, it.collect()))
}

/// Launch `app` on `dir`. The directory is always the final argument.
///
/// Spawned, not waited on: the app is a long-lived editor or a file manager,
/// and the row menu has already closed. The only failure surfaced is the
/// spawn itself — an app that starts and then declines the directory is its
/// own problem to report.
pub fn launch(app: &OpenInApp, dir: &Path) -> Result<(), String> {
    let (program, args) = parse_command(&app.command)
        .ok_or_else(|| format!("\u{201c}{}\u{201d} has no launch command", app.name))?;
    std::process::Command::new(&program)
        .args(&args)
        .arg(dir)
        .spawn()
        .map(drop)
        .map_err(|err| format!("could not start {program}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, command: &str) -> OpenInApp {
        OpenInApp { name: name.into(), command: command.into() }
    }

    /// The whole point of the empty-means-built-in rule: a fresh install has
    /// an `Open in` submenu without anyone visiting the settings pane.
    #[test]
    fn an_empty_configured_list_falls_back_to_the_built_in_one() {
        let apps = effective_apps(&GitSettings::shipped());
        assert!(!apps.is_empty(), "the platform opener is always offered");
        assert_eq!(apps[0], platform_opener());
    }

    /// A configured list replaces the built-in one outright — it does not
    /// merge — so removing an app in the pane removes it from the menu.
    #[test]
    fn a_configured_list_wins_and_is_not_merged() {
        let settings = GitSettings { open_in: vec![app("Zed", "zed")], ..GitSettings::shipped() };
        assert_eq!(effective_apps(&settings), vec![app("Zed", "zed")]);
    }

    /// Every preset is launchable as written — the picker must never offer
    /// a command the parser would refuse — and every detected editor is one
    /// of the presets, so the two lists cannot drift apart.
    #[test]
    fn presets_parse_and_cover_every_detected_editor() {
        let presets = presets();
        assert!(!presets.is_empty());
        for app in &presets {
            assert!(parse_command(&app.command).is_some(), "{app:?}");
            assert!(!app.name.is_empty());
        }
        let preset_names: Vec<&str> = presets.iter().map(|a| a.name.as_str()).collect();
        for app in installed_editors() {
            assert!(preset_names.contains(&app.name.as_str()), "{app:?} is not a preset");
        }
    }

    /// A hand-edited entry missing its name or command is dropped, not
    /// offered; a list of nothing but those behaves like an empty list.
    #[test]
    fn unusable_configured_entries_are_dropped_and_an_all_unusable_list_falls_back() {
        let settings = GitSettings {
            open_in: vec![app("", "code"), app("Zed", "zed"), app("Broken", ""), app("Odd", "\"\"")],
            ..GitSettings::shipped()
        };
        assert_eq!(effective_apps(&settings), vec![app("Zed", "zed")]);
        let none_usable =
            GitSettings { open_in: vec![app("", "code"), app("Broken", "")], ..GitSettings::shipped() };
        assert_eq!(effective_apps(&none_usable), default_apps());
    }

    #[test]
    fn a_bare_program_has_no_arguments() {
        assert_eq!(parse_command("code"), Some(("code".into(), vec![])));
        assert_eq!(parse_command("  code  "), Some(("code".into(), vec![])));
    }

    /// The macOS default entries are written in exactly this shape, so this
    /// is the case that must keep working.
    #[test]
    fn double_quotes_group_an_argument_with_spaces() {
        assert_eq!(
            parse_command("open -a \"Visual Studio Code\""),
            Some(("open".into(), vec!["-a".into(), "Visual Studio Code".into()]))
        );
        // Quotes can sit inside a word too — a shell would accept this and
        // so do we.
        assert_eq!(
            parse_command("open -a Visual\" \"Studio"),
            Some(("open".into(), vec!["-a".into(), "Visual Studio".into()]))
        );
    }

    #[test]
    fn a_blank_or_unterminated_command_cannot_be_launched() {
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("open -a \"Visual Studio"), None);
    }

    /// A quoted nothing is a word, and a word that is an empty program
    /// would reach `Command::new("")`. Refused at the parser so the pane and
    /// the launch path cannot disagree about it.
    #[test]
    fn a_quoted_empty_program_cannot_be_launched() {
        assert_eq!(parse_command("\"\""), None);
        assert_eq!(parse_command("\" \""), None);
        assert_eq!(parse_command("\"\" -a Zed"), None);
        // An empty *argument* is still fine — only the program must be real.
        assert_eq!(parse_command("open \"\""), Some(("open".into(), vec![String::new()])));
    }

    /// An unlaunchable entry is refused with a reason that names the app the
    /// user picked, not the empty string they never typed.
    #[test]
    fn launching_a_blank_command_names_the_app() {
        let err = launch(&app("Broken", ""), Path::new("/tmp")).unwrap_err();
        assert!(err.contains("Broken"), "{err}");
    }

    /// A program that does not exist fails at spawn, with the program named,
    /// rather than panicking or silently succeeding.
    #[test]
    fn launching_a_missing_program_reports_the_program() {
        let err = launch(&app("Nope", "oximux-no-such-program-4f2a"), Path::new("/tmp"))
            .unwrap_err();
        assert!(err.contains("oximux-no-such-program-4f2a"), "{err}");
    }
}
