//! `oximux open <folder>` and `oximux <folder>`: open a folder as a project in
//! the OxiMux app.
//!
//! The folder goes to the desktop over the local control socket — not as
//! launch arguments, which a running app on macOS never receives. When nothing
//! answers there, the app is started and given a moment to come up. Which
//! project the folder belongs to (its git work tree, a linked worktree's main
//! repository) is the desktop's to decide; this side only makes the path
//! absolute and checks it is a folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oximux_remote_proto::proto::{OPEN_PROJECT_MIN_VERSION, Request, Response, RpcError};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, HostSelection, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// How long a freshly started app gets to start answering.
const LAUNCH_WAIT: Duration = Duration::from_secs(15);

/// The reply can wait on the desktop: up to its own window wait after a launch,
/// plus reading the folder's default branch.
const REPLY_FLOOR: Duration = Duration::from_secs(30);

/// `argv` with `open` inserted when its first argument past the global flags
/// names a folder rather than a command: path-like (`.`, `..`, `/…`, `./…`,
/// `../…`, `~`, `~/…`, a Windows drive), or an existing directory whose name is
/// no command's. Anything else — a command, an unknown flag, a mistyped
/// command — passes through untouched, so clap still answers it. `is_dir` is
/// the filesystem question, passed in so the rule can be tested without one.
pub fn with_bare_path_as_open(
    argv: Vec<OsString>,
    commands: &[String],
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<OsString> {
    let Some(at) = first_positional(&argv) else {
        return argv;
    };
    let text = argv[at].to_string_lossy();
    if commands.iter().any(|command| *command == text) {
        return argv;
    }
    let path_like = matches!(text.as_ref(), "." | ".." | "~")
        || ["/", "./", "../", "~/", ".\\", "..\\"].iter().any(|prefix| text.starts_with(prefix))
        || is_windows_drive_path(&text);
    if !path_like && !is_dir(Path::new(&argv[at])) {
        return argv;
    }
    let mut argv = argv;
    argv.insert(at, "open".into());
    argv
}

/// Where the first positional argument sits, past the global flags
/// (`--json`, and `--dir`/`--host`/`--timeout` with their values). `None` for
/// any other flag first: that is clap's to read.
fn first_positional(argv: &[OsString]) -> Option<usize> {
    const VALUED: [&str; 3] = ["--dir", "--host", "--timeout"];
    let mut at = 1;
    while let Some(arg) = argv.get(at) {
        let arg = arg.to_string_lossy();
        if arg == "--json" || VALUED.iter().any(|flag| arg.starts_with(&format!("{flag}="))) {
            at += 1;
        } else if VALUED.contains(&arg.as_ref()) {
            at += 2;
        } else if arg.starts_with('-') {
            return None;
        } else {
            return Some(at);
        }
    }
    None
}

fn is_windows_drive_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

/// Run the verb.
pub async fn run(path: &Path, timeout: u64, selection: HostSelection) -> Result<(Value, String), Failure> {
    let folder = folder(path)?;
    let client = connect_or_launch(selection, timeout).await?;
    client.require_version(OPEN_PROJECT_MIN_VERSION, "open")?;
    let request = Request::OpenProject { path: folder.to_string_lossy().into_owned() };
    match client.call_at_least(request, REPLY_FLOOR).await? {
        Response::ProjectOpened { project, worktree } => {
            let data = json!({ "opened": project.path, "name": project.name, "worktree": worktree });
            let mut human = format!("opened {} ({})", project.name, project.path);
            if let Some(worktree) = &worktree {
                human.push_str(&format!("\nworktree {worktree}"));
            }
            Ok((data, human))
        }
        Response::Error(RpcError::Unsupported) => Err(Failure::new(
            "unsupported",
            exit::ERROR,
            "this host has no windows to open a folder in (a headless `oximux serve`)",
        )
        .with_steps(["open the folder from the OxiMux app on that machine".into()])),
        Response::Error(err) => Err(rpc_failure(err)),
        other => Err(unexpected_reply("OpenProject", &other)),
    }
}

/// `path` made absolute (`~` expanded, against the current directory) and
/// resolved, if it is a folder.
fn folder(path: &Path) -> Result<PathBuf, Failure> {
    let text = path.to_string_lossy();
    let expanded = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => dirs::home_dir()
            .map(|home| home.join(rest.trim_start_matches(['/', '\\'])))
            .unwrap_or_else(|| path.to_path_buf()),
        _ => path.to_path_buf(),
    };
    match expanded.canonicalize() {
        Ok(resolved) if resolved.is_dir() => Ok(resolved),
        _ => Err(Failure::new("not_found", exit::ERROR, format!("{} is not a folder", path.display()))),
    }
}

/// Connect to the selected host, starting the app first when the selection is
/// this machine's default host and nothing answers there. Never for `--dir`:
/// the app would start on its own data directory, not the one named, and the
/// wait that follows could only time out.
async fn connect_or_launch(selection: HostSelection, timeout: u64) -> Result<Client, Failure> {
    let first = match Client::resolve_and_connect(selection.clone(), timeout).await {
        Ok(client) => return Ok(client),
        Err(failure) => failure,
    };
    if first.code != "unreachable" || selection.dir.is_some() || !selection.is_local()? {
        return Err(first);
    }
    launch_app()?;
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        match Client::connect(selection.dir.clone(), timeout).await {
            Ok(client) => return Ok(client),
            Err(failure) if failure.code == "unreachable" && Instant::now() < deadline => continue,
            Err(mut failure) => {
                if failure.code == "unreachable" {
                    // Started, yet silent: by far most likely the desktop's
                    // local access is off, which no amount of waiting fixes.
                    failure.next_steps = vec![
                        "turn on local CLI access in OxiMux (Settings → Remote), then try again".into(),
                    ];
                }
                return Err(failure);
            }
        }
    }
}

/// Start the desktop app: the one this CLI ships inside, else the installed one.
fn launch_app() -> Result<(), Failure> {
    let not_installed = || {
        Failure::new("unreachable", exit::UNREACHABLE, "the OxiMux app is not installed here")
            .with_steps(["install OxiMux, or start a host here with `oximux serve`".into()])
    };
    #[cfg(target_os = "macos")]
    {
        let mut open = std::process::Command::new("open");
        let this = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
        match this.as_deref().and_then(enclosing_bundle) {
            Some(bundle) => open.arg("-a").arg(bundle),
            None => open.args(["-b", "dev.nhtera.oximux"]),
        };
        match open.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
            Ok(status) if status.success() => Ok(()),
            _ => Err(not_installed()),
        }
    }
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|exe| windows_app_exe(&exe, dirs::data_local_dir().as_deref()))
            .ok_or_else(not_installed)?;
        std::process::Command::new(exe).spawn().map(|_| ()).map_err(|_| not_installed())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = not_installed;
        Err(Failure::new("unreachable", exit::UNREACHABLE, "no OxiMux host is running here")
            .with_steps(["start a host here with `oximux serve`".into()]))
    }
}

/// The `.app` that `exe` (resolved through links) lives in, when it is the CLI
/// the app ships.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().find(|dir| dir.extension().is_some_and(|ext| ext == "app")).map(Path::to_path_buf)
}

/// The app's `oximux.exe` for a CLI at `exe`: beside it in the app's install
/// (or one level up, from the install's `bin`), else the per-user default
/// install under `local_app_data`.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_app_exe(exe: &Path, local_app_data: Option<&Path>) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let install = if dir.file_name().is_some_and(|name| name == "bin") { dir.parent()? } else { dir };
    let beside = install.join("oximux.exe");
    if beside.is_file() && beside != exe {
        return Some(beside);
    }
    let default = local_app_data?.join("Programs").join("OxiMux").join("oximux.exe");
    default.is_file().then_some(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rewrite with `dirs` as the only existing folders.
    fn rewritten_with(args: &[&str], commands: &[&str], dirs: &[&str]) -> Vec<String> {
        let argv: Vec<OsString> = std::iter::once("oximux").chain(args.iter().copied()).map(OsString::from).collect();
        let commands: Vec<String> = commands.iter().map(|c| c.to_string()).collect();
        with_bare_path_as_open(argv, &commands, |p| dirs.iter().any(|d| Path::new(d) == p))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn rewritten(args: &[&str], commands: &[&str]) -> Vec<String> {
        rewritten_with(args, commands, &[])
    }

    #[test]
    fn a_path_like_first_argument_opens() {
        for path in [".", "..", "./x", "../x", "/abs", "~", "~/x", "C:\\x", "d:/x", ".\\x"] {
            assert_eq!(rewritten(&[path], &["term"]), ["oximux", "open", path], "{path}");
        }
    }

    #[test]
    fn commands_flags_and_typos_are_left_to_the_parser() {
        assert_eq!(rewritten(&["term", "ls"], &["term"]), ["oximux", "term", "ls"]);
        assert_eq!(rewritten(&["--verbose", "."], &["term"]), ["oximux", "--verbose", "."]);
        assert_eq!(rewritten(&["trem"], &["term"]), ["oximux", "trem"]);
        assert_eq!(rewritten(&[], &["term"]), ["oximux"]);
    }

    #[test]
    fn global_flags_before_the_folder_are_kept() {
        assert_eq!(rewritten(&["--json", "."], &["term"]), ["oximux", "--json", "open", "."]);
        assert_eq!(
            rewritten(&["--dir", "/d", "--timeout=5", "~/x"], &["term"]),
            ["oximux", "--dir", "/d", "--timeout=5", "open", "~/x"]
        );
        // The value of a valued flag is not the folder.
        assert_eq!(rewritten(&["--host", "."], &["term"]), ["oximux", "--host", "."]);
    }

    #[test]
    fn an_existing_folder_opens_unless_it_is_named_like_a_command() {
        assert_eq!(rewritten_with(&["myproj"], &["term"], &["myproj"]), ["oximux", "open", "myproj"]);
        // A real folder called `term` is still the command.
        assert_eq!(rewritten_with(&["term"], &["term"], &["term"]), ["oximux", "term"]);
    }

    #[test]
    fn the_bundled_cli_starts_the_app_it_ships_in() {
        let exe = Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-cli");
        assert_eq!(enclosing_bundle(exe), Some(PathBuf::from("/Applications/OxiMux.app")));
        assert_eq!(enclosing_bundle(Path::new("/opt/homebrew/Cellar/oximux/0.1.44/bin/oximux")), None);
    }

    #[test]
    fn a_windows_cli_starts_the_app_beside_it_else_the_default_install() {
        let root = tempfile::tempdir().expect("tempdir");
        let install = root.path().join("OxiMux");
        std::fs::create_dir_all(install.join("bin")).expect("mkdir");
        std::fs::write(install.join("oximux.exe"), "").expect("write");
        // The install's own copies: the app sits beside, or one level up.
        for exe in [install.join("oximux-cli.exe"), install.join("bin").join("oximux.exe")] {
            assert_eq!(windows_app_exe(&exe, None), Some(install.join("oximux.exe")));
        }
        // A separately installed CLI: the per-user default install.
        let local = root.path().join("Local");
        let default = local.join("Programs").join("OxiMux");
        std::fs::create_dir_all(&default).expect("mkdir");
        std::fs::write(default.join("oximux.exe"), "").expect("write");
        let elsewhere = root.path().join("tools").join("oximux.exe");
        assert_eq!(windows_app_exe(&elsewhere, Some(&local)), Some(default.join("oximux.exe")));
        assert_eq!(windows_app_exe(&elsewhere, None), None);
    }

    #[test]
    fn only_a_folder_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(folder(dir.path()).unwrap(), dir.path().canonicalize().unwrap());
        let file = dir.path().join("f");
        std::fs::write(&file, "").expect("write");
        assert_eq!(folder(&file).unwrap_err().code, "not_found");
        assert_eq!(folder(&dir.path().join("missing")).unwrap_err().code, "not_found");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(folder(Path::new("~")).unwrap(), home.canonicalize().unwrap());
        }
    }
}
