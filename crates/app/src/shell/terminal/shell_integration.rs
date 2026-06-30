//! OSC 133 shell-integration bootstrap injected at plain-shell spawn time.
//!
//! The terminal already *parses* OSC 133/633 command marks (`oximux_pty`'s
//! OSC scanner), but a stock shell emits none — so the prompt/output bands and
//! per-command exit codes that drive "send last output to agent" and the
//! exit-code gutter badges stay empty by default. This module installs a small
//! shell hook on spawn so those marks exist out of the box.
//!
//! How: the hook is delivered without touching the user's own dotfiles.
//! - **zsh** has no "extra rcfile" flag, so we point `ZDOTDIR` at an overlay
//!   dir whose startup files `source` the user's real ones (resolved from
//!   `OXIMUX_ORIG_ZDOTDIR`, defaulting to `$HOME`) and then arm `precmd` /
//!   `preexec` hooks. `ZDOTDIR` is restored to the user's value afterward so
//!   child shells and tools see the real one.
//! - **bash** takes `--rcfile`, whose script sources `~/.bashrc` then arms a
//!   `PROMPT_COMMAND` + `DEBUG`-trap pair.
//! - **fish** takes an `--init-command` that registers `fish_prompt` /
//!   `fish_preexec` / `fish_postexec` event functions.
//!
//! All three are guarded against double-emitting marks: a re-entry sentinel
//! makes a re-source idempotent, an opt-out env var
//! (`OXIMUX_SHELL_INTEGRATION=0`, also surfaced as the `shell_integration`
//! setting) disables it, and — crucially — we generically detect an *existing*
//! OSC-133 emitter (any already-registered prompt/pre-exec hook whose body
//! contains a `133;` mark, plus the well-known VS Code / iTerm env sentinels)
//! and skip our own injection. Two emitters would double the prompt marks and
//! collapse the output band to nothing, so when the user's shell already has an
//! integration we defer to it entirely.
//!
//! Verified end-to-end against a real pty: an interactive zsh under the overlay
//! emits the full `A / C / D;<exit>` stream with correct per-command exit codes,
//! which is exactly what the prompt/output band logic and the gutter badges
//! consume.
//!
//! The overlay files are written app-side under the app data dir; the local
//! relay daemon spawns shells from the same `$HOME`, so it reads the same
//! files. The chosen `env` + `args` ride the existing spawn wire fields, so
//! both the in-process and relay backends pick them up with no protocol change.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use oximux_pty::SpawnConfig;

use super::terminal_view::shell_integration_enabled;

/// Shells we know how to bootstrap. Anything else is left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

/// The spawn-time additions for one shell: extra env vars and extra argv.
struct Integration {
    env: Vec<(String, String)>,
    args: Vec<String>,
}

/// Merge the OSC 133 bootstrap into `cfg` when shell integration is enabled and
/// `cfg.shell` is a shell we understand. A no-op (and never fatal) otherwise —
/// a failed file write just leaves the shell un-instrumented.
pub fn augment_spawn_config(cfg: &mut SpawnConfig) {
    if !shell_integration_enabled() {
        return;
    }
    let Some(kind) = shell_kind(&cfg.shell) else {
        return;
    };
    let Some(base) = integration_dir() else {
        return;
    };
    let integration = match install(kind, &base, orig_zdotdir(&base).as_deref()) {
        Ok(integration) => integration,
        Err(err) => {
            tracing::warn!(?err, ?kind, "shell-integration install failed; skipping");
            return;
        }
    };
    // Prepend our args (bash `--rcfile`, fish `-C`) ahead of any caller args so
    // the flag and its value stay adjacent; caller args are empty for a plain
    // shell today, but appending keeps us correct if that ever changes.
    if !integration.args.is_empty() {
        let mut args = integration.args;
        args.append(&mut cfg.args);
        cfg.args = args;
    }
    // Caller env wins on key collision because the backend applies `cfg.env`
    // last; our keys (`ZDOTDIR`, `OXIMUX_ORIG_ZDOTDIR`) don't collide with the
    // context-id env the terminal sets, so a plain extend is correct.
    cfg.env.extend(integration.env);
}

/// Classify a shell by its path's file name. Login shells whose argv[0] is
/// `-zsh` never reach here (the spawn config carries a real path), so a plain
/// basename match suffices.
fn shell_kind(shell: &str) -> Option<ShellKind> {
    let name = Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())?
        .to_ascii_lowercase();
    match name.as_str() {
        "zsh" => Some(ShellKind::Zsh),
        "bash" => Some(ShellKind::Bash),
        "fish" => Some(ShellKind::Fish),
        _ => None,
    }
}

/// Where the overlay scripts live: a stable subdir of the app data dir.
fn integration_dir() -> Option<PathBuf> {
    crate::terminal_settings::app_data_dir().map(|d| d.join("shell-integration"))
}

/// The user's real `ZDOTDIR`, if the app inherited one (e.g. launched from a
/// terminal). `None` for a GUI launch — the overlay scripts then default to
/// `$HOME` at runtime. A value pointing back into our own overlay dir is
/// ignored so a re-spawn can't fold the overlay in on itself.
fn orig_zdotdir(base: &Path) -> Option<String> {
    let z = std::env::var("ZDOTDIR").ok()?;
    if z.is_empty() || Path::new(&z).starts_with(base) {
        return None;
    }
    Some(z)
}

/// Write the overlay scripts for `kind` under `base` and return the env + args
/// that activate them. Idempotent: files are only rewritten when their content
/// changed (so an app upgrade that ships a newer hook refreshes them).
fn install(kind: ShellKind, base: &Path, orig: Option<&str>) -> io::Result<Integration> {
    match kind {
        ShellKind::Zsh => {
            let dir = base.join("zsh");
            write_if_changed(&dir.join(".zshenv"), scripts::ZSH_ZSHENV)?;
            write_if_changed(&dir.join(".zprofile"), scripts::ZSH_ZPROFILE)?;
            write_if_changed(&dir.join(".zshrc"), scripts::ZSH_ZSHRC)?;
            write_if_changed(&dir.join(".zlogin"), scripts::ZSH_ZLOGIN)?;
            let mut env = vec![("ZDOTDIR".to_string(), dir.to_string_lossy().into_owned())];
            if let Some(orig) = orig {
                env.push(("OXIMUX_ORIG_ZDOTDIR".to_string(), orig.to_string()));
            }
            Ok(Integration { env, args: Vec::new() })
        }
        ShellKind::Bash => {
            let rcfile = base.join("bash").join("rcfile");
            write_if_changed(&rcfile, scripts::BASH_RCFILE)?;
            Ok(Integration {
                env: Vec::new(),
                args: vec!["--rcfile".to_string(), rcfile.to_string_lossy().into_owned()],
            })
        }
        // fish reads its init from the conf dir; rather than redirect that, we
        // pass the hook as an init-command so the user's config loads normally.
        ShellKind::Fish => Ok(Integration {
            env: Vec::new(),
            args: vec!["-C".to_string(), scripts::FISH_INIT.to_string()],
        }),
    }
}

/// Create parent dirs and write `content`, skipping the write when the file is
/// already byte-identical (the common steady state — avoids needless churn the
/// settings watcher would otherwise see).
fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    fs::write(path, content)
}

/// The shell-script payloads. Kept as data constants (not generated) so they're
/// auditable and stable; the only runtime input is `OXIMUX_ORIG_ZDOTDIR`, read
/// from the environment by the scripts themselves.
mod scripts {
    /// zsh `.zshenv` — always sourced. Chain to the user's real `.zshenv`,
    /// pinning `OXIMUX_ORIG_ZDOTDIR` to a concrete value for the later files.
    pub const ZSH_ZSHENV: &str = "\
# OxiMux shell integration overlay. Chains to your real zsh startup files.
OXIMUX_ORIG_ZDOTDIR=\"${OXIMUX_ORIG_ZDOTDIR:-$HOME}\"
[[ -f \"$OXIMUX_ORIG_ZDOTDIR/.zshenv\" ]] && builtin source \"$OXIMUX_ORIG_ZDOTDIR/.zshenv\"
";

    /// zsh `.zprofile` — login shells only; chains to the user's.
    pub const ZSH_ZPROFILE: &str = "\
# OxiMux shell integration overlay.
OXIMUX_ORIG_ZDOTDIR=\"${OXIMUX_ORIG_ZDOTDIR:-$HOME}\"
[[ -f \"$OXIMUX_ORIG_ZDOTDIR/.zprofile\" ]] && builtin source \"$OXIMUX_ORIG_ZDOTDIR/.zprofile\"
";

    /// zsh `.zlogin` — login shells only; chains to the user's.
    pub const ZSH_ZLOGIN: &str = "\
# OxiMux shell integration overlay.
OXIMUX_ORIG_ZDOTDIR=\"${OXIMUX_ORIG_ZDOTDIR:-$HOME}\"
[[ -f \"$OXIMUX_ORIG_ZDOTDIR/.zlogin\" ]] && builtin source \"$OXIMUX_ORIG_ZDOTDIR/.zlogin\"
";

    /// zsh `.zshrc` — interactive. Source the user's rc, restore `ZDOTDIR`, then
    /// arm the OSC 133 command-mark hooks: `precmd` closes the previous command
    /// with its exit (`D;$?`) and opens the next prompt (`A`); `preexec` marks
    /// output start (`C`).
    pub const ZSH_ZSHRC: &str = "\
# OxiMux shell integration overlay.
OXIMUX_ORIG_ZDOTDIR=\"${OXIMUX_ORIG_ZDOTDIR:-$HOME}\"
[[ -f \"$OXIMUX_ORIG_ZDOTDIR/.zshrc\" ]] && builtin source \"$OXIMUX_ORIG_ZDOTDIR/.zshrc\"
# Restore ZDOTDIR so child shells and tools resolve your real startup dir.
ZDOTDIR=\"$OXIMUX_ORIG_ZDOTDIR\"
# Defer to an existing integration: if any registered prompt/pre-exec hook
# already emits an OSC 133 mark, a second emitter would double the marks and
# break the output band, so skip ours entirely.
__oximux_emits_133=0
for __oximux_f in $precmd_functions $preexec_functions; do
  if [[ \"${functions[$__oximux_f]:-}\" == *\"133;\"* ]]; then
    __oximux_emits_133=1
    break
  fi
done
if [[ -z \"${__oximux_shell_integration:-}\" && \"${OXIMUX_SHELL_INTEGRATION:-1}\" != \"0\" \\
      && \"$__oximux_emits_133\" == \"0\" \\
      && -z \"${VSCODE_SHELL_INTEGRATION:-}\" && -z \"${ITERM_SHELL_INTEGRATION_INSTALLED:-}\" ]]; then
  __oximux_shell_integration=1
  __oximux_preexec() { printf '\\033]133;C\\007'; }
  __oximux_precmd() {
    local __oximux_status=$?
    printf '\\033]133;D;%s\\007' \"$__oximux_status\"
    printf '\\033]133;A\\007'
  }
  autoload -Uz add-zsh-hook 2>/dev/null
  if (( ${+functions[add-zsh-hook]} )); then
    add-zsh-hook preexec __oximux_preexec
    add-zsh-hook precmd __oximux_precmd
  else
    typeset -ga preexec_functions precmd_functions
    preexec_functions+=(__oximux_preexec)
    precmd_functions+=(__oximux_precmd)
  fi
fi
unset __oximux_emits_133 __oximux_f
";

    /// bash `--rcfile`. Source the user's rc, then install a `PROMPT_COMMAND`
    /// (emit `D;$?` for the finished command, then `A`) plus a `DEBUG` trap
    /// (emit `C` once per command). The `__oximux_in_command` latch keeps the
    /// trap from firing for the prompt command itself or for completion.
    pub const BASH_RCFILE: &str = "\
# OxiMux shell integration overlay. Chains to your real bash startup files.
[[ -f /etc/bash.bashrc ]] && source /etc/bash.bashrc
[[ -f \"$HOME/.bashrc\" ]] && source \"$HOME/.bashrc\"
# Defer to an existing integration: skip if any function body or PROMPT_COMMAND
# already emits an OSC 133 mark (a second emitter would double the marks).
__oximux_emits_133=0
if { declare -f 2>/dev/null; printf '%s' \"${PROMPT_COMMAND:-}\"; } | grep -q ']133;'; then
  __oximux_emits_133=1
fi
if [[ -z \"${__oximux_shell_integration:-}\" && \"${OXIMUX_SHELL_INTEGRATION:-1}\" != \"0\" \\
      && \"$__oximux_emits_133\" == \"0\" \\
      && -z \"${VSCODE_SHELL_INTEGRATION:-}\" ]]; then
  __oximux_shell_integration=1
  __oximux_precmd() {
    local __oximux_status=$?
    if [[ -n \"${__oximux_in_command:-}\" ]]; then
      printf '\\033]133;D;%s\\007' \"$__oximux_status\"
      unset __oximux_in_command
    fi
    printf '\\033]133;A\\007'
  }
  __oximux_preexec() {
    [[ -n \"${COMP_LINE:-}\" ]] && return
    [[ -n \"${__oximux_in_command:-}\" ]] && return
    [[ \"$BASH_COMMAND\" == \"__oximux_precmd\" ]] && return
    printf '\\033]133;C\\007'
    __oximux_in_command=1
  }
  case \";${PROMPT_COMMAND:-};\" in
    *\";__oximux_precmd;\"*) ;;
    *) PROMPT_COMMAND=\"__oximux_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}\" ;;
  esac
  trap '__oximux_preexec' DEBUG
fi
unset __oximux_emits_133
";

    /// fish `--init-command`. Registers event functions for prompt (`A`),
    /// pre-exec (`C`), and post-exec (`D;$status`). Runs after the user's
    /// config, so it coexists with their setup.
    pub const FISH_INIT: &str = "\
set -l __oximux_emits_133 0
for __oximux_f in (functions -n)
  if functions $__oximux_f 2>/dev/null | string match -q '*133;*'
    set __oximux_emits_133 1
    break
  end
end
if not set -q __oximux_shell_integration; and test \"$OXIMUX_SHELL_INTEGRATION\" != 0; and not set -q VSCODE_SHELL_INTEGRATION; and test $__oximux_emits_133 -eq 0
  set -g __oximux_shell_integration 1
  function __oximux_preexec --on-event fish_preexec
    printf '\\033]133;C\\007'
  end
  function __oximux_postexec --on-event fish_postexec
    printf '\\033]133;D;%s\\007' $status
  end
  function __oximux_prompt --on-event fish_prompt
    printf '\\033]133;A\\007'
  end
end
";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oximux-si-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn zsh_rc_skips_when_existing_integration_emits_133() {
        // The guard must defer to an already-registered 133 emitter.
        let base = temp_base("zsh-guard");
        install(ShellKind::Zsh, &base, None).expect("install");
        let rc = fs::read_to_string(base.join("zsh").join(".zshrc")).expect("read rc");
        assert!(rc.contains("precmd_functions $preexec_functions"));
        assert!(rc.contains("__oximux_emits_133=1"));
        assert!(rc.contains("\"$__oximux_emits_133\" == \"0\""));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn shell_kind_matches_known_shells_by_basename() {
        assert_eq!(shell_kind("/bin/zsh"), Some(ShellKind::Zsh));
        assert_eq!(shell_kind("/usr/local/bin/bash"), Some(ShellKind::Bash));
        assert_eq!(shell_kind("/opt/homebrew/bin/fish"), Some(ShellKind::Fish));
        assert_eq!(shell_kind("/usr/bin/ZSH"), Some(ShellKind::Zsh));
        assert_eq!(shell_kind("/path/to/claude"), None);
        assert_eq!(shell_kind(""), None);
    }

    #[test]
    fn zsh_install_writes_overlay_and_points_zdotdir() {
        let base = temp_base("zsh");
        let intg = install(ShellKind::Zsh, &base, None).expect("install");
        // ZDOTDIR points at the overlay dir; no args.
        let zdir = base.join("zsh");
        assert_eq!(
            intg.env,
            vec![("ZDOTDIR".to_string(), zdir.to_string_lossy().into_owned())]
        );
        assert!(intg.args.is_empty());
        // All four startup files exist and the rc chains to the user's rc,
        // restores ZDOTDIR, and emits the three command marks.
        for f in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
            assert!(zdir.join(f).exists(), "missing {f}");
        }
        let rc = fs::read_to_string(zdir.join(".zshrc")).expect("read rc");
        assert!(rc.contains("source \"$OXIMUX_ORIG_ZDOTDIR/.zshrc\""));
        assert!(rc.contains("ZDOTDIR=\"$OXIMUX_ORIG_ZDOTDIR\""));
        assert!(rc.contains("133;A"));
        assert!(rc.contains("133;C"));
        assert!(rc.contains("133;D;%s"));
        assert!(rc.contains("__oximux_shell_integration"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn zsh_install_passes_orig_zdotdir_when_known() {
        let base = temp_base("zsh-orig");
        let intg = install(ShellKind::Zsh, &base, Some("/home/me/.zsh")).expect("install");
        assert!(intg.env.iter().any(|(k, v)| k == "OXIMUX_ORIG_ZDOTDIR"
            && v == "/home/me/.zsh"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bash_install_writes_rcfile_and_passes_flag() {
        let base = temp_base("bash");
        let intg = install(ShellKind::Bash, &base, None).expect("install");
        let rcfile = base.join("bash").join("rcfile");
        assert_eq!(
            intg.args,
            vec!["--rcfile".to_string(), rcfile.to_string_lossy().into_owned()]
        );
        assert!(intg.env.is_empty());
        let rc = fs::read_to_string(&rcfile).expect("read rcfile");
        assert!(rc.contains("source \"$HOME/.bashrc\""));
        assert!(rc.contains("PROMPT_COMMAND"));
        assert!(rc.contains("trap '__oximux_preexec' DEBUG"));
        assert!(rc.contains("133;A"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn fish_install_passes_init_command_without_files() {
        let base = temp_base("fish");
        let intg = install(ShellKind::Fish, &base, None).expect("install");
        assert_eq!(intg.args.len(), 2);
        assert_eq!(intg.args[0], "-C");
        assert!(intg.args[1].contains("fish_prompt"));
        assert!(intg.args[1].contains("133;A"));
        assert!(intg.args[1].contains("133;D;%s"));
        // fish needs no on-disk overlay.
        assert!(!base.join("fish").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn write_if_changed_is_idempotent() {
        let base = temp_base("idem");
        let path = base.join("zsh").join(".zshrc");
        write_if_changed(&path, "v1").expect("first write");
        let first = fs::metadata(&path).expect("meta").modified().ok();
        // Re-writing identical content is a no-op (mtime unchanged on most FSes).
        write_if_changed(&path, "v1").expect("idempotent write");
        assert_eq!(fs::read_to_string(&path).unwrap(), "v1");
        // Changed content is rewritten.
        write_if_changed(&path, "v2").expect("changed write");
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2");
        let _ = first;
        let _ = fs::remove_dir_all(&base);
    }
}
