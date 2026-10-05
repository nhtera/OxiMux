//! Per-terminal shell history against real shells over a PTY.
//!
//! Each test drives an interactive zsh / bash / fish with the shipped history
//! block (`oximux_shell_env::history::scripts`) appended to a small rc, in a
//! sandbox `HOME` with a cleared environment, and reads the files the shell
//! wrote. Files are checked mid-session and after `kill -9`, never only after
//! a clean exit: a clean exit flushes history that a crash or reboot loses.
//!
//! A shell that is not installed skips its tests (fish is usually absent on
//! CI; Linux CI is the bash 5 leg, macOS the zsh 5.9 + bash 3.2 one).
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oximux_shell_env::history::{self, HistoryShell, TabId, scripts};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

const PROMPT: &str = "PTYTEST> ";
const WAIT: Duration = Duration::from_secs(15);
const TAB_A: &str = "11111111-2222-3333-4444-555555555555";
const TAB_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sh {
    Zsh,
    Bash,
    Fish,
}

impl Sh {
    fn program(self) -> Option<PathBuf> {
        let name = match self {
            Sh::Zsh => "zsh",
            Sh::Bash => "bash",
            Sh::Fish => "fish",
        };
        let path = std::env::var("PATH").unwrap_or_default();
        path.split(':')
            .chain(["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"])
            .map(|dir| Path::new(dir).join(name))
            .find(|p| p.is_file())
    }
}

/// A throwaway HOME + history dir. pid + counter so parallel tests never share.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "oximux-hist-pty-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "hist dir", "home/.config/fish", "home/.local/share/fish"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { root }
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    /// With a space, like the real `~/Library/Application Support/...`.
    fn hist_dir(&self) -> PathBuf {
        self.root.join("hist dir")
    }
    /// The user's own history file.
    fn global(&self, sh: Sh) -> PathBuf {
        match sh {
            Sh::Zsh => self.home().join(".zsh_history"),
            Sh::Bash => self.home().join(".bash_history"),
            Sh::Fish => self.home().join(".local/share/fish/fish_history"),
        }
    }
    /// The terminal's own history file.
    fn tab(&self, sh: Sh, tab: &str) -> PathBuf {
        let id = TabId::parse(tab).unwrap();
        match sh {
            Sh::Zsh => history::tab_file(&self.hist_dir(), &id, HistoryShell::Zsh),
            Sh::Bash => history::tab_file(&self.hist_dir(), &id, HistoryShell::Bash),
            Sh::Fish => self
                .home()
                .join(format!(".local/share/fish/oximux_{}_history", tab.replace('-', "_"))),
        }
    }
    fn read(&self, path: &Path) -> String {
        std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    }
    /// Seed the user's history with `lines` in the shell's own format.
    fn seed_global(&self, sh: Sh, lines: &[&str]) {
        let text: String = lines
            .iter()
            .map(|l| match sh {
                Sh::Zsh => format!(": 1700000000:0;{l}\n"),
                Sh::Bash => format!("{l}\n"),
                Sh::Fish => format!("- cmd: {l}\n  when: 1700000000\n"),
            })
            .collect();
        std::fs::write(self.global(sh), text).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A test copy of the app's bash OSC 133 hooks (`BASH_RCFILE`), so the bash
/// tests run the history flush beside them exactly as the app does. The
/// app's own unit test pins the `__oximux_*` trap guard this relies on.
const BASH_OSC_HOOKS: &str = r#"
__oximux_precmd() {
  local __oximux_status=$?
  if [[ -n "${__oximux_in_command:-}" ]]; then
    printf '\033]133;D;%s\007' "$__oximux_status"
    unset __oximux_in_command
  fi
  printf '\033]133;A\007'
}
__oximux_preexec() {
  [[ -n "${COMP_LINE:-}" ]] && return
  [[ -n "${__oximux_in_command:-}" ]] && return
  [[ "$BASH_COMMAND" == __oximux_* ]] && return
  declare -F __oximux_hist_flush >/dev/null && __oximux_hist_flush
  printf '\033]133;C\007'
  __oximux_in_command=1
}
PROMPT_COMMAND="__oximux_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
trap '__oximux_preexec' DEBUG
"#;

struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    out: Arc<Mutex<Vec<u8>>>,
    prompts: usize,
}

/// What a test's shell gets beyond the defaults.
#[derive(Default)]
struct Opts<'a> {
    /// The user's rc, before the history block.
    rc: &'a str,
    env: &'a [(&'a str, &'a str)],
    /// Leave the history block out (the OSC parity baseline).
    no_block: bool,
}

impl Session {
    fn start(sh: Sh, sb: &Sandbox, tab: &str, opts: Opts) -> Option<Session> {
        let program = sh.program()?;
        let mut cmd = CommandBuilder::new(&program);
        cmd.env_clear();
        let home = sb.home();
        let lang = if cfg!(target_os = "macos") { "en_US.UTF-8" } else { "C.UTF-8" };
        for (k, v) in [
            ("HOME", home.to_str().unwrap()),
            ("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"),
            ("TERM", "xterm"),
            ("LANG", lang),
            (history::HISTORY_DIR_ENV, sb.hist_dir().to_str().unwrap()),
            (history::TAB_ID_ENV, tab),
        ] {
            cmd.env(k, v);
        }
        for (k, v) in opts.env {
            cmd.env(k, v);
        }
        cmd.cwd(&home);
        let block = |b: &'static str| if opts.no_block { "" } else { b };
        match sh {
            Sh::Zsh => {
                // The app overlay's order: user rc, then per-command saving
                // (unless the user chose a mode it conflicts with), then the
                // history block.
                let rc = format!(
                    "HISTFILE=\"$HOME/.zsh_history\"\nHISTSIZE=2000\nSAVEHIST=1000\n\
                     PROMPT='{PROMPT}'\nunsetopt prompt_sp\n{}\n\
                     [[ ! -o share_history && ! -o inc_append_history_time ]] && setopt inc_append_history\n{}",
                    opts.rc,
                    block(scripts::ZSH_BLOCK)
                );
                std::fs::write(sb.root.join("home/.zshrc"), rc).unwrap();
                cmd.env("ZDOTDIR", home.to_str().unwrap());
                cmd.arg("-i");
            }
            Sh::Bash => {
                let rc = format!(
                    "PS1='{PROMPT}'\nHISTFILE=\"$HOME/.bash_history\"\n{}\n{BASH_OSC_HOOKS}\n{}",
                    opts.rc,
                    block(scripts::BASH_BLOCK)
                );
                let rcfile = sb.root.join("bashrc");
                std::fs::write(&rcfile, rc).unwrap();
                cmd.args(["--noprofile", "--rcfile", rcfile.to_str().unwrap(), "-i"]);
            }
            Sh::Fish => {
                let config = format!("function fish_prompt; echo -n '{PROMPT}'; end\n{}\n", opts.rc);
                std::fs::write(sb.root.join("home/.config/fish/config.fish"), config).unwrap();
                cmd.args(["-i", "-C", block(scripts::FISH_BLOCK)]);
            }
        }
        let pair = native_pty_system()
            .openpty(PtySize { rows: 40, cols: 200, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let out = Arc::new(Mutex::new(Vec::new()));
        let (out2, writer2) = (out.clone(), writer.clone());
        std::thread::spawn(move || {
            let _master = pair.master; // keep the PTY open while reading
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let chunk = &buf[..n];
                // fish 4 waits for a primary device-attributes reply.
                if chunk.windows(3).any(|w| w == b"[0c") || chunk.windows(3).any(|w| w == b"\x1b[c") {
                    let _ = writer2.lock().unwrap().write_all(b"\x1b[?62;22c");
                }
                out2.lock().unwrap().extend_from_slice(chunk);
            }
        });
        let mut session = Session { child, writer, out, prompts: 0 };
        session.wait_prompt();
        Some(session)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn wait_prompt(&mut self) {
        self.prompts += 1;
        let deadline = Instant::now() + WAIT;
        while self.text().matches(PROMPT).count() < self.prompts {
            assert!(Instant::now() < deadline, "no prompt #{}; output:\n{}", self.prompts, self.text());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Type `input` (raw bytes, e.g. an arrow key) and wait for the next
    /// prompt. Returns the output since the previous prompt.
    fn send(&mut self, input: &str) -> String {
        let before = self.text().len();
        self.writer.lock().unwrap().write_all(input.as_bytes()).unwrap();
        self.wait_prompt();
        self.text()[before..].to_string()
    }

    fn run(&mut self, line: &str) -> String {
        self.send(&format!("{line}\r"))
    }

    /// Start `line` without waiting for it to finish.
    fn start_line(&mut self, line: &str) {
        self.writer.lock().unwrap().write_all(format!("{line}\r").as_bytes()).unwrap();
    }

    /// Re-run the previous command via Up-arrow + Enter: what Up-arrow
    /// recalls is what runs.
    fn up_enter(&mut self) -> String {
        self.send("\x1b[A\r")
    }

    /// `kill -9` the shell: nothing it had not written yet survives.
    fn kill9(mut self) {
        if let Some(pid) = self.child.process_id() {
            let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = self.child.wait();
    }
}

/// Start `sh`, or skip the test when it is not installed.
macro_rules! start {
    ($sh:expr, $sb:expr, $tab:expr) => {
        start!($sh, $sb, $tab, Opts::default())
    };
    ($sh:expr, $sb:expr, $tab:expr, $opts:expr) => {
        match Session::start($sh, $sb, $tab, $opts) {
            Some(s) => s,
            None => {
                eprintln!("skipped: {:?} not installed", $sh);
                return;
            }
        }
    };
}

/// A command whose OUTPUT (not its echo) carries a unique token, in syntax
/// all three shells accept: it prints `T<n>`, while the typed line shows
/// `T%s`.
fn cmd(n: u32) -> String {
    format!("printf 'T%s\\n' {n}")
}
fn out(n: u32) -> String {
    format!("T{n}")
}

fn each_shell(test: impl Fn(Sh)) {
    for sh in [Sh::Zsh, Sh::Bash, Sh::Fish] {
        test(sh);
    }
}

#[test]
fn a_split_starts_from_its_parent_not_from_the_global_tail() {
    each_shell(|sh| {
        let sb = Sandbox::new("inherit");
        sb.seed_global(sh, &["echo global-tail"]);
        let Some(mut parent) = Session::start(sh, &sb, TAB_A, Opts::default()) else { return };
        parent.run(&cmd(100));
        // Another terminal runs something later: it is now the global tail.
        let mut other = Session::start(sh, &sb, TAB_B, Opts::default()).unwrap();
        other.run(&cmd(200));
        other.kill9();
        // Split from the parent: the app copies its history first.
        let child_tab = "cccccccc-2222-3333-4444-555555555555";
        history::inherit(&sb.hist_dir(), &TabId::parse(TAB_A).unwrap(), &TabId::parse(child_tab).unwrap())
            .unwrap();
        // No sleep: the app spawns the split right after copying.
        let mut child = Session::start(sh, &sb, child_tab, Opts::default()).unwrap();
        let recalled = child.up_enter();
        assert!(recalled.contains(&out(100)), "{sh:?}: split recalled {recalled:?}");
        assert!(!recalled.contains(&out(200)), "{sh:?}: split recalled the global tail");
        // ...then the two diverge.
        child.run(&cmd(300));
        assert!(!parent.up_enter().contains(&out(300)), "{sh:?}: parent sees the child's command");
        child.kill9();
        parent.kill9();
    });
}

#[test]
fn a_respawned_terminal_recalls_its_own_last_command() {
    each_shell(|sh| {
        let sb = Sandbox::new("respawn");
        let Some(mut a) = Session::start(sh, &sb, TAB_A, Opts::default()) else { return };
        let mut b = Session::start(sh, &sb, TAB_B, Opts::default()).unwrap();
        a.run(&cmd(1));
        b.run(&cmd(2));
        // Both die with the app (a reboot, a relay death).
        a.kill9();
        b.kill9();
        std::thread::sleep(Duration::from_millis(1100));
        let mut a = Session::start(sh, &sb, TAB_A, Opts::default()).unwrap();
        let mut b = Session::start(sh, &sb, TAB_B, Opts::default()).unwrap();
        assert!(a.up_enter().contains(&out(1)), "{sh:?}: A lost its history");
        assert!(b.up_enter().contains(&out(2)), "{sh:?}: B lost its history");
        a.kill9();
        b.kill9();
    });
}

#[test]
fn every_command_reaches_both_files_before_the_shell_dies() {
    each_shell(|sh| {
        let sb = Sandbox::new("both-files");
        let Some(mut s) = Session::start(sh, &sb, TAB_A, Opts::default()) else { return };
        s.run(&cmd(10));
        s.run(&cmd(11));
        // Mid-session, not after exit.
        // The line's tail, which every shell's file format keeps verbatim
        // (fish escapes the backslash earlier in it).
        let saved = |n: u32| format!("' {n}\n");
        for n in [10, 11] {
            assert!(sb.read(&sb.global(sh)).contains(&saved(n)), "{sh:?}: global lacks {n}");
            assert!(sb.read(&sb.tab(sh, TAB_A)).contains(&saved(n)), "{sh:?}: tab lacks {n}");
        }
        s.kill9();
        assert!(sb.read(&sb.global(sh)).contains(&saved(11)), "{sh:?}: lost on kill -9");
    });
}

#[test]
fn a_long_session_keeps_teeing_after_zsh_would_trim_its_file() {
    // zsh rewrites a history file that outgrows SAVEHIST by a fifth; the tee
    // must not lose lines when that would have happened.
    let sb = Sandbox::new("trim");
    let seed: Vec<String> = (0..3000).map(|i| format!("echo old{i}")).collect();
    sb.seed_global(Sh::Zsh, &seed.iter().map(String::as_str).collect::<Vec<_>>());
    let mut s = start!(Sh::Zsh, &sb, TAB_A, Opts { rc: "SAVEHIST=20", ..Opts::default() });
    for n in 0..40 {
        s.run(&cmd(n));
    }
    s.kill9();
    let global = sb.read(&sb.global(Sh::Zsh));
    let missing: Vec<u32> = (0..40).filter(|&n| !global.contains(&format!("{}\n", cmd(n)))).collect();
    assert!(missing.is_empty(), "lost from the global file: {missing:?}");
}

#[test]
fn user_filters_apply_to_both_files() {
    let cases: [(Sh, &str, &[&str]); 3] = [
        (Sh::Zsh, "setopt hist_ignore_dups\nzshaddhistory() { [[ $1 != secret* ]] }", &["secret one"]),
        (Sh::Bash, "HISTCONTROL=ignoreboth", &[" echo spaced"]),
        (Sh::Fish, "", &[" echo spaced"]),
    ];
    for (sh, rc, hidden) in cases {
        let sb = Sandbox::new("filters");
        let Some(mut s) = Session::start(sh, &sb, TAB_A, Opts { rc, ..Opts::default() }) else {
            continue;
        };
        s.run("echo dup");
        s.run("echo dup");
        for line in hidden {
            s.run(line);
        }
        s.run("echo kept");
        s.kill9();
        for file in [sb.global(sh), sb.tab(sh, TAB_A)] {
            let text = sb.read(&file);
            assert!(text.contains("echo kept"), "{sh:?} {file:?}: {text}");
            assert_eq!(text.matches("echo dup").count(), 1, "{sh:?} {file:?}: dups kept");
            for line in hidden {
                assert!(!text.contains(line.trim()), "{sh:?} {file:?}: kept {line:?}");
            }
        }
    }
}

#[test]
fn incognito_writes_nothing_anywhere() {
    for (sh, off) in [(Sh::Zsh, "unset HISTFILE"), (Sh::Bash, "unset HISTFILE"), (Sh::Bash, "set +o history")] {
        let sb = Sandbox::new("incognito");
        let Some(mut s) = Session::start(sh, &sb, TAB_A, Opts::default()) else { continue };
        s.run("echo before");
        s.run(off);
        s.run("echo hidden-one");
        s.run("echo hidden-two");
        s.kill9();
        for file in [sb.global(sh), sb.tab(sh, TAB_A)] {
            assert!(!sb.read(&file).contains("hidden"), "{sh:?} `{off}` leaked into {file:?}");
        }
        assert!(sb.read(&sb.global(sh)).contains("echo before"));
    }
}

#[test]
fn shared_or_opted_out_history_stays_on_the_users_file() {
    type Case<'a> = (Sh, &'a str, &'a [(&'a str, &'a str)]);
    let cases: [Case; 5] = [
        (Sh::Zsh, "setopt share_history", &[]),
        (Sh::Bash, "PROMPT_COMMAND='history -a; history -n'", &[]),
        (Sh::Zsh, "", &[(history::OPT_OUT_ENV, "0")]),
        (Sh::Bash, "", &[(history::OPT_OUT_ENV, "0")]),
        (Sh::Fish, "", &[(history::OPT_OUT_ENV, "0")]),
    ];
    for (sh, rc, env) in cases {
        let sb = Sandbox::new("shared");
        let Some(mut s) = Session::start(sh, &sb, TAB_A, Opts { rc, env, ..Opts::default() }) else {
            continue;
        };
        s.run(&cmd(5));
        s.kill9();
        assert!(!sb.tab(sh, TAB_A).exists(), "{sh:?} [{rc}] {env:?}: made a tab file");
    }
}

#[test]
fn share_history_turned_on_late_hands_history_back() {
    // A deferred plugin loader enables share_history after the rc returned.
    let sb = Sandbox::new("late-share");
    let mut s = start!(Sh::Zsh, &sb, TAB_A);
    s.run(&cmd(1));
    s.run("setopt share_history");
    s.run(&cmd(2));
    s.kill9();
    assert!(sb.read(&sb.global(Sh::Zsh)).contains(&cmd(2)));
    assert!(!sb.read(&sb.tab(Sh::Zsh, TAB_A)).contains(&cmd(2)), "still writing the tab file");
}

#[test]
fn an_invalid_tab_id_means_no_per_terminal_file() {
    each_shell(|sh| {
        let sb = Sandbox::new("bad-id");
        let Some(mut s) = Session::start(sh, &sb, "../escape", Opts::default()) else { return };
        s.run(&cmd(1));
        s.kill9();
        assert_eq!(std::fs::read_dir(sb.hist_dir()).unwrap().count(), 0, "{sh:?}");
        assert!(!sb.root.join("escape.zsh_history").exists());
    });
}

#[test]
fn utf8_commands_round_trip() {
    each_shell(|sh| {
        let sb = Sandbox::new("utf8");
        let Some(mut s) = Session::start(sh, &sb, TAB_A, Opts::default()) else { return };
        s.run("echo 'xin chào thế giới'");
        s.kill9();
        let global = std::fs::read(sb.global(sh)).unwrap();
        let tab = std::fs::read(sb.tab(sh, TAB_A)).unwrap();
        // zsh stores history metafied: compare the shell's own bytes, then
        // check the recall decodes.
        let line = |b: &[u8]| {
            b.split(|&c| c == b'\n')
                .rfind(|l| l.windows(3).any(|w| w == b"xin"))
                .map(<[u8]>::to_vec)
        };
        assert!(line(&global).is_some(), "{sh:?}: missing from global");
        assert_eq!(line(&global), line(&tab), "{sh:?}: the two files disagree");
        std::thread::sleep(Duration::from_millis(1100)); // fish hides same-second foreign items
        let mut s = Session::start(sh, &sb, TAB_A, Opts::default()).unwrap();
        let recalled = s.up_enter();
        assert!(recalled.contains("xin chào thế giới"), "{sh:?}: recall garbled: {recalled:?}");
        s.kill9();
    });
}

#[test]
fn bash_command_marks_are_unchanged_by_the_flush() {
    let marks = |no_block: bool| -> Option<Vec<String>> {
        let sb = Sandbox::new("osc");
        let mut s = Session::start(Sh::Bash, &sb, TAB_A, Opts { no_block, ..Opts::default() })?;
        let mut stream = String::new();
        stream += &s.run(""); // empty Enter
        stream += &s.send("\x03"); // Ctrl-C at the prompt
        stream += &s.run("true");
        stream += &s.run("false");
        s.kill9();
        Some(
            stream
                .split("\x1b]133;")
                .skip(1)
                .map(|m| m.split('\x07').next().unwrap_or_default().to_string())
                .collect(),
        )
    };
    let (Some(baseline), Some(with_block)) = (marks(true), marks(false)) else { return };
    assert_eq!(baseline, with_block);
    assert!(with_block.contains(&"D;1".to_string()), "{with_block:?}");
}

#[test]
fn bash_array_prompt_command_keeps_running_every_element() {
    let sb = Sandbox::new("pc-array");
    let rc = "PROMPT_COMMAND=('touch \"$HOME/pc0\"' 'touch \"$HOME/pc1\"')";
    let mut s = start!(Sh::Bash, &sb, TAB_A, Opts { rc, ..Opts::default() });
    let _ = std::fs::remove_file(sb.home().join("pc0"));
    let _ = std::fs::remove_file(sb.home().join("pc1"));
    s.run(&cmd(1));
    let bash5_1 = s.run("echo V$(( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) ))");
    s.kill9();
    assert!(sb.home().join("pc0").exists(), "the user's PROMPT_COMMAND stopped running");
    if bash5_1.contains("V1") {
        assert!(sb.home().join("pc1").exists(), "bash >= 5.1 lost an array element");
    }
    assert!(sb.read(&sb.global(Sh::Bash)).contains(&cmd(1)));
}

#[test]
fn zsh_tee_cost_does_not_grow_with_the_global_file() {
    let Some(zsh) = Sh::Zsh.program() else { return };
    let per_call = |lines: usize| -> f64 {
        let sb = Sandbox::new("latency");
        let global: String = (0..lines).map(|i| format!(": 1700000000:0;echo old{i}\n")).collect();
        std::fs::write(sb.global(Sh::Zsh), global).unwrap();
        let script = format!(
            "HISTFILE=$HOME/.zsh_history; SAVEHIST=1000; HISTSIZE=2000\n{}\n\
             zmodload zsh/datetime\n__oximux_hist_tee\nt0=$EPOCHREALTIME\n\
             for i in {{1..300}}; do print \": 1:0;echo new$i\" >> $HISTFILE; __oximux_hist_tee; done\n\
             print -- $(( (EPOCHREALTIME - t0) * 1000 / 300 ))",
            scripts::ZSH_BLOCK
        );
        let out = std::process::Command::new(zsh.clone())
            .args(["-f", "-c", &script])
            .env_clear()
            .env("HOME", sb.home())
            .env(history::HISTORY_DIR_ENV, sb.hist_dir())
            .env(history::TAB_ID_ENV, TAB_A)
            .output()
            .unwrap();
        let ms: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert_eq!(sb.read(&sb.global(Sh::Zsh)).matches("echo new").count(), 300);
        ms
    };
    let (small, large) = (per_call(10_000), per_call(100_000));
    // Generous absolute bound (shared CI runners); flatness is the claim.
    assert!(large < 10.0, "tee costs {large} ms per command");
    assert!(large < small * 3.0 + 0.2, "10k: {small} ms, 100k: {large} ms");
}

#[test]
fn zsh_options_in_the_users_rc_do_not_break_the_copy() {
    for rc in ["setopt sh_word_split", "setopt ksh_arrays", "setopt sh_word_split ksh_arrays"] {
        let sb = Sandbox::new("zsh-opts");
        let mut s = start!(Sh::Zsh, &sb, TAB_A, Opts { rc, ..Opts::default() });
        s.run(&cmd(41));
        s.run(&cmd(42));
        s.kill9();
        let global = sb.read(&sb.global(Sh::Zsh));
        assert!(global.contains("' 41\n") && global.contains("' 42\n"), "[{rc}] global: {global}");
    }
}

#[test]
fn zsh_resumes_copying_after_a_temporary_history() {
    // `fc -p` / `fc -P`: a pushed, private history, then back to this one.
    let sb = Sandbox::new("fc-p");
    let mut s = start!(Sh::Zsh, &sb, TAB_A);
    s.run(&cmd(1));
    s.run("fc -p");
    s.run("echo hidden-in-pushed");
    s.run("fc -P");
    s.run(&cmd(2));
    s.run(&cmd(3));
    s.kill9();
    let global = sb.read(&sb.global(Sh::Zsh));
    for n in [1, 2, 3] {
        assert!(global.contains(&format!("' {n}\n")), "global lacks {n}: {global}");
    }
    assert!(!global.contains("hidden-in-pushed"));
}

#[test]
fn fish_private_mode_turned_on_later_keeps_commands_out_of_both() {
    let sb = Sandbox::new("fish-private");
    let mut s = start!(Sh::Fish, &sb, TAB_A);
    s.run(&cmd(1));
    s.run("set -g fish_private_mode 1");
    s.run("echo secret-one");
    s.kill9();
    for file in [sb.global(Sh::Fish), sb.tab(Sh::Fish, TAB_A)] {
        assert!(!sb.read(&file).contains("secret-one"), "leaked into {file:?}");
    }
}

#[test]
fn bash_saves_a_command_when_it_starts_not_only_at_the_next_prompt() {
    // A pane closed while a long command runs never reaches the next prompt;
    // the command must already be in the user's history by then.
    let sb = Sandbox::new("bash-long");
    let mut s = start!(Sh::Bash, &sb, TAB_A);
    s.start_line("sleep 30 # longrun");
    let deadline = Instant::now() + WAIT;
    while !sb.read(&sb.global(Sh::Bash)).contains("longrun") {
        assert!(Instant::now() < deadline, "not saved at command start");
        std::thread::sleep(Duration::from_millis(20));
    }
    s.kill9();
}
