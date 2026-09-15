//! Filesystem watch for the file explorer: the signal that says a directory
//! the panel is showing changed on disk.
//!
//! Before this, `FileExplorer` had exactly two refresh triggers — regaining
//! window focus, and the toolbar's Refresh button. Neither fires for the case
//! that matters most here: an agent or a terminal *inside* the app writing
//! files while the window stays focused the whole time. The tree simply went
//! on showing the directory as it was when it was last expanded, and only a
//! manual refresh corrected it. The git poller is not a substitute — it sees
//! nothing outside git's view, and a folder project has no poller at all.
//!
//! **One recursive watch at the repo root, with `NoCache`.** The recursion is
//! deliberate and so is the cache choice. `notify_debouncer_full`'s default
//! `FileIdMap` builds itself by recursively `stat`-ing every file under each
//! watched root — on a repo with a populated `target/` that is hundreds of
//! thousands of syscalls and seconds of blocking I/O (it is what once froze
//! this app at first paint). `NoCache` does no walk at all: registration is
//! one `FSEventStreamCreate`, and the only thing given up is rename-cookie
//! stitching, which this consumer does not use — a rename reaches us as
//! whatever pair of events the platform emits, and both name a directory to
//! re-read, which is all we ask.
//!
//! Volume is handled by subtraction, not by watching less: a debounced batch
//! is mapped to the set of directories the panel actually has open, and
//! everything else — the whole of `target/`, `.git/` churn, a build touching
//! thousands of files in collapsed subtrees — resolves to nothing.
//!
//! Watching less is not on the table, which is worth saying because a peer
//! tool solves this differently. `notify` never calls
//! `FSEventStreamSetExclusionPaths`, so there is no way to tell the OS not to
//! send us `target/`; the filtering has to happen in this process either way.
//! Measured against a real watcher, that is affordable: 5000 files written
//! under `target/debug/deps` delivered 5008 events in 7 batches and cost
//! 3.3 ms of main-thread work in total — about 0.66 µs an event. The
//! alternative, a non-recursive watch per open directory, would refuse those
//! events at the daemon, but `notify`'s FSEvents backend stops and re-creates
//! its stream (and joins its run-loop thread) on every `watch`/`unwatch`, so
//! it would move the cost onto the main thread on every expand and collapse —
//! a hitch where the user is looking, to save microseconds where they are not.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use notify_debouncer_full::{
    DebounceEventResult, Debouncer, NoCache, new_debouncer_opt,
    notify::{Config, RecommendedWatcher, RecursiveMode},
};
use tokio::sync::mpsc;

use crate::shell::file_explorer::tree_state::should_include;

/// Debounce window for a filesystem burst. A single save can fire several
/// FSEvents, and a tool writing a directory of files fires one per file; one
/// re-read per burst is what the panel needs, and 200 ms is short enough that
/// the row appears while the user is still looking at the place it appeared.
const DEBOUNCE_MS: u64 = 200;

/// The debouncer type this module hands back. Held by the explorer for its
/// lifetime — dropping it stops the FSEvents stream, which is exactly what
/// should happen when the panel goes away with its project.
pub type ExplorerWatcher = Debouncer<RecommendedWatcher, NoCache>;

/// Start watching `root` recursively; debounced batches arrive on `tx`.
///
/// Returns `None` (having logged) when the watch cannot be established — a
/// path that is not a directory, or a platform refusing the stream. The panel
/// still works in that case: it keeps the focus-regain and manual refreshes it
/// always had. Live updates are an improvement on those, not a dependency.
pub fn spawn_watcher(
    root: &Path,
    tx: mpsc::UnboundedSender<DebounceEventResult>,
) -> Option<ExplorerWatcher> {
    let mut debouncer = match new_debouncer_opt::<_, RecommendedWatcher, NoCache>(
        std::time::Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
        NoCache,
        Config::default(),
    ) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                %err,
                "could not create the explorer watcher; live refresh off"
            );
            return None;
        }
    };
    if let Err(err) = debouncer.watch(root, RecursiveMode::Recursive) {
        tracing::warn!(
            target: "oximux_app::file_explorer",
            root = %root.display(),
            %err,
            "explorer watch failed; live refresh off"
        );
        return None;
    }
    Some(debouncer)
}

/// An event path respelled the way the panel spells its own paths.
///
/// The platform reports events under the *resolved* directory: notify
/// canonicalises a watch root before registering it, and macOS FSEvents
/// reports real paths regardless — a project under a symlinked home, a
/// checkout on another volume, or a `/var/…` temp dir (really
/// `/private/var/…`) all arrive spelled differently from the path the project
/// was opened with. Every path the panel holds — `repo_root`, the expanded
/// set, the cache keys — is that opening spelling. Without this, each event
/// would resolve to "no directory of mine" and the watch would be silently
/// inert: no error, no refresh, exactly the symptom it exists to fix.
///
/// Identity when the two roots agree, which is the common case; `None` for a
/// path under neither, which a watch rooted at one of them should not produce.
pub fn as_project_path(event: &Path, real_root: &Path, repo_root: &Path) -> Option<PathBuf> {
    if real_root == repo_root {
        return event.starts_with(repo_root).then(|| event.to_path_buf());
    }
    match event.strip_prefix(real_root) {
        Ok(rest) => Some(repo_root.join(rest)),
        // Already in the panel's spelling (a platform that does not resolve).
        Err(_) => event.starts_with(repo_root).then(|| event.to_path_buf()),
    }
}

/// Every directory whose listing may have changed, for one debounced batch of
/// `paths`.
///
/// **Both readings of an event path are offered, because the platform uses
/// both.** An event naming a file means the directory holding it changed, so
/// the parent is a candidate; an event naming a directory means something
/// inside it changed, so the directory itself is. FSEvents emits the first
/// form on this developer's machine and the second — the directory, with a
/// trailing slash — on GitHub's macOS runners, which is how the difference was
/// found: taking only the parent resolved a change in the watched root to the
/// root's *own parent*, so the watch did nothing at all there. Distinguishing
/// them properly would mean a `stat` per path, on a path that may already be
/// gone; offering both costs a `HashSet` lookup and cannot be wrong, since the
/// caller only ever acts on directories it has open or cached and a file path
/// is neither.
///
/// Paths under a name the explorer never shows (`.git`, `node_modules`,
/// `target`) are dropped, which is what keeps a running build from turning
/// into work here — measured at 5000 files written under `target/`: every
/// event is still *delivered* (`notify` sets no FSEvents exclusion paths, so
/// there is no way to refuse them), but they resolve to nothing in ~0.66 µs
/// each.
///
/// Whether a touched directory is re-read or merely marked suspect is the
/// caller's decision, because only it knows which directories are open and
/// which are merely cached — see `FileExplorer::apply_fs_events`.
///
/// Deduplicated and sorted: a burst names the same directory many times, and
/// the order a `HashSet` iterates in is not something a test should depend on.
pub fn touched_dirs(paths: impl IntoIterator<Item = PathBuf>, root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !is_inside_the_tree(&path, root) {
            continue;
        }
        // Normalised so a trailing slash cannot make one directory look like
        // two. `Path` compares and hashes by component, so this only tidies
        // what is stored and logged.
        let path: PathBuf = path.components().collect();
        if let Some(parent) = path.parent().filter(|p| p.starts_with(root)) {
            out.push(parent.to_path_buf());
        }
        out.push(path);
    }
    out.sort();
    out.dedup();
    out
}

/// True when `dir` is one the panel is currently showing the contents of: the
/// root, which is always listed, or a directory the user expanded.
pub fn is_open(dir: &Path, root: &Path, expanded: &HashSet<PathBuf>) -> bool {
    dir == root || expanded.contains(dir)
}

/// True when `path` is under `root` and no part of it *below* `root` is a
/// directory name the explorer never lists — so it could be a row.
///
/// The names come from `tree_state::should_include`, the same predicate the
/// directory loader filters entries with, so the two cannot drift into
/// disagreeing about what the tree contains. Only the part below `root` is
/// examined, because the project's own location is not the project's content:
/// a checkout at `~/target/app` or `~/build/site` is an ordinary place to keep
/// code, and testing the absolute path would match on the prefix and throw
/// every event in that project away — a watch that silently never fires.
fn is_inside_the_tree(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    rel.components().all(|c| {
        c.as_os_str()
            .to_str()
            .is_none_or(should_include)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// The directories a batch would actually re-read, with nothing expanded:
    /// what `apply_fs_events` keeps after the open-directory filter.
    fn open_dirs_touched(paths: &[PathBuf], root: &Path) -> Vec<PathBuf> {
        let none = HashSet::new();
        touched_dirs(paths.iter().cloned(), root)
            .into_iter()
            .filter(|d| is_open(d, root, &none))
            .collect()
    }

    /// What the panel would re-read for a batch: the touched directories it
    /// currently has open. Mirrors the first half of
    /// `FileExplorer::apply_fs_events`, so these tests read as the behaviour
    /// the user sees rather than as two functions composed.
    fn reloads(paths: &[&str], root: &str, expanded: &[&str]) -> Vec<PathBuf> {
        let root = Path::new(root);
        let expanded = set(expanded);
        touched_dirs(paths.iter().map(PathBuf::from), root)
            .into_iter()
            .filter(|d| is_open(d, root, &expanded))
            .collect()
    }

    /// The other half: a touched directory that is NOT open is not re-read,
    /// but the panel must be told its cached listing is now suspect.
    ///
    /// `cached` mirrors `invalidate_dir`'s own guard — it acts only on paths
    /// the cache holds, and the cache holds directories — which is what makes
    /// it safe for `touched_dirs` to offer a file path as a candidate.
    fn suspects(paths: &[&str], root: &str, expanded: &[&str], cached: &[&str]) -> Vec<PathBuf> {
        let root = Path::new(root);
        let expanded = set(expanded);
        let cached = set(cached);
        touched_dirs(paths.iter().map(PathBuf::from), root)
            .into_iter()
            .filter(|d| !is_open(d, root, &expanded) && cached.contains(d))
            .collect()
    }

    #[test]
    fn a_new_file_in_the_root_reloads_the_root() {
        assert_eq!(reloads(&["/r/README.md"], "/r", &[]), vec![PathBuf::from("/r")]);
    }

    #[test]
    fn a_new_file_in_an_expanded_dir_reloads_that_dir() {
        assert_eq!(
            reloads(&["/r/src/main.rs"], "/r", &["/r/src"]),
            vec![PathBuf::from("/r/src")]
        );
    }

    /// The subtraction that makes a recursive watch affordable: a collapsed
    /// directory is not being shown, so nothing is re-read for it now...
    #[test]
    fn a_file_in_a_collapsed_dir_reloads_nothing() {
        assert!(reloads(&["/r/src/deep/mod.rs"], "/r", &["/r/src"]).is_empty());
    }

    /// ...but it is exactly the case that used to rot: the panel caches a
    /// directory's children when it is expanded and keeps them after it is
    /// collapsed, so without this the file created while it was shut stays
    /// invisible (and a deleted one stays visible) the next time it opens.
    #[test]
    fn a_file_in_a_collapsed_dir_marks_it_suspect() {
        assert_eq!(
            suspects(&["/r/src/deep/mod.rs"], "/r", &["/r/src"], &["/r/src/deep"]),
            vec![PathBuf::from("/r/src/deep")]
        );
    }

    /// A file path is offered as a candidate but is never in the cache, so
    /// nothing acts on it.
    #[test]
    fn a_file_path_is_never_itself_invalidated() {
        assert!(
            suspects(&["/r/src/deep/mod.rs"], "/r", &["/r/src"], &[])
                .is_empty()
        );
    }

    /// A build writes thousands of paths under `target/`; none of them is a
    /// row, and the root must not be re-read because `target/` itself changed.
    #[test]
    fn build_output_and_git_internals_touch_nothing() {
        let paths = [
            "/r/target/debug/oximux",
            "/r/target",
            "/r/.git/index",
            "/r/node_modules/react/index.js",
        ];
        assert!(reloads(&paths, "/r", &["/r/src"]).is_empty());
        assert!(suspects(&paths, "/r", &["/r/src"], &["/r/target", "/r/.git"]).is_empty());
    }

    /// The form GitHub's macOS runners emit: the watched directory itself,
    /// with a trailing slash, instead of the file that changed inside it.
    /// Taking only the parent resolved this to the root's own parent and the
    /// watch silently did nothing.
    #[test]
    fn a_directory_granularity_event_reloads_that_directory() {
        assert_eq!(reloads(&["/r/"], "/r", &[]), vec![PathBuf::from("/r")]);
        assert_eq!(
            reloads(&["/r/src/"], "/r", &["/r/src"]),
            vec![PathBuf::from("/r"), PathBuf::from("/r/src")]
        );
    }

    /// A trailing slash must not make one directory look like two.
    #[test]
    fn a_trailing_slash_does_not_double_a_directory() {
        assert_eq!(
            reloads(&["/r/src/", "/r/src"], "/r", &["/r/src"]),
            vec![PathBuf::from("/r"), PathBuf::from("/r/src")]
        );
    }

    /// A project is allowed to live at a path that happens to contain one of
    /// the skipped names. Only what is *inside* the project decides.
    #[test]
    fn a_project_kept_under_a_dir_named_target_still_refreshes() {
        assert_eq!(
            reloads(
                &["/home/me/target/app/src/main.rs"],
                "/home/me/target/app",
                &["/home/me/target/app/src"]
            ),
            vec![PathBuf::from("/home/me/target/app/src")]
        );
    }

    #[test]
    fn a_burst_naming_one_dir_many_times_reloads_it_once() {
        assert_eq!(
            reloads(&["/r/src/a.rs", "/r/src/b.rs", "/r/src/c.rs"], "/r", &["/r/src"]),
            vec![PathBuf::from("/r/src")]
        );
    }

    #[test]
    fn several_open_dirs_each_reload_in_a_stable_order() {
        assert_eq!(
            reloads(
                &["/r/src/b.rs", "/r/docs/a.md", "/r/top.txt"],
                "/r",
                &["/r/src", "/r/docs"]
            ),
            vec![PathBuf::from("/r"), PathBuf::from("/r/docs"), PathBuf::from("/r/src")]
        );
    }

    /// A new directory appearing under an open one is a listing change for the
    /// parent — the panel shows the new folder collapsed, and does not (and
    /// cannot) read inside it until the user expands it.
    #[test]
    fn a_new_directory_reloads_its_parent_not_itself() {
        assert_eq!(
            reloads(&["/r/src/created"], "/r", &["/r/src"]),
            vec![PathBuf::from("/r/src")]
        );
    }

    #[test]
    fn a_path_outside_the_project_touches_nothing() {
        assert!(touched_dirs([PathBuf::from("/elsewhere/file.rs")], Path::new("/r")).is_empty());
    }

    /// The one thing the pure tests above cannot establish: that this watch,
    /// with `NoCache` and a recursive registration, actually delivers on this
    /// platform. Drives the whole path a new file takes — real directory, real
    /// watcher, real event — and ends where the panel starts, at the set of
    /// directories to re-read.
    ///
    /// Polls rather than sleeping a fixed span, and uses the tempdir's
    /// unresolved spelling on purpose: on macOS that is `/var/…` while events
    /// arrive under `/private/var/…`, so a regression in `as_project_path`
    /// fails here too.
    #[test]
    fn the_watch_delivers_a_created_file_as_a_directory_to_reload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let real_root = std::fs::canonicalize(&root).expect("canonicalize");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _watcher = spawn_watcher(&root, tx).expect("the watch must start");

        // The stream is registered asynchronously on its own run loop; give it
        // a beat before making the change it is supposed to see.
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(root.join("new-file.txt"), "hello\n").expect("write");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen: Vec<PathBuf> = Vec::new();
        while std::time::Instant::now() < deadline {
            while let Ok(Ok(events)) = rx.try_recv() {
                seen.extend(
                    events
                        .into_iter()
                        .flat_map(|e| e.event.paths)
                        .filter_map(|p| as_project_path(&p, &real_root, &root)),
                );
            }
            if !open_dirs_touched(&seen, &root).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Asserted through the caller's own filter: the platform may name the
        // created file or the directory holding it, and either must come out
        // as "re-read the root".
        assert_eq!(
            open_dirs_touched(&seen, &root),
            vec![root.clone()],
            "a file created in the watched root must resolve to re-reading it \
             (paths seen: {seen:?})"
        );
    }

    #[test]
    fn an_event_under_a_resolved_root_is_respelled_for_the_panel() {
        let got = as_project_path(
            Path::new("/private/var/t/repo/src/main.rs"),
            Path::new("/private/var/t/repo"),
            Path::new("/var/t/repo"),
        );
        assert_eq!(got, Some(PathBuf::from("/var/t/repo/src/main.rs")));
    }

    #[test]
    fn an_event_is_left_alone_when_the_roots_agree() {
        let got = as_project_path(
            Path::new("/r/src/main.rs"),
            Path::new("/r"),
            Path::new("/r"),
        );
        assert_eq!(got, Some(PathBuf::from("/r/src/main.rs")));
    }

    /// A platform that reports the unresolved spelling must not have its paths
    /// mangled just because the roots differ.
    #[test]
    fn an_event_already_in_the_panels_spelling_is_kept() {
        let got = as_project_path(
            Path::new("/var/t/repo/src/main.rs"),
            Path::new("/private/var/t/repo"),
            Path::new("/var/t/repo"),
        );
        assert_eq!(got, Some(PathBuf::from("/var/t/repo/src/main.rs")));
    }

    #[test]
    fn an_event_under_neither_root_is_dropped() {
        assert_eq!(
            as_project_path(
                Path::new("/elsewhere/x"),
                Path::new("/private/var/t/repo"),
                Path::new("/var/t/repo"),
            ),
            None
        );
    }

}

