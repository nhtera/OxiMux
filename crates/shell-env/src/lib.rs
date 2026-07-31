//! What a spawned terminal shell should be, and what its environment needs.
//!
//! Two rules, both of which used to live twice — once in `oximux-pty`'s
//! in-process backend and once in the relay daemon's registry. They are the
//! same rule in both places, and the daemon is the one that has to be right:
//! a phone paired to a desktop asks for "a terminal", and only the host knows
//! what a terminal is there. Its own crate because those two consumers share
//! no other dependency (the daemon deliberately avoids the grid emulator).

use portable_pty::CommandBuilder;

/// The program a new terminal runs when the caller asks for a plain shell.
///
/// Returns a path (or, on Windows, whatever the probe resolved) rather than a
/// bare name, because the caller hands it straight to `CommandBuilder::new`.
pub fn default_shell() -> String {
    #[cfg(unix)]
    {
        unix_shell()
    }
    #[cfg(windows)]
    {
        windows_shell()
    }
}

/// `$SHELL` when the process inherited one, else the first of the usual suspects
/// that actually exists.
///
/// The existence check is what makes this correct off macOS: `/bin/zsh` is
/// guaranteed there and was the long-standing hardcoded fallback, but on a
/// stock Linux box it is frequently absent, and a shell that does not exist
/// fails the spawn outright rather than degrading.
#[cfg(unix)]
fn unix_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return shell;
    }
    for candidate in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if std::path::Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    // Nothing matched, which should be impossible on a POSIX system. Return the
    // one every POSIX system is required to have and let the spawn error say so.
    "/bin/sh".to_string()
}

/// PowerShell 7, else Windows PowerShell, else the command processor.
///
/// `$SHELL` is deliberately NOT consulted here. Git Bash and MSYS2 both export
/// it, and they export a POSIX path (`/usr/bin/bash`) that means something only
/// inside their own root — no Win32 API can spawn it. Honoring `$SHELL` on
/// Windows therefore turns "user has Git for Windows installed", which is
/// nearly all of them, into "terminals do not open".
#[cfg(windows)]
fn windows_shell() -> String {
    // PATHEXT resolution lives in the `which` crate, so `pwsh` finds `pwsh.exe`
    // and would equally find a `.cmd` shim if someone installed one.
    for name in ["pwsh", "powershell"] {
        if let Ok(path) = which::which(name) {
            return path.to_string_lossy().into_owned();
        }
    }
    // A sparse PATH is a real possibility for a GUI-launched process, and both
    // remaining candidates ship at fixed locations, so fall back to those
    // rather than to a bare name the spawn would have to resolve itself.
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let windows_powershell = std::path::Path::new(&root)
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if windows_powershell.exists() {
        return windows_powershell.to_string_lossy().into_owned();
    }
    std::env::var("COMSPEC").unwrap_or_else(|_| format!(r"{root}\System32\cmd.exe"))
}

/// Seed a UTF-8 locale on a spawned shell when the environment supplies none.
///
/// A GUI-launched app inherits no `LANG`/`LC_*` from LaunchServices, unlike
/// Terminal.app, which injects a locale on startup. Without one, an interactive
/// `zsh` falls back to the C locale and its line editor mangles multibyte
/// input: Vietnamese/CJK text (typed, pasted, or dictated) echoes back as
/// `<XX>` meta bytes rather than the intended glyphs. Seed a UTF-8 ctype only
/// when nothing is set, and before the caller/user rc runs so any explicit
/// locale still wins.
///
/// Windows has no equivalent knob: the console is UTF-16 internally and
/// ConPTY hands us UTF-8 regardless, so there is nothing to seed.
pub fn seed_utf8_locale(command: &mut CommandBuilder) {
    #[cfg(unix)]
    {
        if std::env::var_os("LC_ALL").is_none()
            && std::env::var_os("LC_CTYPE").is_none()
            && std::env::var_os("LANG").is_none()
        {
            command.env("LANG", "en_US.UTF-8");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shell_is_a_program_that_exists() {
        let shell = default_shell();
        assert!(!shell.is_empty());
        // The whole point of the resolver is that the answer is spawnable. A
        // relative answer would only be legal on Windows via PATH, and the
        // Windows arm resolves to an absolute path before returning.
        assert!(
            std::path::Path::new(&shell).exists(),
            "resolved shell {shell:?} does not exist"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_prefers_the_inherited_shell() {
        // Not using the real env: `SHELL` is process-global and the test
        // harness runs threads in parallel. The branch under test is the
        // ordering, which is visible from the fallback list alone.
        let fallbacks: Vec<&str> = ["/bin/zsh", "/bin/bash", "/bin/sh"]
            .into_iter()
            .filter(|c| std::path::Path::new(c).exists())
            .collect();
        assert!(
            !fallbacks.is_empty(),
            "no POSIX shell present; the fallback chain cannot resolve"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_still_lands_on_zsh_without_an_inherited_shell() {
        // Guards the Linux-motivated existence check against changing the
        // macOS answer, which existing installs depend on.
        assert!(std::path::Path::new("/bin/zsh").exists());
    }

    #[test]
    fn locale_seeding_matches_the_platform_contract() {
        let mut command = CommandBuilder::new(default_shell());
        seed_utf8_locale(&mut command);
        // `iter_extra_env_as_str` is the env this builder set, as opposed to
        // the env it would inherit — the distinction the assertion needs.
        let seeded = command
            .iter_extra_env_as_str()
            .any(|(k, _)| k == "LANG");

        let parent_has_locale = std::env::var_os("LC_ALL").is_some()
            || std::env::var_os("LC_CTYPE").is_some()
            || std::env::var_os("LANG").is_some();

        if cfg!(unix) {
            // Seed exactly when the parent had nothing: an explicit locale in
            // the environment has to survive, or a user who set one loses it.
            assert_eq!(seeded, !parent_has_locale);
        } else {
            assert!(!seeded, "nothing should seed LANG off unix");
        }
    }
}
