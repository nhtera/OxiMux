//! The stable file every installed agent hook runs, instead of an OxiMux binary.
//!
//! The hooks OxiMux installs live in the agents' own GLOBAL configuration, so
//! every `claude`, `codex`, `pi` and `omp` on the machine runs them, inside
//! OxiMux or not. Naming a binary there made whichever OxiMux wrote them last
//! the one all of those agents called — including a dev build under cargo's
//! `target/`. Such a binary cannot start outside cargo (its dylibs are found
//! through cargo's loader environment, not an rpath), and on a Mac whose exit
//! path is stuck each failed start wedged unkillable: 1490 of them in two days,
//! one per agent tool call, cleared only by a reboot.
//!
//! So hooks name this shim, at a path that never moves, and the shim picks the
//! binary when it runs: the one that wrote it unless that is a build output,
//! then the installed app, and a build output only when nothing else exists.
//! With no binary at all it drains the event and exits 0, so a moved or
//! deleted app never fails an agent's turn either. It is rewritten on every
//! app start (a no-op when unchanged), which is what keeps the list current.
//!
//! Unix only. Elsewhere the hooks keep naming the binary directly, as before.

use std::path::{Path, PathBuf};

/// Set to `1` to keep a build output first in the shim — for a developer
/// testing a change to the hook path itself, who accepts that the dev binary
/// must be able to start on its own (e.g. relinked with an rpath).
pub const USE_DEV_BUILD_ENV: &str = "OXIMUX_HOOKS_USE_DEV_BUILD";

/// The program an installed hook should run, for hooks written by `writer`
/// (the running binary): the shim, rewritten to list `writer`, or `writer`
/// itself where there is no shim (not Unix, no home dir, or a failed write —
/// logged, and no worse than before the shim existed).
///
/// Every caller that writes a hook command goes through this, so the global
/// install and the per-spawn `--settings` JSON stay byte-identical.
pub fn hook_program(writer: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        let dev_first = std::env::var(USE_DEV_BUILD_ENV).is_ok_and(|v| v == "1");
        if let Some(shim) = shim_path() {
            match install(&shim, &render(&candidates(writer, dev_first))) {
                Ok(()) => return shim,
                Err(err) => tracing::warn!(%err, shim = %shim.display(), "hook shim: write failed; hooks name the binary"),
            }
        }
    }
    writer.to_path_buf()
}

#[cfg(unix)]
/// `~/.oximux/agent-hooks/oximux-agent-status` — per user, outside any app
/// bundle or build tree, so nothing that replaces a binary replaces it.
fn shim_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oximux").join("agent-hooks").join("oximux-agent-status"))
}

#[cfg(unix)]
/// The shim's candidates as shell words, in the order it tries them.
fn candidates(writer: &Path, dev_first: bool) -> Vec<String> {
    let writer_word = sh_quote(&writer.display().to_string());
    let demote = is_build_output(writer) && !dev_first;
    let mut words = Vec::new();
    if !demote {
        words.push(writer_word.clone());
    }
    if cfg!(target_os = "macos") {
        words.push(sh_quote("/Applications/OxiMux.app/Contents/MacOS/oximux"));
        words.push("\"$HOME/Applications/OxiMux.app/Contents/MacOS/oximux\"".to_owned());
    } else {
        // Where `scripts/install-cli.sh` puts the CLI by default.
        words.push("\"$HOME/.local/bin/oximux\"".to_owned());
    }
    // Last resort, not dropped: on a machine with no installed app the build
    // output is the only OxiMux there is.
    if demote {
        words.push(writer_word);
    }
    words.dedup();
    words
}

#[cfg(unix)]
/// True for a binary cargo built in place — `…/target/debug/…`,
/// `…/target/release/…`, or the same under a `target/<triple>/`.
fn is_build_output(path: &Path) -> bool {
    let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    parts.iter().enumerate().any(|(i, part)| {
        *part == "target"
            && parts[i + 1..]
                .iter()
                .take(2)
                .any(|p| *p == "debug" || *p == "release")
    })
}

#[cfg(unix)]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn render(candidates: &[String]) -> String {
    format!(
        r#"#!/bin/sh
# Managed by OxiMux. Rewritten on every app start; edits are overwritten.
#
# Agent status hooks run this file, never an OxiMux binary: a hook names a path
# that must outlive every build, update and uninstall. It runs the first OxiMux
# found below, and always exits 0, so a status report can never fail the
# agent's turn. With none installed it reads the event and does nothing.
for oximux in {}; do
  if [ -x "$oximux" ]; then
    "$oximux" "$@"
    exit 0
  fi
done
cat >/dev/null
exit 0
"#,
        candidates.join(" ")
    )
}

/// Write the shim only when it differs, executable, via temp + rename: an
/// agent may run it at any moment and must never see half a file. The temp
/// name is unique per call — the app's boot sync, a CLI run and a per-spawn
/// `--settings` build can all write at once, and a shared temp file would let
/// one rename another's half-written copy into place.
#[cfg(unix)]
fn install(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 == 0o111);
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) && executable(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{seq}.oximux-tmp", std::process::id()));
    let written = std::fs::write(&tmp, contents)
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_build_output_is_recognised_under_any_target_layout() {
        assert!(is_build_output(Path::new("/r/OxiMux/target/debug/oximux")));
        assert!(is_build_output(Path::new("/r/OxiMux/target/release/oximux")));
        assert!(is_build_output(Path::new("/r/target/aarch64-apple-darwin/debug/oximux")));
        assert!(!is_build_output(Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")));
        assert!(!is_build_output(Path::new("/r/OxiMux/dist/OxiMux.app/Contents/MacOS/oximux")));
        assert!(!is_build_output(Path::new("/home/u/.local/bin/oximux")));
    }

    #[test]
    fn a_build_output_goes_last_so_an_installed_app_answers_first() {
        let dev = Path::new("/r/target/debug/oximux");
        let words = candidates(dev, false);
        assert_eq!(words.last().unwrap(), "'/r/target/debug/oximux'");
        if cfg!(target_os = "macos") {
            assert_eq!(words[0], "'/Applications/OxiMux.app/Contents/MacOS/oximux'");
        }
        // The escape hatch puts it back in front.
        assert_eq!(candidates(dev, true)[0], "'/r/target/debug/oximux'");
    }

    #[test]
    fn the_writing_binary_answers_first_when_it_is_not_a_build_output() {
        let cli = Path::new("/home/o'x/.local/bin/oximux");
        let words = candidates(cli, false);
        assert_eq!(words[0], r"'/home/o'\''x/.local/bin/oximux'", "quoted against injection");
        // The installed app writing itself is not listed twice.
        let app = Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux");
        let words = candidates(app, false);
        let listed = words.iter().filter(|w| w.starts_with("'/Applications/OxiMux.app")).count();
        assert_eq!(listed, 1);
    }

    /// The shim, run for real: the first executable candidate gets the hook's
    /// arguments and stdin, a missing one is skipped, and the shim exits 0
    /// even when the binary it ran did not.
    #[test]
    fn the_shim_runs_the_first_binary_it_finds_and_always_exits_zero() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-oximux");
        let seen = dir.path().join("seen");
        let seen_word = sh_quote(&seen.display().to_string());
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" > {seen_word}\ncat >> {seen_word}\nexit 3\n")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = dir.path().join("shim");
        let words = vec![sh_quote("/nonexistent/oximux"), sh_quote(&fake.display().to_string())];
        install(&shim, &render(&words)).unwrap();

        let run = |shim: &Path| {
            let mut child = Command::new(shim)
                .args(["agent-status", "--state", "idle"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"{\"k\":1}").unwrap();
            child.wait().unwrap()
        };
        assert!(run(&shim).success(), "the binary's exit 3 must not reach the agent");
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "agent-status --state idle\n{\"k\":1}");

        // Nothing installed: the event is drained and the hook still succeeds.
        let empty = dir.path().join("empty-shim");
        install(&empty, &render(&[sh_quote("/nonexistent/oximux")])).unwrap();
        assert!(run(&empty).success());
    }
}
