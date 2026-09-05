//! What a spawned terminal shell should be, and what its environment needs.
//!
//! Two rules, both of which used to live twice — once in `oximux-pty`'s
//! in-process backend and once in the relay daemon's registry. They are the
//! same rule in both places, and the daemon is the one that has to be right:
//! a phone paired to a desktop asks for "a terminal", and only the host knows
//! what a terminal is there. Its own crate because those two consumers share
//! no other dependency (the daemon deliberately avoids the grid emulator).

use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};

/// Which shell family a new terminal pane runs on Windows.
///
/// Stored in `terminal.toml` (via `oximux-settings`) and surfaced as a
/// segmented control in the settings UI. Ignored off Windows, where the shell
/// is the inherited `$SHELL` or the POSIX fallback chain (see [`default_shell`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WindowsShell {
    /// Windows PowerShell or PowerShell 7, per [`WindowsPowerShell`]. The
    /// default, matching every prior release.
    #[default]
    #[serde(rename = "powershell")]
    PowerShell,
    /// The classic command processor, `cmd.exe`.
    #[serde(rename = "cmd")]
    CommandPrompt,
    /// Git for Windows' `bash.exe`, resolved from the standard install
    /// locations (or `OXIMUX_GIT_BASH_PATH`). Falls back to PowerShell when
    /// Git for Windows is not installed.
    #[serde(rename = "git-bash")]
    GitBash,
    /// Prefer Git Bash when Git for Windows is installed, else PowerShell.
    #[serde(rename = "auto")]
    Auto,
}

/// Which PowerShell binary the `PowerShell` family resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WindowsPowerShell {
    /// PowerShell 7+ (`pwsh.exe`) when present, else inbox Windows PowerShell.
    #[default]
    Auto,
    /// Force PowerShell 7+ (`pwsh.exe`); still falls back to inbox PowerShell
    /// if `pwsh.exe` cannot be found, so a terminal always opens.
    Pwsh,
    /// Force inbox Windows PowerShell 5.1 (`powershell.exe`).
    #[serde(rename = "powershell")]
    Windows,
}

/// A resolved shell to spawn: the program plus the argv and environment its
/// launch needs. `args`/`env` are empty for shells that need neither.
///
/// The shell-integration layer may still prepend its own argv (bash
/// `--rcfile`, PowerShell `-Command`) on top of these — see the desktop app's
/// `augment_spawn_config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedShell {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl ResolvedShell {
    fn program(program: String) -> Self {
        Self {
            program,
            args: Vec::new(),
            env: Vec::new(),
        }
    }
}

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
///
/// This is the daemon/relay fallback and the `SpawnConfig::default` shell; the
/// desktop app resolves the user's [`WindowsShell`] choice through
/// [`resolve_windows_shell`] instead. Both keep PowerShell as the default.
#[cfg(windows)]
fn windows_shell() -> String {
    resolve_windows_shell(WindowsShell::PowerShell, WindowsPowerShell::Auto).program
}

/// Resolve a Windows shell choice into a spawnable program + argv + env.
///
/// - `GitBash` / `Auto` probe for Git for Windows and fall back to PowerShell
///   when it is absent, so the terminal always opens.
/// - Git Bash launches interactive and NON-login (`-i`, no `-l`): the login
///   profile ignores the `--rcfile` the shell-integration layer prepends,
///   which would drop the OSC 133 marks. A non-login interactive shell still
///   rebuilds the full MSYS `PATH` via `/etc/bash.bashrc`. `CHERE_INVOKING`
///   keeps it in the pane's cwd; `MSYSTEM`/`LANG` seed the MSYS locale that
///   the (skipped) login profile would otherwise set.
/// - PowerShell/cmd carry no argv here; the shell-integration layer adds
///   PowerShell's `-Command` bootstrap, and an empty argv is what lets it.
#[cfg(windows)]
pub fn resolve_windows_shell(shell: WindowsShell, powershell: WindowsPowerShell) -> ResolvedShell {
    match shell {
        WindowsShell::CommandPrompt => ResolvedShell::program(cmd_path()),
        WindowsShell::PowerShell => ResolvedShell::program(powershell_path(powershell)),
        WindowsShell::GitBash | WindowsShell::Auto => git_bash_shell()
            .unwrap_or_else(|| ResolvedShell::program(powershell_path(powershell))),
    }
}

/// A [`ResolvedShell`] for Git for Windows' bash, or `None` when it is not
/// installed. See [`resolve_windows_shell`] for why these args/env are chosen.
#[cfg(windows)]
fn git_bash_shell() -> Option<ResolvedShell> {
    let program = git_bash_path()?;
    Some(ResolvedShell {
        program,
        // `-i` only; `--rcfile` (added later) is honoured only for a non-login
        // interactive shell.
        args: vec!["-i".to_string()],
        env: vec![
            // 64-bit MSYS runtime — Git for Windows' default. Without it a
            // non-login shell can pick the wrong mount table.
            ("MSYSTEM".to_string(), "MINGW64".to_string()),
            // Stay in the directory the pane was spawned in rather than $HOME.
            ("CHERE_INVOKING".to_string(), "1".to_string()),
            // The login profile would set this; seed it so multibyte paths and
            // output are UTF-8 rather than the C locale.
            ("LANG".to_string(), "en_US.UTF-8".to_string()),
        ],
    })
}

/// Locate Git for Windows' `bash.exe`.
///
/// Order: `OXIMUX_GIT_BASH_PATH` override, then the standard per-machine and
/// per-user install roots, then the install `git` on `PATH` resolves to. Only
/// a real Git-for-Windows layout counts, so a WSL/Cygwin `bash.exe` on `PATH`
/// is never mistaken for it.
#[cfg(windows)]
fn git_bash_path() -> Option<String> {
    use std::path::{Path, PathBuf};

    // 1. Operator override for a non-standard install (mirrors Claude Code's
    //    CLAUDE_CODE_GIT_BASH_PATH).
    if let Ok(custom) = std::env::var("OXIMUX_GIT_BASH_PATH")
        && Path::new(&custom).is_file()
    {
        return Some(custom);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    // 2. Standard install roots. `ProgramW6432` is the 64-bit Program Files
    //    even for a 32-bit process; `Programs\Git` under LOCALAPPDATA is the
    //    per-user (winget) install.
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Ok(root) = std::env::var(var) else { continue };
        let root = PathBuf::from(root);
        for suffix in [
            r"Git\bin\bash.exe",
            r"Git\usr\bin\bash.exe",
            r"Programs\Git\bin\bash.exe",
            r"Programs\Git\usr\bin\bash.exe",
        ] {
            candidates.push(root.join(suffix));
        }
    }

    // 3. Whatever install the `git` on PATH belongs to (Scoop, Chocolatey,
    //    PortableGit). Climb the ancestors of `git.exe` and probe the two
    //    bash locations under each — the root is a couple of levels up
    //    (`<root>\cmd\git.exe`, `<root>\mingw64\bin\git.exe`).
    if let Ok(git) = which::which("git") {
        let mut dir = git.parent();
        let mut hops = 0;
        while let Some(d) = dir {
            candidates.push(d.join(r"bin\bash.exe"));
            candidates.push(d.join(r"usr\bin\bash.exe"));
            hops += 1;
            if hops >= 4 {
                break;
            }
            dir = d.parent();
        }
    }

    candidates
        .into_iter()
        .find(|c| c.is_file() && is_git_for_windows_bash(c))
        .map(|c| c.to_string_lossy().into_owned())
}

/// True when `path` is a Git-for-Windows (or PortableGit) `bash.exe`, as
/// opposed to a WSL/Cygwin/MSYS2-standalone one. The Git layout always places
/// bash under `...\bin\bash.exe` or `...\usr\bin\bash.exe` with `git` or
/// `portablegit` in an ancestor directory name.
#[cfg(windows)]
fn is_git_for_windows_bash(path: &std::path::Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    (lower.ends_with(r"\bin\bash.exe") || lower.ends_with(r"\usr\bin\bash.exe"))
        && (lower.contains(r"\git\") || lower.contains(r"\portablegit\"))
}

/// Resolve the `PowerShell` family to a concrete, spawnable exe.
///
/// `Auto`/`Pwsh` prefer PowerShell 7 (`pwsh.exe`) from its standard install
/// roots (skipping the Microsoft Store app-execution-alias stubs that ConPTY
/// cannot launch), then fall back to inbox Windows PowerShell, then `cmd.exe`
/// so a terminal always opens.
#[cfg(windows)]
fn powershell_path(powershell: WindowsPowerShell) -> String {
    match powershell {
        WindowsPowerShell::Windows => windows_powershell_path(),
        WindowsPowerShell::Auto | WindowsPowerShell::Pwsh => {
            pwsh_path().unwrap_or_else(windows_powershell_path)
        }
    }
}

/// PowerShell 7+ (`pwsh.exe`), or `None` if not installed. Rejects the
/// zero-byte Store app-execution-alias reparse points under `\WindowsApps\`,
/// which ConPTY's `CreateProcessW(lpApplicationName)` refuses with
/// ERROR_ACCESS_DENIED.
#[cfg(windows)]
fn pwsh_path() -> Option<String> {
    use std::path::{Path, PathBuf};

    let mut candidates: Vec<PathBuf> = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(root) = std::env::var(var) {
            for major in ["7", "8", "6"] {
                candidates.push(Path::new(&root).join("PowerShell").join(major).join("pwsh.exe"));
            }
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        for major in ["7", "8", "6"] {
            candidates.push(
                Path::new(&local)
                    .join(r"Microsoft\PowerShell")
                    .join(major)
                    .join("pwsh.exe"),
            );
        }
    }
    if let Ok(on_path) = which::which("pwsh") {
        candidates.push(on_path);
    }
    candidates
        .into_iter()
        .find(|c| is_real_executable(c))
        .map(|c| c.to_string_lossy().into_owned())
}

/// Inbox Windows PowerShell 5.1 at its fixed location, then a PATH lookup,
/// then `cmd.exe` as the last resort.
#[cfg(windows)]
fn windows_powershell_path() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let inbox = std::path::Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if inbox.exists() {
        return inbox.to_string_lossy().into_owned();
    }
    if let Ok(on_path) = which::which("powershell") {
        return on_path.to_string_lossy().into_owned();
    }
    cmd_path()
}

/// `%ComSpec%`, else `cmd.exe` at its fixed location.
#[cfg(windows)]
fn cmd_path() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    std::env::var("COMSPEC").unwrap_or_else(|_| format!(r"{root}\System32\cmd.exe"))
}

/// True for a real, launchable exe. Rejects the Microsoft Store
/// app-execution-alias stubs (zero-byte reparse points under `\WindowsApps\`)
/// that resolve on PATH but cannot be spawned via ConPTY.
#[cfg(windows)]
fn is_real_executable(path: &std::path::Path) -> bool {
    if path.to_string_lossy().to_ascii_lowercase().contains(r"\windowsapps\") {
        return false;
    }
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
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

/// Environment variables that tell a child "this terminal cannot do colour".
///
/// `NO_COLOR` (any non-empty value) and `FORCE_COLOR=0` are both honoured by
/// essentially every modern CLI — chalk, supports-color, clap, ripgrep, and the
/// agent CLIs OxiMux exists to host.
const COLOUR_SUPPRESSORS: &[&str] = &["NO_COLOR", "FORCE_COLOR"];

/// Drop inherited "no colour" flags from a PTY child's environment.
///
/// OxiMux forces `TERM=xterm-256color` and `COLORTERM=truecolor` on every PTY
/// child, because a GUI-launched app (and a detached relay daemon even more so)
/// inherits no terminal identity of its own. Passing an inherited `NO_COLOR`
/// through alongside those is self-contradictory: the same environment would
/// tell the child both that the terminal renders 24-bit colour and that it must
/// not use any.
///
/// The variable is almost never the user's: it is injected by whatever launched
/// OxiMux. Coding agents set `NO_COLOR=1` on their child processes so tool
/// output arrives as clean text — which is correct for the tools they run, and
/// wrong for a terminal emulator started from one, whose panes then render
/// every agent, pager and build tool in flat monochrome. Nothing reports it;
/// the terminal simply looks wrong.
///
/// The cost is real and worth naming: a user who sets `NO_COLOR` globally
/// loses it *for OxiMux panes only*. The escape hatch is that this runs before
/// the caller-supplied environment is applied, so an explicit
/// `SpawnConfig`/`SpawnArgs` entry still wins, as does anything the user's own
/// shell profile exports once the pane is live.
pub fn clear_inherited_colour_suppression(command: &mut CommandBuilder) {
    for key in COLOUR_SUPPRESSORS {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removal has to happen for every name a CLI might read, and it has to
    /// leave the terminal-identity vars alone — stripping `COLORTERM` here
    /// would break the very thing this exists to protect.
    #[test]
    fn colour_suppressors_are_removed_and_identity_is_left_alone() {
        let mut command = CommandBuilder::new("cmd");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for key in COLOUR_SUPPRESSORS {
            command.env(key, "1");
        }

        clear_inherited_colour_suppression(&mut command);

        for key in COLOUR_SUPPRESSORS {
            assert!(
                command.get_env(key).is_none(),
                "{key} survived the scrub and would still silence colour"
            );
        }
        assert_eq!(command.get_env("TERM").unwrap(), "xterm-256color");
        assert_eq!(command.get_env("COLORTERM").unwrap(), "truecolor");
    }

    /// A caller that deliberately asks for no colour must still get it. This is
    /// the escape hatch the doc comment promises, and it only holds because
    /// every spawn site applies the caller's environment *after* the scrub.
    #[test]
    fn a_caller_can_still_ask_for_no_colour_afterwards() {
        let mut command = CommandBuilder::new("cmd");
        command.env("NO_COLOR", "1");
        clear_inherited_colour_suppression(&mut command);
        command.env("NO_COLOR", "1");
        assert_eq!(command.get_env("NO_COLOR").unwrap(), "1");
    }

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

    #[test]
    fn windows_shell_choices_have_stable_toml_spellings() {
        // These strings are what lands in `terminal.toml`; changing one
        // silently invalidates every user's saved choice.
        for (choice, spelling) in [
            (WindowsShell::PowerShell, "\"powershell\""),
            (WindowsShell::CommandPrompt, "\"cmd\""),
            (WindowsShell::GitBash, "\"git-bash\""),
            (WindowsShell::Auto, "\"auto\""),
        ] {
            let json = serde_json::to_string(&choice).expect("serialize");
            assert_eq!(json, spelling, "{choice:?} serialized wrong");
            let back: WindowsShell = serde_json::from_str(spelling).expect("deserialize");
            assert_eq!(back, choice);
        }
        for (choice, spelling) in [
            (WindowsPowerShell::Auto, "\"auto\""),
            (WindowsPowerShell::Pwsh, "\"pwsh\""),
            (WindowsPowerShell::Windows, "\"powershell\""),
        ] {
            let json = serde_json::to_string(&choice).expect("serialize");
            assert_eq!(json, spelling, "{choice:?} serialized wrong");
        }
    }

    #[test]
    fn windows_shell_and_powershell_default_to_powershell_auto() {
        assert_eq!(WindowsShell::default(), WindowsShell::PowerShell);
        assert_eq!(WindowsPowerShell::default(), WindowsPowerShell::Auto);
    }

    #[cfg(windows)]
    #[test]
    fn powershell_resolves_to_an_exe_that_exists() {
        // The default family must always land on a spawnable program.
        let resolved = resolve_windows_shell(WindowsShell::PowerShell, WindowsPowerShell::Auto);
        assert!(
            std::path::Path::new(&resolved.program).exists(),
            "resolved PowerShell {:?} does not exist",
            resolved.program
        );
        assert!(resolved.args.is_empty(), "PowerShell carries no argv here");
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_choice_resolves_and_carries_msys_env_when_installed() {
        // Self-skipping: only asserts when Git for Windows is actually present,
        // exactly like the NO_COLOR wiring test.
        let resolved = resolve_windows_shell(WindowsShell::GitBash, WindowsPowerShell::Auto);
        let name = std::path::Path::new(&resolved.program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name == "bash.exe" {
            assert!(is_git_for_windows_bash(std::path::Path::new(&resolved.program)));
            assert_eq!(resolved.args, vec!["-i".to_string()]);
            assert!(
                resolved.env.iter().any(|(k, _)| k == "CHERE_INVOKING"),
                "git bash must stay in the pane cwd"
            );
        } else {
            // No Git for Windows here — must have fallen back to PowerShell.
            assert!(
                name == "pwsh.exe" || name == "powershell.exe",
                "git-bash fallback should be PowerShell, got {name:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_store_alias_stub_is_never_a_real_executable() {
        // Any path under \WindowsApps\ is rejected outright, whether or not it
        // exists, because ConPTY cannot spawn the alias reparse points there.
        assert!(!is_real_executable(std::path::Path::new(
            r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\pwsh.exe"
        )));
    }
}

/// Shell spellings for integration tests that drive a real PTY.
///
/// Behind a feature so it never ships in a normal build, but in this crate
/// rather than copied into each test file: four suites need the same rule, and
/// four copies of "how do I say this to the platform's shell" is precisely the
/// thing that drifts — someone fixes cmd quoting in one and not the others.
///
/// `cmd.exe`, not PowerShell, on purpose. Tests assert on shell output, and cmd
/// has no banner, no profile to load, and the same `echo`/`exit` spelling as
/// `sh`. Predictability beats matching what a user would actually get; the
/// product's own default is resolved by [`default_shell`].
#[cfg(feature = "test-support")]
pub mod test_support {
    use std::path::PathBuf;

    /// The shell these tests drive.
    pub fn test_shell() -> String {
        if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string())
        } else {
            "/bin/sh".to_string()
        }
    }

    /// A directory a spawned shell can sit in. `/tmp` is not a path on Windows,
    /// and `/` there names the current drive root, which is not somewhere to
    /// start a process.
    pub fn test_cwd() -> PathBuf {
        if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        }
    }

    /// Terminate command lines for the shell under test. `cmd.exe` reads
    /// console input terminated by CR; `sh` accepts either.
    pub fn lines(cmds: &[&str]) -> Vec<u8> {
        let eol = if cfg!(windows) { "\r\n" } else { "\n" };
        cmds.iter()
            .flat_map(|c| format!("{c}{eol}").into_bytes())
            .collect()
    }

    /// argv that runs `commands` non-interactively and exits — `sh -c` and
    /// `cmd /c`. The separator differs: `;` sequences unconditionally in sh,
    /// and `&` is its cmd counterpart (`&&` would stop at the first failure,
    /// which would swallow a deliberate non-zero exit).
    pub fn run_script(commands: &[&str]) -> Vec<String> {
        if cfg!(windows) {
            vec!["/c".to_string(), commands.join(" & ")]
        } else {
            vec!["-c".to_string(), commands.join("; ")]
        }
    }

    /// `echo` of two env vars joined by a pipe, in the running shell's syntax.
    /// The values can only appear in the EXPANDED output, never in the command
    /// line the tty echoes back — which is what keeps the assertion honest
    /// instead of reading back its own input.
    pub fn echo_two_vars(a: &str, b: &str) -> String {
        if cfg!(windows) {
            // `^` escapes the pipe; cmd would otherwise read it as an operator.
            format!("echo %{a}%^|%{b}%")
        } else {
            format!("echo \"${a}|${b}\"")
        }
    }

    /// `echo <label>=<value of var>` in the running shell's syntax. `sh`
    /// expands `$VAR`, `cmd` expands `%VAR%` — a line written for one prints
    /// the other's sigil literally, and the assertion then reads back the
    /// command instead of the environment.
    pub fn echo_var(label: &str, var: &str) -> String {
        if cfg!(windows) {
            format!("echo {label}=%{var}%")
        } else {
            format!("echo {label}=${var}")
        }
    }

    /// `echo <present>` when `var` is set and non-empty, `<absent>` otherwise.
    /// `cmd` has no `[ -n ... ]`: fed one it stops at `-n was unexpected at
    /// this time`, which is a shell parse error and not the assertion failing.
    pub fn echo_if_var_set(var: &str, present: &str, absent: &str) -> String {
        if cfg!(windows) {
            format!("if defined {var} (echo {present}) else (echo {absent})")
        } else {
            format!("if [ -n \"${var}\" ]; then echo {present}; else echo {absent}; fi")
        }
    }

    /// A program and argv that print `arg` and exit without reading input, so a
    /// marker can only have come from argv. Windows has no `/bin/echo`;
    /// `cmd /c echo` is the nearest thing that still never touches the console.
    pub fn echo_program(arg: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                test_shell(),
                vec!["/c".to_string(), "echo".to_string(), arg.to_string()],
            )
        } else {
            ("/bin/echo".to_string(), vec![arg.to_string()])
        }
    }
}

/// Variables Claude Code stamps on child processes to mark them as part of a
/// running session — identity-of-launch only, never user configuration, so
/// dropping them cannot lose a setting the user chose.
///
/// One list, three consumers (the desktop app, the relay daemon, and
/// `oximux serve`): each is a process that spawns agent CLIs and terminals,
/// and an inherited `CLAUDE_CODE_CHILD_SESSION` makes a spawned `claude`
/// treat itself as a nested child and switch transcript saving off — the
/// exact history the hosts exist to keep.
pub const CLAUDE_SESSION_MARKERS: [&str; 12] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDECODE",
    "CLAUDE_CODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_PARENT_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
    // An inherited "already sandboxed" claim makes `claude` skip its folder
    // trust prompt — a security gate, not just bookkeeping.
    "CLAUDE_CODE_SANDBOXED",
];

/// Remove inherited Claude Code session markers from this process, returning
/// the names that were actually present (for the caller to log with its own
/// subscriber — this crate stays logging-free).
///
/// # Safety contract (why this is not `unsafe fn`)
///
/// Call from the top of `main`, **before any thread exists**: `remove_var` is
/// only unsound when it races a concurrent environment read, and at that
/// point in a process's life there is nothing to race.
pub fn scrub_inherited_claude_session_markers() -> Vec<&'static str> {
    let mut dropped = Vec::new();
    for name in CLAUDE_SESSION_MARKERS {
        if std::env::var_os(name).is_none() {
            continue;
        }
        // SAFETY: single-threaded by the documented contract above.
        unsafe { std::env::remove_var(name) };
        dropped.push(name);
    }
    dropped
}
