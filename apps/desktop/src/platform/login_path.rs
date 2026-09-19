//! Give a GUI-launched app the `PATH` its owner actually has.
//!
//! # The failure this exists to stop
//!
//! Agent backends are spawned by bare name — `Command::new("claude")`,
//! `Command::new("codex")` — and so is the `which` that detects which ones are
//! installed. That resolves through the inherited `PATH`, and what a process
//! inherits depends entirely on how it was started. A shell launch inherits the
//! `PATH` the user's login and `rc` files built; a Finder, Dock, Spotlight, or
//! desktop-launcher launch inherits whatever stub the session manager hands
//! out, and nothing else.
//!
//! Every agent CLI installs outside that stub — `~/.local/bin`, Homebrew, a
//! Node version manager. So a double-clicked OxiMux lists every agent as "not
//! installed", and starting one reports "Failed to start agent: spawn agent
//! process" with no hint that the cause is an environment variable.
//!
//! It hides well. Terminals keep working, because those are spawned through the
//! relay's login shell and pick the real `PATH` up on the way. Only the agent
//! path breaks — and never for a developer, who runs the binary from a shell
//! that already has it.
//!
//! # Why the whole process, rather than each spawn site
//!
//! Setting it once here fixes every present and future `Command::new`, and the
//! detection pass that decides which agents to offer. Threading an environment
//! through each call site would leave the next one to be written broken again,
//! and the symptom is far enough from the cause that it would not be noticed.
//!
//! # The three ways this module was itself wrong
//!
//! The first two were measured on 2026-09-19 against an installed 0.1.21 on
//! macOS 15.6, which listed all seven agents as "not installed" while every one
//! of them ran in the user's terminal. `ps eww` on the running app is what
//! settled it, and is the way to settle it again.
//!
//! 1. **The launch test compared `PATH` against launchd's stub as one exact
//!    string.** launchd hands a GUI app
//!    `/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin` — five entries, not the
//!    four in `<paths.h>`'s `_PATH_STDPATH`, which this module had hard-coded.
//!    The string never matched, so adoption never ran at all. The lesson is not
//!    "add `/usr/local/bin`": it is that a guard keyed to a string the OS owns
//!    will go stale again. See [`started_from_a_shell`].
//! 2. **The shell was run non-interactive.** For `zsh` that skips `~/.zshrc`,
//!    which is where `nvm`, `bun`, `pnpm`, and most CLI installers write their
//!    `PATH` line — so three of seven agents stayed invisible even once
//!    adoption ran. See [`probes`].
//! 3. **It was `#[cfg(unix)]`, and split `PATH` on `:`.** The gap is not
//!    macOS-only: a Linux desktop launcher hands out a systemd stub the same
//!    way, and on Windows a `PATH` the user's PowerShell profile extends is
//!    invisible to a process Explorer started. Everything here is now written
//!    against [`std::env::split_paths`] and a per-platform [`probes`] list.

use std::env::{join_paths, split_paths};
use std::ffi::OsString;
use std::io::{IsTerminal, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the **whole** probe sequence gets before it is abandoned.
///
/// An interactive shell runs the user's whole `rc` file, which may start a
/// version manager or touch the network; PowerShell loads `$PROFILE`. This is a
/// boot path, so it gets a cap rather than the benefit of the doubt.
///
/// Ten seconds, not the five this shipped with first. `zsh -lic` answered in
/// 0.8 s on the machine this was written on, which is exactly the measurement
/// that makes a small number look safe: a competitor shipped 5 s on the same
/// reasoning and had to raise it after a real `~/.zshrc` carrying `nvm`, `rvm`,
/// `conda`, and `gcloud` took 6-7 s on a loaded machine. The cost of being
/// generous is bounded and only paid when the shell is genuinely that slow; the
/// cost of being tight is the failure this whole module exists to prevent.
///
/// One budget for the sequence, not one per probe. Both Unix probes run the
/// same program, and on Windows `Auto` resolves to the inbox shell when `pwsh`
/// is absent, so a profile that hangs would otherwise cost this twice over with
/// nothing to show for it — and this blocks `main` before any window exists.
///
/// Note the number this replaces in spirit: before any of this, the probe ran
/// with no timeout at all.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Brackets around the answer, because a shell that loads the user's config
/// prints other things — instant prompts, version-manager notices, a MOTD.
/// Without them the first `PATH` entry would silently become "banner text glued
/// to a directory".
const BEGIN: &str = "__OXIMUX_PATH_BEGIN__";
const END: &str = "__OXIMUX_PATH_END__";

/// Whether the boot path already ran a probe in this process.
///
/// A cold launch pays for a shell to fill an empty cache; without this the
/// background refresh would immediately run a second one for the same answer.
/// Only ever set, never cleared — there is one boot per process.
static PROBED_THIS_LAUNCH: AtomicBool = AtomicBool::new(false);

/// Adopt the user's real `PATH` when this process was not started from a shell.
///
/// Call before any thread exists — see the `unsafe` note below. Callers must
/// skip the CLI subcommands (`notify`, `agent-status`): those are agent hooks,
/// they run many times a second, they already inherit a real `PATH` from the
/// app that spawned them, and they have no terminal — so they would otherwise
/// pay for a shell they do not need.
pub fn adopt_login_shell_path() {
    if started_from_a_shell() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let shell = probe_shell();
    // The previous launch already paid for this answer. Reusing it is what
    // keeps a slow `rc` file from being a slow launch — see [`cache`].
    let user = match shell.as_deref().and_then(cache::read) {
        Some(cached) => Some(cached),
        None => {
            // Nothing cached: a first launch, or the first since the user
            // changed shells. There is no fast answer to fall back on, and
            // starting with a stub `PATH` is the failure this module exists to
            // prevent, so this one launch waits.
            PROBED_THIS_LAUNCH.store(true, Ordering::Relaxed);
            let probed = user_path();
            if let (Some(shell), Some(path)) = (shell.as_deref(), probed.as_deref()) {
                cache::write(shell, path);
            }
            probed
        }
    };
    let Some(user) = user else {
        // Nothing to adopt. Left alone deliberately: a wrong `PATH` is worse
        // than a bare one, and the agent pane already reports what it cannot
        // find.
        tracing::warn!("could not read the user's shell PATH; agent CLIs may not be found");
        return;
    };
    let Some(merged) = merged(&user, &current) else {
        // `join_paths` refuses an entry containing the separator itself. Better
        // to keep the `PATH` we have than to write a mangled one.
        tracing::warn!("the user's shell PATH could not be merged; leaving this process' PATH");
        return;
    };
    if merged == current {
        return;
    }
    // SAFETY: `set_var` is unsound only when another thread reads the
    // environment concurrently. Two claims carry that here, and the second one
    // exists because the first is not enough on its own:
    //
    // 1. This runs at the top of `main`, before the tokio runtime and the GPUI
    //    executor are built, so none of the app's own threads exist yet.
    // 2. This function nonetheless *creates* threads on the way here — one
    //    reader per probe, in `run_with_timeout`. A probe that answered has
    //    had its reader joined. A probe that timed out leaves one parked in
    //    `read(2)` on a pipe, and that thread only reads bytes, allocates a
    //    `String`, and exits: it never touches the environment.
    unsafe { std::env::set_var("PATH", &merged) };
    tracing::info!(path = %merged.to_string_lossy(), "adopted the user's shell PATH");
}

/// Whether a terminal started this process, rather than a GUI shell.
///
/// This replaced a test that compared `PATH` to the session manager's stub as a
/// fixed string, which is how the module came to do nothing at all on a macOS
/// whose stub had grown an entry. A terminal is a property of *this* process
/// that no OS release renames, it means the same thing on all three platforms,
/// and it is the actual question being asked — "did a shell set this up for
/// us?" — rather than a proxy for it.
///
/// All three descriptors, because redirecting one (`oximux > log.txt`) does not
/// make a terminal launch into a GUI launch. A GUI launch has none: on macOS
/// launchd hands the bundle `/dev/null` on all three (verified with `lsof`), a
/// desktop launcher does the same, and an Explorer-launched Windows binary has
/// no console at all.
///
/// The failure direction is deliberate. Guessing "shell" when it was a GUI is
/// the silent, total failure — every agent reported missing. Guessing "GUI"
/// when it was a shell costs one shell invocation at boot and yields the
/// `PATH` that process already had.
fn started_from_a_shell() -> bool {
    looks_like_a_shell_launch(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    )
}

/// The rule itself, separated from the descriptors so it can be tested.
///
/// Worth the seam: a wrong answer here is what shipped broken for fourteen
/// months, and the version that shipped had tests — of the wrong constant.
/// Stating the rule as a function makes the contract something a test can
/// hold, rather than something a comment asserts.
fn looks_like_a_shell_launch(stdin: bool, stdout: bool, stderr: bool) -> bool {
    stdin || stdout || stderr
}

/// Re-probe the user's shell and cache the answer for the next launch.
///
/// Call once per session, **after** the tokio runtime exists and after the
/// helper-CLI short-circuits — a hook process must not spawn a login shell,
/// and a second instance that bows out to the single-instance guard should not
/// either.
///
/// It deliberately does **not** write the process environment. On Unix
/// `set_var` is unsound the moment another thread can read the environment,
/// and by the time this runs many can; `shell::integrations::path_refresh`
/// records the same reasoning for the same reason. So a CLI installed while
/// the app is running becomes visible at the next launch rather than this one.
/// That is the cost of not blocking every launch on the shell, and it is why
/// this writes a cache instead of trying to be clever.
pub fn refresh_cached_path_in_background() {
    // A terminal launch never adopted anything, so it has no cache to keep
    // warm — and a developer's `cargo run` should not pay for a login shell.
    // The next GUI launch refreshes its own cache.
    if started_from_a_shell() {
        return;
    }
    // A cold launch already probed, synchronously, for this exact answer.
    if PROBED_THIS_LAUNCH.load(Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        let Some(shell) = probe_shell() else {
            return;
        };
        let Some(path) = user_path() else {
            return;
        };
        cache::write(&shell, &path);
    });
}

/// Which shell the answer would come from, used to key the cache.
///
/// The first probe's program: the one that actually answers on a healthy
/// machine, and the one whose change (a user switching login shells) should
/// invalidate what the previous one said.
fn probe_shell() -> Option<String> {
    probes().into_iter().next().map(|probe| probe.program)
}

/// The previous launch's answer, so this one does not wait for it.
///
/// A file rather than an in-process memo, because the probe is slow once per
/// *launch*, not once per call — a cache that dies with the process saves
/// nothing. The trade it makes is explicit: a launch starts with the `PATH` the
/// session before it saw, and [`refresh_cached_path_in_background`] brings the
/// file up to date for the launch after. Freshness is one launch behind;
/// correctness is not, because the merge keeps whatever the process already had.
mod cache {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    /// `cache_dir` and not `data_dir`, deliberately: losing this file costs one
    /// slow launch and nothing else, which is exactly the contract that
    /// directory documents.
    fn file() -> Option<PathBuf> {
        crate::app_paths::cache_dir().map(|dir| dir.join("shell-path"))
    }

    /// The cached `PATH`, if the shell that wrote it is the one we would ask now.
    pub(super) fn read(shell: &str) -> Option<OsString> {
        read_from(&file()?, shell)
    }

    /// Record `path` as what `shell` answers. Failure is silent on purpose:
    /// an unwritable cache directory costs a slow launch, and there is nothing
    /// the user could do about it that a log line would prompt.
    pub(super) fn write(shell: &str, path: &OsStr) {
        let Some(file) = file() else {
            return;
        };
        write_to(&file, shell, path);
    }

    /// The pair above, against a named file rather than the app's cache
    /// directory — which is keyed to the real user's home and so cannot be
    /// pointed somewhere else from a test without writing the environment.
    fn read_from(file: &Path, shell: &str) -> Option<OsString> {
        let raw = std::fs::read_to_string(file).ok()?;
        let (wrote, path) = raw.split_once('\n')?;
        let path = path.trim();
        // A user who changed their login shell gets one slow launch rather than
        // a `PATH` built by a shell they no longer use.
        (wrote == shell && !path.is_empty()).then(|| OsString::from(path))
    }

    fn write_to(file: &Path, shell: &str, path: &OsStr) {
        if let Some(parent) = file.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        // One line each, in an order that makes the file readable by hand:
        // the shell that answered, then what it said.
        let body = format!("{shell}\n{}", path.to_string_lossy());

        // Written to a sibling and renamed, never straight over the
        // destination. `fs::write` truncates first, and the thread doing this
        // is detached: quitting the app kills it mid-write and leaves a short
        // file behind. A file cut off inside the PATH line still satisfies
        // every check `read_from` can make, so the next launch would adopt a
        // truncated PATH and lose agents — the very symptom this module
        // exists to prevent. `rename` is atomic on POSIX, and Rust's Windows
        // implementation passes `MOVEFILE_REPLACE_EXISTING`, so a reader sees
        // either the old file or the whole new one.
        //
        // The pid keeps two processes from colliding on the temp name. They
        // should not overlap — the single-instance guard — but a crash can
        // leave one behind, and a name nobody else uses makes that litter
        // harmless rather than a corrupt read.
        let temp = file.with_extension(format!("tmp-{}", std::process::id()));
        if let Err(err) = std::fs::write(&temp, body) {
            tracing::debug!(?err, "could not cache the shell PATH for the next launch");
            return;
        }
        if let Err(err) = std::fs::rename(&temp, file) {
            tracing::debug!(?err, "could not replace the cached shell PATH");
            let _ = std::fs::remove_file(&temp);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scratch() -> tempfile::TempDir {
            tempfile::tempdir().expect("a scratch directory")
        }

        #[test]
        fn what_was_written_comes_back() {
            let dir = scratch();
            let file = dir.path().join("nested").join("shell-path");
            let path = OsString::from("/opt/homebrew/bin:/usr/bin");
            // The nested directory is the real case: the cache directory does
            // not exist on a machine that has never run the app.
            write_to(&file, "/bin/zsh", &path);
            assert_eq!(read_from(&file, "/bin/zsh"), Some(path));
        }

        #[test]
        fn a_rewrite_is_all_or_nothing_and_leaves_no_litter() {
            // The defect this guards: `fs::write` truncates first, and the
            // thread that calls this is detached, so quitting mid-write left a
            // short file that `read_from` could not tell from a real one.
            let dir = scratch();
            let file = dir.path().join("shell-path");
            write_to(&file, "/bin/zsh", OsStr::new("/first/bin"));
            write_to(&file, "/bin/zsh", OsStr::new("/second/bin:/third/bin"));

            assert_eq!(
                read_from(&file, "/bin/zsh").as_deref(),
                Some(OsStr::new("/second/bin:/third/bin")),
            );
            // The temp file is renamed onto the destination, never left behind
            // to accumulate one per launch.
            let left: Vec<_> = std::fs::read_dir(dir.path())
                .expect("readable")
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(left, vec!["shell-path".to_string()], "{left:?}");
        }

        #[test]
        fn a_different_shell_invalidates_the_answer() {
            let dir = scratch();
            let file = dir.path().join("shell-path");
            write_to(&file, "/bin/zsh", OsStr::new("/opt/homebrew/bin"));
            // Someone who switched to fish must not inherit zsh's PATH: the
            // whole point of the probe is that it is the user's own shell.
            assert!(read_from(&file, "/opt/homebrew/bin/fish").is_none());
        }

        #[test]
        fn nothing_usable_reads_as_no_answer() {
            let dir = scratch();
            let missing = dir.path().join("never-written");
            assert!(read_from(&missing, "/bin/zsh").is_none());

            // Truncated to the shell line: a half-written file must not be
            // read as "this shell has an empty PATH".
            let partial = dir.path().join("partial");
            std::fs::write(&partial, "/bin/zsh").expect("write");
            assert!(read_from(&partial, "/bin/zsh").is_none());

            let blank = dir.path().join("blank");
            std::fs::write(&blank, "/bin/zsh\n   \n").expect("write");
            assert!(read_from(&blank, "/bin/zsh").is_none());
        }
    }
}

/// Ask the user's shell what `PATH` it sets up, trying each probe in turn.
fn user_path() -> Option<OsString> {
    let deadline = Instant::now() + SHELL_TIMEOUT;
    probes().into_iter().find_map(|probe| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // A fallback with no time left is not a fallback; stop rather than
        // spawn a shell that will only be killed.
        (!remaining.is_zero()).then(|| probe.run(remaining)).flatten()
    })
}

/// One way to ask a shell for its `PATH`.
struct Probe {
    program: String,
    args: Vec<String>,
}

impl Probe {
    fn run(self, budget: Duration) -> Option<OsString> {
        let printed = run_with_timeout(&self.program, &self.args, budget)?;
        path_between_markers(&printed).map(OsString::from)
    }
}

/// The shells to ask, best answer first.
///
/// Interactive **and** login, in that order of importance. The login files are
/// the ones a shell reads to build `PATH` in theory; in practice `zsh` reads
/// `~/.zshrc` only when interactive, and that is the file `nvm`, `bun`,
/// `pnpm`, and the agent CLIs' own installers append their `PATH` line to. On
/// the machine this was measured on, `-lc` found four of seven agents and
/// `-lic` found all seven.
///
/// The cost of `-i` is the user's whole `rc` file on a boot path, which is what
/// [`SHELL_TIMEOUT`] and the markers are for. `-lc` follows as a fallback for
/// any shell that refuses `-i` without a terminal, so the worst case is the
/// behaviour this had before rather than nothing at all.
#[cfg(unix)]
fn probes() -> Vec<Probe> {
    // Not a bare `$SHELL`: off macOS that variable is frequently unset, and
    // `default_shell` falls back to a shell that is actually on the box.
    let shell = oximux_shell_env::default_shell();
    let script = unix_script(&shell);
    // Separate flags rather than a bundled `-lic`, because bundling is a
    // convention of POSIX shells' own option parsers and `fish` is not one.
    vec![
        Probe {
            program: shell.clone(),
            args: vec!["-l".into(), "-i".into(), "-c".into(), script.clone()],
        },
        Probe { program: shell, args: vec!["-l".into(), "-c".into(), script] },
    ]
}

/// The one-liner that prints a bracketed `PATH`, in this shell's own syntax.
///
/// `fish` is the exception worth carrying: it stores `$PATH` as a list, so the
/// POSIX `"$PATH"` a quoted expansion produces there is **space**-separated.
/// That is the nastier kind of wrong — it parses, so the shell answers, and the
/// answer becomes one nonexistent directory whose name contains every real one.
/// `string join` is how fish spells the colon-joined form.
///
/// Still unhandled, and deliberately: `nu`, `csh`, and `tcsh`, none of which
/// accept this script or these flags. They fail the way an absent shell does —
/// no markers, no answer, `PATH` untouched — which is the safe direction.
#[cfg(unix)]
fn unix_script(shell: &str) -> String {
    let name = std::path::Path::new(shell).file_name().and_then(std::ffi::OsStr::to_str);
    if name == Some("fish") {
        format!("printf '%s%s%s' '{BEGIN}' (string join : $PATH) '{END}'")
    } else {
        format!("printf '%s%s%s' '{BEGIN}' \"$PATH\" '{END}'")
    }
}

/// The shells to ask on Windows, best answer first.
///
/// PowerShell, because it is the one that loads a user profile — `$PROFILE` is
/// Windows' `~/.zshrc`, and a `PATH` line added there is exactly what a process
/// Explorer started cannot see. It also re-reads the registry `PATH`, so this
/// doubles as the repair for an app that was launched before an agent CLI was
/// installed.
///
/// Deliberately **not** `resolve_windows_shell`'s `Auto`, which prefers Git
/// Bash when Git for Windows is present: a Git Bash `PATH` is a list of MSYS
/// paths (`/c/Users/...`) that `Command::new` cannot resolve, so adopting one
/// would replace a thin `PATH` with an unusable one.
///
/// The fallback is inbox Windows PowerShell, forced. `Auto` prefers `pwsh`,
/// and a PowerShell 7 install that is present but broken — a half-finished
/// upgrade, a module that throws on load — would otherwise take the only probe
/// down with it. Deliberately **not** `cmd.exe /c echo`: that argument contains
/// a space, so Rust quotes it, and `cmd` re-parses quotes by its own rules
/// rather than the ones every other program follows. Reaching for `raw_arg` to
/// work around that buys a fallback with no profile support anyway.
#[cfg(windows)]
fn probes() -> Vec<Probe> {
    use oximux_shell_env::{WindowsPowerShell, WindowsShell};
    // `[Console]::Out.Write` rather than `Write-Host`: no trailing newline, and
    // no console-host formatting between the markers.
    let script = format!("[Console]::Out.Write('{BEGIN}' + $env:PATH + '{END}')");
    [WindowsPowerShell::Auto, WindowsPowerShell::Windows]
        .into_iter()
        .map(|flavour| Probe {
            program: oximux_shell_env::resolve_windows_shell(WindowsShell::PowerShell, flavour)
                .program,
            args: vec![
                "-NoLogo".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.clone(),
            ],
        })
        .collect()
}

/// Whether the closing marker has arrived, so the read can stop.
///
/// Scans the whole buffer rather than the tail: a themed prompt can emit
/// escapes after the marker, so "ends with" is not literally true of the bytes.
/// The buffer is a `PATH` plus a banner — kilobytes — so this stays cheap.
fn ends_with_marker(buf: &[u8]) -> bool {
    let end = END.as_bytes();
    buf.len() >= end.len() && buf.windows(end.len()).any(|w| w == end)
}

/// The text between the two markers, trimmed, or `None` if it is not there.
///
/// Kept separate from the spawn so the parsing has tests that do not depend on
/// which shell the machine running them happens to have.
fn path_between_markers(printed: &str) -> Option<String> {
    let printed = strip_ansi(printed);
    let (_, rest) = printed.split_once(BEGIN)?;
    let (path, _) = rest.split_once(END)?;
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Remove terminal escape sequences, so a themed prompt cannot corrupt the
/// answer or the markers that delimit it.
///
/// `oh-my-zsh` and `powerlevel10k` are the named culprits: a competitor added
/// exactly this guard after hitting it in the field. An interactive shell that
/// mis-detects its missing terminal writes colour codes to stdout, and they can
/// land adjacent to — or inside — a marker, at which point the marker no longer
/// matches and the launch silently keeps its stub `PATH`.
///
/// Stderr is already discarded, and `Stdio::null()` on stdin makes most themes
/// disable themselves, so this is the third layer rather than the first.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // BEL on its own is noise; as an OSC terminator it is consumed below.
            '\u{7}' => {}
            '\u{1b}' => match chars.next() {
                // CSI: parameter and intermediate bytes, then a final byte in
                // `@`..=`~`. Colour codes are these.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: runs until BEL or ST (`ESC \`). Title-setting lives here.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Any other escape is two characters wide; drop it whole.
                _ => {}
            },
            _ => out.push(c),
        }
    }
    out
}

/// Capture a command's stdout, killing it if it does not finish in `timeout`.
///
/// The exit status is deliberately ignored: an `rc` file whose last line fails
/// makes an otherwise perfectly good shell exit non-zero, and the markers are a
/// stricter contract than the status anyway — either the answer is in the
/// output or it is not.
fn run_with_timeout(program: &str, args: &[String], timeout: Duration) -> Option<String> {
    use oximux_no_window::NoWindow as _;
    let mut child = Command::new(program)
        .args(args)
        // An `rc` file that reads from stdin blocks until the timeout
        // otherwise; at EOF it gives up at once.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Interactive shells complain about not having a terminal, and a
        // version manager may narrate. None of it is this module's business.
        .stderr(Stdio::null())
        // A hook for users whose `rc` file is slow on purpose: guarding the
        // expensive half of it on this variable makes the probe cheap without
        // changing what their terminal does. Borrowed from a competitor that
        // ships the same escape hatch.
        .env("OXIMUX_SHELL_PATH_PROBE", "1")
        .no_window()
        .spawn()
        .ok()?;

    // Only the pipe crosses into the reader thread, so the `Child` stays here
    // and can still be killed once the deadline passes.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        // Bytes, not `read_to_string`: that returns `Err` and discards
        // everything on invalid UTF-8, and a prompt theme can emit anything.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Stop at the closing marker rather than at end-of-file.
                    // EOF needs *every* copy of the write end closed, and a
                    // shell whose `rc` file starts a daemon — a prompt theme's
                    // status worker is the common one — hands that daemon the
                    // same pipe. Waiting for EOF there means discarding an
                    // answer we already hold, burning the whole budget, and
                    // keeping the stub `PATH`: the exact failure this module
                    // exists to prevent, reached the long way round.
                    if ends_with_marker(&buf) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });

    match rx.recv_timeout(timeout) {
        Ok(printed) => {
            // The answer is in hand. `kill` before `wait` because the reader
            // now returns on the marker, so the shell may not have exited yet —
            // and `wait` on a shell that lingers would reintroduce the block
            // the marker check just removed. Everything the script had to do
            // has already happened: printing the `PATH` is its last act.
            let _ = child.kill();
            let _ = child.wait();
            // Joined, not abandoned: the caller writes the environment a few
            // lines later, and that is only sound while no other thread could
            // be reading it. This one has already sent its answer, so the join
            // costs nothing and makes the `SAFETY` note above true rather than
            // nearly true.
            let _ = reader.join();
            Some(printed)
        }
        Err(_) => {
            tracing::warn!(program, "the shell did not print its PATH in time");
            let _ = child.kill();
            let _ = child.wait();
            // Not joined — it is parked in `read(2)` on a pipe a grandchild may
            // still hold open, so joining would reintroduce the hang the
            // timeout exists to stop. It touches no environment; see `SAFETY`.
            None
        }
    }
}

/// The user's `PATH`, then anything the process already had that it did not
/// list.
///
/// The user's entries come first so their own choice of which `claude` wins —
/// the same one their terminal would run. The inherited entries are kept rather
/// than replaced because the system directories must stay reachable even if a
/// login file rewrote `PATH` from scratch.
///
/// [`split_paths`] and [`join_paths`] rather than `split(':')`, because the
/// separator is `;` on Windows and this module is no longer `#[cfg(unix)]`.
/// They also drop the surrounding quotes Windows allows around an entry.
fn merged(user: &OsString, current: &OsString) -> Option<OsString> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for entry in split_paths(user).chain(split_paths(current)) {
        // An empty entry means "the current directory" to some tools, which is
        // not something to inherit into a process that spawns agent binaries.
        if entry.as_os_str().is_empty() || out.contains(&entry) {
            continue;
        }
        out.push(entry);
    }
    join_paths(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether this machine's login shell is one the probe scripts are written
    /// for, so the two live-probe tests below assert a property of the code
    /// rather than a property of whoever is running them.
    ///
    /// `nu`, `csh`, and `tcsh` are unsupported by design — the module says so —
    /// and a contributor whose login shell is one of them should not get a red
    /// suite for it. Verified on this machine that `sh`, `bash`, `zsh`, and
    /// `dash` all accept `-l -i -c`; `fish` is listed because it gets its own
    /// script, and is the one entry here not verified locally.
    #[cfg(unix)]
    fn the_login_shell_is_one_we_target() -> bool {
        let shell = oximux_shell_env::default_shell();
        let name = std::path::Path::new(&shell)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_owned();
        let targeted = matches!(name.as_str(), "zsh" | "bash" | "sh" | "dash" | "ksh" | "fish");
        if !targeted {
            eprintln!("skipping the live probe: {name} is not a shell this module targets");
        }
        targeted
    }

    /// Windows has one shell family here, and it ships with the OS.
    #[cfg(windows)]
    fn the_login_shell_is_one_we_target() -> bool {
        true
    }

    #[test]
    fn only_a_process_with_no_terminal_at_all_counts_as_a_gui_launch() {
        // The rule that decides whether any of this runs. A GUI launch has no
        // terminal on any descriptor — verified with `lsof` against a bundled
        // app, which holds `/dev/null` on all three.
        assert!(!looks_like_a_shell_launch(false, false, false));
        // Redirecting one descriptor (`oximux > log.txt`) does not turn a
        // terminal launch into a GUI launch, so any terminal at all counts.
        for (stdin, stdout, stderr) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            assert!(
                looks_like_a_shell_launch(stdin, stdout, stderr),
                "{stdin} {stdout} {stderr} is a shell launch",
            );
        }
    }

    /// The separator the platform's own `PATH` uses, so the merge tests state
    /// their inputs the way the OS running them would.
    const SEP: char = if cfg!(windows) { ';' } else { ':' };

    /// A `PATH` in the shape this platform writes them.
    fn path_of(entries: &[&str]) -> OsString {
        OsString::from(entries.join(&SEP.to_string()))
    }

    fn user_entries() -> [&'static str; 2] {
        if cfg!(windows) {
            [r"C:\Users\x\.bun\bin", r"C:\Program Files\nodejs"]
        } else {
            ["/opt/homebrew/bin", "/Users/x/.local/bin"]
        }
    }

    fn system_entries() -> [&'static str; 2] {
        if cfg!(windows) {
            [r"C:\Windows\system32", r"C:\Windows"]
        } else {
            ["/usr/bin", "/bin"]
        }
    }

    #[test]
    fn the_user_path_wins_over_the_inherited_one() {
        // Which `claude` runs has to match the one the user's terminal runs,
        // otherwise an agent behaves differently depending on how the app was
        // opened — a difference nothing in the UI would explain.
        let merged = merged(&path_of(&user_entries()), &path_of(&system_entries()))
            .expect("mergeable");
        let merged = merged.to_string_lossy().into_owned();
        assert!(merged.starts_with(user_entries()[0]), "{merged}");
    }

    #[test]
    fn inherited_directories_are_kept_even_if_the_shell_dropped_them() {
        // A login file that assigns rather than appends can leave the system
        // directories out entirely. Dropping them here would break unrelated
        // tools in order to fix agents.
        let merged = merged(&path_of(&user_entries()), &path_of(&system_entries()))
            .expect("mergeable");
        let merged = merged.to_string_lossy().into_owned();
        for dir in system_entries() {
            assert!(merged.split(SEP).any(|e| e == dir), "{dir} missing from {merged}");
        }
    }

    #[test]
    fn no_directory_is_listed_twice() {
        // A duplicate is harmless to resolution but makes `PATH` grow on every
        // merge, and it is the kind of thing that shows up in a bug report.
        let shared = system_entries()[0];
        let merged = merged(&path_of(&[shared, user_entries()[0]]), &path_of(&system_entries()))
            .expect("mergeable");
        let merged = merged.to_string_lossy().into_owned();
        let mut seen: Vec<&str> = merged.split(SEP).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "{merged}");
    }

    #[test]
    fn empty_segments_are_dropped() {
        // A trailing separator means "the current directory" to some tools,
        // which is not something a process that spawns agent binaries should
        // inherit.
        let user = OsString::from(format!("{}{SEP}{SEP}", user_entries()[0]));
        let current = OsString::from(format!("{}{SEP}", system_entries()[0]));
        let merged = merged(&user, &current).expect("mergeable");
        assert!(!merged.to_string_lossy().split(SEP).any(str::is_empty), "{merged:?}");
    }

    #[test]
    fn the_path_is_read_from_between_the_markers() {
        // A shell that loads the user's config prints prompts and
        // version-manager notices around the answer; without the markers the
        // first entry would become the banner glued to a directory.
        let printed = format!("p10k: using instant prompt\n{BEGIN}/opt/homebrew/bin:/usr/bin{END}\n");
        assert_eq!(
            path_between_markers(&printed).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin"),
        );
    }

    #[test]
    fn output_without_both_markers_is_not_a_path() {
        // A shell killed mid-print, or one that died before it printed, must
        // read as "no answer" — never as a truncated `PATH` that gets adopted.
        assert!(path_between_markers("").is_none());
        assert!(path_between_markers(&format!("{BEGIN}/usr/bin")).is_none());
        assert!(path_between_markers(&format!("/usr/bin{END}")).is_none());
        // Markers present with nothing between them is also no answer.
        assert!(path_between_markers(&format!("{BEGIN}   {END}")).is_none());
    }

    /// The probe list must name a program that exists, or every launch silently
    /// keeps the stub `PATH`. Running the real first probe is the only way to
    /// know; it is also an end-to-end test of the script, the markers, and the
    /// pipe on whichever platform the suite runs.
    #[test]
    fn the_first_probe_answers_on_this_platform() {
        if !the_login_shell_is_one_we_target() {
            return;
        }
        let probe = probes().into_iter().next().expect("at least one probe");
        let got = probe.run(SHELL_TIMEOUT).expect("the user's shell must print a PATH");
        let got = got.to_string_lossy().into_owned();
        assert!(split_paths(&got).next().is_some(), "{got}");
    }

    #[test]
    fn every_probe_answers_on_this_platform() {
        if !the_login_shell_is_one_we_target() {
            return;
        }
        // The fallback exists for shells that reject the first probe's flags,
        // so it has to work on its own.
        for probe in probes() {
            let program = probe.program.clone();
            assert!(probe.run(SHELL_TIMEOUT).is_some(), "{program} printed no PATH");
        }
    }

    #[test]
    fn a_themed_prompt_cannot_corrupt_the_answer() {
        // A colour-coded prompt before the answer and an OSC title after it —
        // the shape `oh-my-zsh` and `powerlevel10k` emit when they mis-detect
        // a missing terminal and write to stdout anyway.
        let printed = format!(
            "\u{1b}[1;32m>\u{1b}[0m {BEGIN}/opt/homebrew/bin:/usr/bin{END}\u{1b}]0;a title\u{7}"
        );
        assert_eq!(
            path_between_markers(&printed).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin"),
        );
    }

    #[test]
    fn an_escape_spliced_into_a_marker_still_matches() {
        // The failure that makes stripping worth doing at all: escapes land
        // *inside* the marker, so a search of the raw text finds nothing and
        // the launch silently keeps its stub PATH.
        let (head, tail) = BEGIN.split_at(8);
        let printed = format!("{head}\u{1b}[0m{tail}/usr/bin{END}");
        assert_eq!(path_between_markers(&printed).as_deref(), Some("/usr/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn fish_is_asked_for_a_colon_joined_path() {
        // fish's `$PATH` is a list, so the POSIX quoted expansion yields a
        // space-separated string: an answer that parses, and is wrong.
        let fish = unix_script("/opt/homebrew/bin/fish");
        assert!(fish.contains("string join :"), "{fish}");
        let posix = unix_script("/bin/zsh");
        assert!(posix.contains("\"$PATH\""), "{posix}");
        assert!(!posix.contains("string join"), "{posix}");
    }

    /// The regression this guards is the one that makes the whole module
    /// pointless: a `~/.zshrc` that starts a daemon — a prompt theme's status
    /// worker is the usual one — hands it the same stdout pipe. The shell
    /// prints the answer and exits, but the pipe never reaches end-of-file, so
    /// a reader waiting for EOF discards a `PATH` it already has, spends the
    /// entire budget, and leaves the launch on its stub `PATH`.
    #[cfg(unix)]
    #[test]
    fn an_answer_arrives_even_when_a_background_child_holds_the_pipe_open() {
        let script = format!(
            "printf '%s%s%s' '{BEGIN}' '/opt/homebrew/bin:/usr/bin' '{END}'; sleep 30 &"
        );
        let args = vec!["-c".to_string(), script];
        let started = std::time::Instant::now();
        let got = run_with_timeout("/bin/sh", &args, SHELL_TIMEOUT)
            .and_then(|printed| path_between_markers(&printed));

        assert_eq!(got.as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        // Promptly, not eventually: the point is that it does not wait out the
        // budget the lingering child would otherwise consume.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "waited {:?} for an answer that had already been printed",
            started.elapsed(),
        );
    }

    #[test]
    fn the_end_marker_is_found_wherever_it_lands() {
        assert!(!ends_with_marker(b""));
        assert!(!ends_with_marker(BEGIN.as_bytes()));
        assert!(ends_with_marker(format!("{BEGIN}/usr/bin{END}").as_bytes()));
        // A theme that keeps printing after the marker must not hide it.
        assert!(ends_with_marker(format!("{END}\u{1b}[0m trailing").as_bytes()));
    }

    #[test]
    fn a_shell_that_never_answers_is_killed_rather_than_waited_on() {
        // The reason the `Child` is not moved into the reader thread: a boot
        // path cannot hang on an `rc` file that blocks.
        // `ping` rather than `cmd /c timeout`, for the same quoting reason the
        // Windows probe list avoids `cmd`: no argument here needs a space.
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            ("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()])
        } else {
            ("/bin/sh", vec!["-c".into(), "sleep 30".into()])
        };
        let started = std::time::Instant::now();
        assert!(run_with_timeout(program, &args, Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn a_program_that_does_not_exist_is_not_an_answer() {
        // Every probe is allowed to be absent — that is what the next one is
        // for — so a missing program must read as `None`, not panic.
        let args = vec!["-c".to_string(), "true".to_string()];
        assert!(
            run_with_timeout("oximux-this-shell-should-not-exist-xyz", &args, SHELL_TIMEOUT)
                .is_none()
        );
    }
}
