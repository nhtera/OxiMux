//! Locate an agent CLI's binary from a GUI-launched app.
//!
//! A macOS app launched from Finder does not inherit the shell's PATH, and
//! agent CLIs typically install under a version manager (nvm/volta/bun) or a
//! user bin dir that only an interactive shell's PATH carries. So a bare
//! `Command::new(bin)` fails for exactly the launch mode most users use.
//!
//! Order: an explicitly configured path wins; then PATH; then any
//! adapter-supplied well-known install dirs (cheap `stat`s — e.g. omp's
//! installer targets `~/.local/bin`, bun installs to `~/.bun/bin`); then a
//! login-shell probe, which is what recovers the version-manager case.
//!
//! Extracted from the Pi adapter (which resolved only `pi`) so the omp
//! adapter shares the login-shell machinery instead of forking it.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use super::launch_posture::CliVersion;

/// Upper bound on the login-shell probe. It runs the user's full rc chain on
/// the UI thread, so it needs a hard ceiling: better an actionable "not
/// found" than a frozen app behind someone's `.zshrc`.
const LOGIN_SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Resolve `bin`, preferring `configured`, then PATH, then `well_known_dirs`,
/// then a cached login-shell probe.
pub fn resolve_agent_binary(
    bin: &'static str,
    configured: Option<&str>,
    well_known_dirs: &[PathBuf],
) -> Result<PathBuf> {
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        if path.is_absolute() && !path.exists() {
            anyhow::bail!("configured {bin} binary not found: {p}");
        }
        return Ok(path);
    }
    if let Some(p) = which_on_path(bin) {
        return Ok(p);
    }
    for dir in well_known_dirs {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    // Cached per bin: the probe spawns a login shell, far too expensive to
    // repeat on every connect/respawn, and connect runs on the UI thread. The
    // answer only changes if the user reinstalls, which warrants a restart.
    static LOGIN_SHELL_CACHE: Mutex<Option<HashMap<&'static str, Option<PathBuf>>>> =
        Mutex::new(None);
    let cached = LOGIN_SHELL_CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(bin).cloned()));
    let probed = match cached {
        Some(v) => v,
        None => {
            let v = login_shell_which(bin);
            if let Ok(mut g) = LOGIN_SHELL_CACHE.lock() {
                g.get_or_insert_with(HashMap::new).insert(bin, v.clone());
            }
            v
        }
    };
    if let Some(p) = probed {
        return Ok(p);
    }
    anyhow::bail!(
        "could not find the `{bin}` binary. Install it, or set an absolute path in \
         Settings → Agents (a GUI launch does not inherit your shell's PATH, so a \
         version-manager install such as nvm is not visible by default)."
    )
}

/// First match for `bin` on the inherited PATH.
///
/// Synchronous on purpose — this runs inside `connect`, on the UI thread with
/// no runtime to await on. The `which` crate does not spawn; what it adds
/// over a hand-rolled walk is `PATHEXT` handling on Windows, where these CLIs
/// install as `.cmd` shims.
fn which_on_path(bin: &str) -> Option<PathBuf> {
    which::which(bin).ok()
}

/// The shell and argv that ask "where is `bin`?" with the user's own startup
/// files loaded — the whole point of the probe: the answer differs from a
/// plain PATH lookup exactly when a startup file edits PATH.
#[cfg(unix)]
fn login_probe_command(bin: &str) -> (String, Vec<String>) {
    (
        oximux_shell_env::default_shell(),
        vec!["-lc".to_string(), format!("command -v {bin}")],
    )
}

#[cfg(windows)]
fn login_probe_command(bin: &str) -> (String, Vec<String>) {
    (
        oximux_shell_env::default_shell(),
        vec![
            "-NoLogo".to_string(),
            "-Command".to_string(),
            // `-ErrorAction SilentlyContinue` so a missing command prints
            // nothing instead of an error record the caller would parse as a
            // path. Empty stdout is already the "not found" signal.
            format!("(Get-Command {bin} -ErrorAction SilentlyContinue).Source"),
        ],
    )
}

/// Ask a login shell where `bin` is — recovers installs a Finder-launched app
/// can't see. Bounded (see [`LOGIN_SHELL_PROBE_TIMEOUT`]).
fn login_shell_which(bin: &str) -> Option<PathBuf> {
    use oximux_no_window::NoWindow as _;
    let (shell, args) = login_probe_command(bin);
    let child = Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .no_window()
        .spawn()
        .ok()?;
    let out = bounded_stdout(child, LOGIN_SHELL_PROBE_TIMEOUT, &format!("login-shell probe for `{bin}`"))?;
    // The LAST non-empty line: an rc file that prints banners ("Now using node
    // v20.11.0") puts them on stdout ahead of `command -v`'s answer, and
    // treating the whole blob as one path fails for exactly the
    // version-manager users this fallback exists to serve.
    let last = out.lines().rev().find(|l| !l.trim().is_empty())?.trim();
    let p = PathBuf::from(last);
    is_executable(&p).then_some(p)
}

/// Wait for `child` to exit successfully within `timeout` and return its
/// stdout. A failure, a timeout or an error is `None`; a child that overstays
/// is killed rather than left to hang the (UI) thread that asked.
fn bounded_stdout(mut child: std::process::Child, timeout: Duration, what: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!("{what} exceeded {timeout:?}; treating it as not found");
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// Upper bound on a `--version` probe.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What `program --version` reports, or `None` when it cannot be run or read.
///
/// Cached by path and modification time rather than for the process: the
/// Claude CLI updates itself in place, so the binary can change under a
/// running app, and a stale answer would gate a spawn posture on a CLI that is
/// no longer the one installed.
pub fn cli_version(program: &Path) -> Option<CliVersion> {
    type Key = (PathBuf, Option<std::time::SystemTime>);
    static CACHE: Mutex<Option<HashMap<Key, Option<CliVersion>>>> = Mutex::new(None);
    let key: Key = (program.to_path_buf(), std::fs::metadata(program).and_then(|m| m.modified()).ok());
    if let Some(hit) = CACHE.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).copied())) {
        return hit;
    }
    use oximux_no_window::NoWindow as _;
    let version = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .no_window()
        .spawn()
        .ok()
        .and_then(|child| bounded_stdout(child, VERSION_PROBE_TIMEOUT, "`--version` probe"))
        .and_then(|out| CliVersion::parse(&out));
    if let Ok(mut g) = CACHE.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, version);
    }
    version
}

#[cfg(unix)]
pub(crate) fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn is_executable(p: &Path) -> bool {
    p.is_file()
}
