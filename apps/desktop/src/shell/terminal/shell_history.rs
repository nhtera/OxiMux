//! App side of per-terminal shell history.
//!
//! The shells do the per-command work (see `oximux_shell_env::history`); the
//! app owns the directory and the moments the shells cannot see:
//! - **spawn**: every integrated zsh/bash/fish gets `OXIMUX_HISTORY_DIR`;
//! - **split / new tab in a pane**: the child's file is copied from the
//!   parent's before the child spawns, so it starts with the parent's history;
//! - **new top-level terminal** (new tab, script tab, floating tab): copied
//!   from the most recently focused terminal in the same worktree, so Up in
//!   worktree B never recalls worktree A's commands;
//! - **close**: the terminal's files are deleted (its commands are already in
//!   the user's own history file);
//! - **project removal / integration off / boot**: stale files are dropped.
//!
//! App quit deliberately does nothing: a quit keeps every terminal, and a
//! restored terminal (same `OXIMUX_TAB_ID`) reads its own file again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use oximux_settings::PerTerminalHistory;
use oximux_shell_env::history::{self, TabId};
use oximux_storage::SettingsRepo;

use super::terminal_view::{per_terminal_history, shell_integration_enabled};

/// Unreferenced history older than this is swept at boot. Long enough that a
/// terminal in a project left closed for weeks keeps its history; persisted
/// layouts are the real liveness signal, this is only the backstop.
const GC_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long after a close the files are deleted a second time. The relay
/// hangs up the shell and kills it 500 ms later; a shell writes its history
/// on SIGHUP, which would recreate the file the first delete removed.
const CLOSE_SETTLE: Duration = Duration::from_millis(2500);

/// `<app data>/shell-history`, made owner-only once per process. `None` when
/// there is no data dir or it cannot be closed to other accounts; terminals
/// then keep using the user's own history file, as before.
pub fn history_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = crate::terminal_settings::app_data_dir()?.join("shell-history");
        match oximux_owner_only::prepare_owner_only_dir(&dir) {
            Ok(()) => Some(dir),
            Err(err) => {
                tracing::warn!(?err, ?dir, "shell history dir unavailable; per-terminal history off");
                None
            }
        }
    })
    .as_deref()
}

/// Whether new terminals get their own history: shell integration on and
/// the history setting not `off`.
fn enabled() -> bool {
    shell_integration_enabled() && per_terminal_history() != PerTerminalHistory::Off
}

/// The history env for an integrated zsh/bash/fish spawn of terminal `tab`
/// under history `mode`. Under `off`, also drops any file a past run left, so
/// switching back on cannot bring back a history that went stale meanwhile.
pub(crate) fn spawn_env(mode: PerTerminalHistory, tab: Option<&str>) -> Vec<(String, String)> {
    if mode == PerTerminalHistory::Off
        && let Some(tab) = tab
    {
        forget(tab);
    }
    spawn_env_for(mode, history_dir())
}

/// Pure half of [`spawn_env`]. `auto` sets no mode, so a value inherited by
/// the app or set in the user's rc still decides; `off` also withholds the
/// directory, so no rc can turn it back on.
fn spawn_env_for(mode: PerTerminalHistory, dir: Option<&Path>) -> Vec<(String, String)> {
    let dir = if mode == PerTerminalHistory::Off { None } else { dir };
    let mut env = vec![env_pair_for(dir)];
    let value = match mode {
        PerTerminalHistory::Auto => return env,
        PerTerminalHistory::Always => "always",
        PerTerminalHistory::Off => "0",
    };
    env.push((history::OPT_OUT_ENV.to_string(), value.to_string()));
    env
}

/// `/`-separated on Windows, where the only consumer is Git Bash. Empty
/// without a dir rather than absent: an OxiMux started from an OxiMux pane
/// inherits that pane's value, which would point its shells at the other
/// app's directory.
fn env_pair_for(dir: Option<&Path>) -> (String, String) {
    let dir = dir.map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = if cfg!(windows) { dir.replace('\\', "/") } else { dir };
    (history::HISTORY_DIR_ENV.to_string(), dir)
}

/// Start `child_tab` with a copy of `parent_tab`'s history. Call before the
/// child spawns; a failure only costs the copy (the child then starts from
/// the user's own history).
pub fn inherit(parent_tab: &str, child_tab: &str) {
    if !enabled() {
        return;
    }
    let (Some(dir), Some(parent), Some(child)) =
        (history_dir(), TabId::parse(parent_tab), TabId::parse(child_tab))
    else {
        return;
    };
    if let Err(err) = history::inherit(dir, &parent, &child) {
        tracing::warn!(?err, parent_tab, child_tab, "shell history inherit failed");
    }
}

/// Start the new top-level terminal `tab`, spawning at `cwd`, from the
/// history of the most recently focused terminal in the same worktree. With
/// none there, it starts from the user's own history, as before.
pub fn seed_from_worktree(cwd: &Path, tab: &str) {
    if let Some(parent) = seed_parent_for(cwd) {
        inherit(&parent, tab);
    }
}

/// Delete `tab`'s history now.
pub fn forget(tab: &str) {
    lock_focused().notes.remove(tab);
    let (Some(dir), Some(id)) = (history_dir(), TabId::parse(tab)) else {
        return;
    };
    if let Err(err) = history::forget(dir, &id) {
        tracing::warn!(?err, tab, "shell history forget failed");
    }
}

/// Delete a closed terminal's history: now, and again once its shell is
/// surely gone. Blocks for [`CLOSE_SETTLE`]; call it off the main thread.
/// The first pass keeps the fish pointer, so the second can still find a
/// fish history file rewritten as the shell exited.
pub fn forget_after_close(tab: &str) {
    lock_focused().notes.remove(tab);
    let (Some(dir), Some(id)) = (history_dir(), TabId::parse(tab)) else {
        return;
    };
    if let Err(err) = history::forget_keeping_fish_pointer(dir, &id) {
        tracing::warn!(?err, tab, "shell history forget failed");
    }
    std::thread::sleep(CLOSE_SETTLE);
    forget(tab);
}

/// How to find a focused terminal's cwd later, at seed time, without
/// reading its view (reading another view inside a pane group's update
/// aborts GPUI). Every field is cheap to capture on each focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FocusNote {
    /// The shell's pid, when the backend knows it in memory.
    pub pid: Option<u32>,
    /// The relay's pty id: the daemon's checkpoint for it names the pid.
    pub relay_pty_id: Option<String>,
    /// The cwd the shell last reported (OSC 7).
    pub cwd_hint: Option<PathBuf>,
    /// Where the terminal was spawned: the last resort.
    pub spawn_cwd: PathBuf,
}

impl FocusNote {
    /// The terminal's cwd now: the shell's live cwd when its pid is known
    /// (it may have `cd`'d since it was focused), else what it last
    /// reported, else where it started.
    fn live_cwd(&self) -> PathBuf {
        let pid = self.pid.or_else(|| {
            let dir = crate::relay_cold_restore::default_checkpoints_dir()?;
            crate::relay_cold_restore::read_checkpoint_pid(&dir, self.relay_pty_id.as_deref()?)
        });
        pid.and_then(crate::shell::cwd_resolver::cwd_of_pid)
            .or_else(|| self.cwd_hint.clone())
            .unwrap_or_else(|| self.spawn_cwd.clone())
    }
}

/// Live terminals by tab id, each with the sequence number of its latest
/// focus. Plain data, so any spawn site can read it.
#[derive(Default)]
struct Focused {
    seq: u64,
    notes: HashMap<String, (u64, FocusNote)>,
}

static FOCUSED: LazyLock<Mutex<Focused>> = LazyLock::new(Mutex::default);

fn lock_focused() -> std::sync::MutexGuard<'static, Focused> {
    FOCUSED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Terminal `tab` just gained focus. Closing it ([`forget`] /
/// [`forget_after_close`]) drops the note.
pub fn note_focus(tab: &str, note: FocusNote) {
    let mut focused = lock_focused();
    focused.seq += 1;
    let seq = focused.seq;
    focused.notes.insert(tab.to_string(), (seq, note));
}

/// Terminal `tab` now runs another session (a restored placeholder attached
/// to its shell): update its note, if it has one, without counting as a focus.
pub fn refresh_focus(tab: &str, note: FocusNote) {
    if let Some((_, old)) = lock_focused().notes.get_mut(tab) {
        *old = note;
    }
}

/// The most recently focused live terminal in the worktree of `cwd`, if any.
pub fn seed_parent_for(cwd: &Path) -> Option<String> {
    if !enabled() {
        return None;
    }
    let dir = history_dir()?;
    let notes = lock_focused()
        .notes
        .iter()
        .map(|(tab, (seq, note))| (*seq, tab.clone(), note.clone()))
        .collect();
    // Resolved outside the lock: a live cwd can cost a checkpoint read.
    newest_in_scope(
        cwd,
        notes,
        |tab| TabId::parse(tab).is_some_and(|id| history::has_history(dir, &id)),
        FocusNote::live_cwd,
    )
}

/// How many of the most recently focused terminals with a history a seed
/// looks at. It runs on the main thread at spawn, and each candidate costs a
/// cwd lookup; one focused longer ago than this is a poor seed anyway.
const SEED_CANDIDATES: usize = 32;

/// Pure half of [`seed_parent_for`]: newest first, so the usual case
/// resolves one or two cwds. Terminals without a history of their own (an
/// agent CLI, a shell that stood down) are skipped before any cwd lookup.
fn newest_in_scope(
    cwd: &Path,
    mut notes: Vec<(u64, String, FocusNote)>,
    has_history: impl Fn(&str) -> bool,
    live_cwd: impl Fn(&FocusNote) -> PathBuf,
) -> Option<String> {
    let scope = Scope::of(cwd);
    notes.sort_unstable_by_key(|(seq, _, _)| std::cmp::Reverse(*seq));
    notes
        .into_iter()
        .filter(|(_, tab, _)| has_history(tab))
        .take(SEED_CANDIDATES)
        .find(|(_, _, note)| scope.contains(&live_cwd(note)))
        .map(|(_, tab, _)| tab)
}

/// The history a terminal at some path shares: its git worktree (the
/// nearest ancestor holding a `.git` entry, a directory in the main checkout
/// and a file in a linked worktree), else that plain directory and every
/// directory under it. Paths are compared resolved, since a shell reports
/// its cwd with symlinks resolved.
#[derive(Debug, PartialEq)]
enum Scope {
    Worktree(PathBuf),
    Dir(PathBuf),
}

impl Scope {
    fn of(path: &Path) -> Self {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match path.ancestors().find(|dir| dir.join(".git").exists()) {
            Some(root) => Scope::Worktree(root.to_path_buf()),
            None => Scope::Dir(path),
        }
    }

    fn contains(&self, cwd: &Path) -> bool {
        match (self, Scope::of(cwd)) {
            (Scope::Worktree(ours), Scope::Worktree(theirs)) => *ours == theirs,
            (Scope::Dir(ours), Scope::Dir(theirs)) => theirs.starts_with(ours),
            _ => false,
        }
    }
}

/// Delete the history of every terminal persisted for a removed project,
/// including windows of it never opened this session (no view, so no close).
pub fn forget_project(repo: &SettingsRepo, project_id: &str) {
    let prefix = crate::persisted_terminals::legacy_settings_key(project_id);
    let rows = match repo.list_prefixed(&prefix) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, project_id, "shell history: project layouts unreadable");
            return;
        }
    };
    let mut ids = HashSet::new();
    for (key, value) in rows {
        // `terminal_tabs:<id>` (legacy) or `terminal_tabs:<id>:<window>`; a
        // longer project id sharing the prefix is someone else's.
        let ours = key
            .strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'));
        if ours && let Ok(json) = serde_json::from_str::<serde_json::Value>(&value) {
            collect_tab_ids(&json, &mut ids);
        }
    }
    if ids.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for id in ids {
            forget(id.as_str());
        }
    });
}

/// Sweep history no persisted terminal refers to, on a background thread.
/// Fail-closed: any layout that cannot be read or parsed, or a layout set
/// aside as corrupt within the grace period, skips the sweep entirely, since
/// an incomplete reference set would delete live history.
pub fn spawn_boot_gc(repo: SettingsRepo) {
    let Some(dir) = history_dir() else {
        return;
    };
    std::thread::spawn(move || {
        if recently_rejected_layout() {
            tracing::info!("shell history gc skipped: a layout was set aside as corrupt");
            return;
        }
        let referenced = match referenced_tab_ids(&repo) {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(%err, "shell history gc skipped");
                return;
            }
        };
        match history::gc(dir, &referenced, GC_MAX_AGE, SystemTime::now()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "shell history gc: forgot closed terminals"),
            Err(err) => tracing::warn!(?err, "shell history gc failed"),
        }
    });
}

/// Every tab id in every persisted pane and floating layout.
fn referenced_tab_ids(repo: &SettingsRepo) -> Result<HashSet<TabId>, String> {
    let mut rows = Vec::new();
    for prefix in [
        crate::persisted_terminals::KEY_PREFIX,
        crate::shell::terminal::floating_terminal_persistence::TABS_KEY_PREFIX,
    ] {
        rows.extend(repo.list_prefixed(prefix).map_err(|err| format!("{prefix}: {err}"))?);
    }
    tab_ids_in(rows)
}

/// Pure half of [`referenced_tab_ids`]: one unparseable value fails all.
fn tab_ids_in(rows: Vec<(String, String)>) -> Result<HashSet<TabId>, String> {
    let mut ids = HashSet::new();
    for (key, value) in rows {
        let json: serde_json::Value =
            serde_json::from_str(&value).map_err(|err| format!("{key}: {err}"))?;
        collect_tab_ids(&json, &mut ids);
    }
    Ok(ids)
}

/// Every `"tab_id"` string anywhere in a layout. Walking the JSON rather
/// than the typed snapshot keeps this right as the layout schema grows (leaf
/// tabs, sub-panes, floating tabs all name their terminal the same way).
fn collect_tab_ids(value: &serde_json::Value, out: &mut HashSet<TabId>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "tab_id"
                    && let Some(id) = child.as_str().and_then(TabId::parse)
                {
                    out.insert(id);
                }
                collect_tab_ids(child, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_tab_ids(item, out)),
        _ => {}
    }
}

/// A `*.corrupt.json` preserved within the grace period: its terminals may
/// still be live on disk, but their ids are no longer in any layout.
fn recently_rejected_layout() -> bool {
    let Some(dir) = crate::restore_fallback::corrupt_layouts_dir() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().ends_with(".corrupt.json")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .map_or(true, |at| at.elapsed().unwrap_or_default() < GC_MAX_AGE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-2222-3333-4444-555555555555";
    const B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    #[test]
    fn tab_ids_are_found_at_any_depth_and_only_when_valid() {
        let layout = format!(
            r#"{{"tabs":[{{"sub_panes":[{{"tab_id":"{A}","tabs":[{{"tab_id":"{B}"}}]}}]}}],
                "tab_id":"tab-1","other":{{"tab_id":7}}}}"#
        );
        let ids = tab_ids_in(vec![("terminal_tabs:p:main".into(), layout)]).unwrap();
        let expected: HashSet<TabId> =
            [A, B].into_iter().map(|s| TabId::parse(s).unwrap()).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn one_unparseable_layout_fails_the_whole_set() {
        let rows = vec![
            ("terminal_tabs:p:main".to_string(), format!(r#"{{"tab_id":"{A}"}}"#)),
            ("terminal_tabs:q:main".to_string(), r#"{"tabs":[{"tab_id":"#.to_string()),
        ];
        assert!(tab_ids_in(rows).is_err(), "a truncated blob must skip the sweep");
    }

    /// `root/main` (a checkout), `root/linked` (a linked worktree),
    /// `root/plain` (no git), and `root/alias` -> `root/main`.
    fn layout() -> tempfile::TempDir {
        let root = tempfile::TempDir::new().unwrap();
        let p = root.path();
        std::fs::create_dir_all(p.join("main/.git")).unwrap();
        std::fs::create_dir_all(p.join("main/src/deep")).unwrap();
        std::fs::create_dir_all(p.join("linked/src")).unwrap();
        std::fs::write(p.join("linked/.git"), "gitdir: ../main/.git/worktrees/linked\n").unwrap();
        std::fs::create_dir_all(p.join("plain/sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(p.join("main"), p.join("alias")).unwrap();
        root
    }

    #[test]
    fn a_scope_is_the_worktree_else_the_plain_directory() {
        let root = layout();
        let p = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(Scope::of(&p.join("main")), Scope::Worktree(p.join("main")));
        assert_eq!(Scope::of(&p.join("main/src/deep")), Scope::Worktree(p.join("main")));
        assert_eq!(Scope::of(&p.join("linked/src")), Scope::Worktree(p.join("linked")));
        assert_eq!(Scope::of(&p.join("plain")), Scope::Dir(p.join("plain")));
        #[cfg(unix)]
        assert_eq!(Scope::of(&root.path().join("alias/src")), Scope::Worktree(p.join("main")));
        let plain = Scope::of(&p.join("plain"));
        assert!(plain.contains(&p.join("plain/sub")));
        assert!(!plain.contains(&p.join("main")), "a repo under a plain dir is its own scope");
        assert!(!Scope::of(&p.join("plain/sub")).contains(&p.join("plain")));
    }

    #[test]
    fn the_seed_is_the_newest_terminal_in_the_same_worktree() {
        let root = layout();
        let p = root.path();
        let note = |cwd: &str| FocusNote { spawn_cwd: p.join(cwd), ..FocusNote::default() };
        let notes = || {
            vec![
                (1, "main-old".to_string(), note("main")),
                (2, "main-new".to_string(), note("main/src/deep")),
                (3, "linked".to_string(), note("linked/src")),
                (4, "plain".to_string(), note("plain/sub")),
            ]
        };
        let pick = |cwd: &str, notes| {
            newest_in_scope(&p.join(cwd), notes, |tab| tab != "agent", |n: &FocusNote| n.spawn_cwd.clone())
        };
        assert_eq!(pick("main", notes()).as_deref(), Some("main-new"));
        // An agent tab focused last in the same worktree has no history to give.
        let mut with_agent = notes();
        with_agent.push((5, "agent".to_string(), note("main")));
        assert_eq!(pick("main", with_agent).as_deref(), Some("main-new"));
        assert_eq!(pick("linked", notes()).as_deref(), Some("linked"), "other worktrees are ignored");
        assert_eq!(pick("plain", notes()).as_deref(), Some("plain"));
        assert_eq!(pick("main", Vec::new()), None);
        // A closed (forgotten) terminal is simply absent from the notes.
        let without_newest = notes().into_iter().filter(|(_, t, _)| t != "main-new").collect();
        assert_eq!(pick("main", without_newest).as_deref(), Some("main-old"));
    }

    #[test]
    fn a_live_cwd_prefers_the_report_over_the_spawn_dir() {
        let note = FocusNote {
            cwd_hint: Some(PathBuf::from("/reported")),
            spawn_cwd: PathBuf::from("/spawned"),
            ..FocusNote::default()
        };
        assert_eq!(note.live_cwd(), PathBuf::from("/reported"));
    }

    #[test]
    fn a_refresh_updates_a_note_but_never_adds_or_reorders_one() {
        let (tab, unseen) = ("55555555-2222-3333-4444-555555555555", "66666666-2222-3333-4444-555555555555");
        note_focus(tab, FocusNote::default());
        let seq = lock_focused().notes[tab].0;
        let live = FocusNote { pid: Some(42), ..FocusNote::default() };
        refresh_focus(tab, live.clone());
        refresh_focus(unseen, live.clone());
        let focused = lock_focused();
        assert_eq!(focused.notes[tab], (seq, live));
        assert!(!focused.notes.contains_key(unseen), "a refresh is not a focus");
        drop(focused);
        forget(tab);
    }

    #[test]
    fn closing_a_terminal_drops_its_focus_note() {
        let tab = "33333333-2222-3333-4444-555555555555";
        note_focus(tab, FocusNote::default());
        assert!(lock_focused().notes.contains_key(tab));
        forget(tab);
        assert!(!lock_focused().notes.contains_key(tab));
    }

    #[test]
    fn the_history_dir_is_owner_only_and_exported_with_forward_slashes() {
        let dir = history_dir().expect("test data dir");
        assert!(oximux_owner_only::is_dir_restricted_to_owner(dir).unwrap());
        let (key, value) = env_pair_for(Some(dir));
        assert_eq!(key, "OXIMUX_HISTORY_DIR");
        assert!(!value.is_empty() && !value.contains('\\'));
    }

    #[test]
    fn an_off_spawn_drops_the_terminals_stale_history() {
        let tab = "44444444-2222-3333-4444-555555555555";
        let file = history_dir().expect("test data dir").join(format!("{tab}.zsh_history"));
        std::fs::write(&file, "stale\n").unwrap();
        spawn_env(PerTerminalHistory::Auto, Some(tab));
        assert!(file.exists(), "auto must keep the file");
        spawn_env(PerTerminalHistory::Off, Some(tab));
        assert!(!file.exists(), "off must drop it");
    }

    #[test]
    fn each_history_mode_sets_its_spawn_env() {
        let dir = Path::new("/data/shell-history");
        let env = |mode| spawn_env_for(mode, Some(dir));
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let dir_pair = pair("OXIMUX_HISTORY_DIR", "/data/shell-history");
        assert_eq!(env(PerTerminalHistory::Auto), vec![dir_pair.clone()]);
        assert_eq!(
            env(PerTerminalHistory::Always),
            vec![dir_pair, pair("OXIMUX_PER_TERMINAL_HISTORY", "always")]
        );
        assert_eq!(
            env(PerTerminalHistory::Off),
            vec![pair("OXIMUX_HISTORY_DIR", ""), pair("OXIMUX_PER_TERMINAL_HISTORY", "0")]
        );
    }

    #[test]
    fn no_history_dir_still_masks_an_inherited_one() {
        assert_eq!(env_pair_for(None), ("OXIMUX_HISTORY_DIR".to_string(), String::new()));
    }
}
