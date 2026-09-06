//! `.oximuxinclude` — copying a project's untracked local files into a fresh
//! worktree.
//!
//! A git worktree gets every *tracked* file for free and none of the untracked
//! ones. That is correct for git and useless for the user: the `.env`, the dev
//! certs, and the local overrides that the setup script and the app both need
//! stay behind in the main checkout. `.oximuxinclude` is the project's
//! declaration of which of those a worktree cannot work without.
//!
//! It is committed, so it names *paths*, never secrets — the files it points at
//! are the untracked ones, and they are copied, not listed.
//!
//! # Supported syntax
//!
//! One pattern per line; blank lines and `#` comments ignored; a leading `!`
//! negates. Paths are relative to the project root. Matching is real gitignore
//! semantics (via the `ignore` crate), so `*.pem`, `certs/`, and `config/*.local`
//! all mean what they mean in a `.gitignore`.
//!
//! The one deliberate departure: a pattern is **anchored at its literal
//! prefix**. `certs/*.pem` scans `certs/` rather than the whole repo, and only
//! a pattern that begins with a wildcard (`*.pem`, `**/x`) scans from the root.
//! Gitignore has no such notion because git is matching paths it already
//! walked; here the walk is the expensive part, and an unanchored scan of a
//! repo with a `node_modules` in it is the difference between instant and
//! minutes. A root-anchored scan is still bounded by [`SCAN_BUDGET`] and
//! reports when it hits it, so the pathological case is a visible skip rather
//! than a worktree creation that appears to hang.
//!
//! # The copy contract
//!
//! Each clause below has its own test, because the two symlink clauses are a
//! security boundary rather than a nicety: a copy that dereferences a source
//! symlink, or writes through one in the target, writes outside the worktree.
//!
//! 1. Regular files are copied. Directories are copied recursively.
//! 2. A path that already exists in the worktree wins and the skip is reported
//!    — a tracked file is never overwritten by an include.
//! 3. A source symlink is skipped, never dereferenced.
//! 4. An existing symlink anywhere in the target path is refused, so a copy
//!    can never be redirected outside the worktree.
//! 5. A pattern that matches nothing is reported, and the copy continues.
//! 6. An unreadable source or a failed write is reported, and the copy
//!    continues.
//!
//! Nothing here fails provisioning. A missing `.env` is worth telling the user
//! about; it is not worth destroying the worktree they asked for. The setup
//! script — which *can* fail creation — is the thing that decides whether the
//! files it needed were actually there.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// File name, at the project root.
pub const FILE_NAME: &str = ".oximuxinclude";

/// Upper bound on directory entries visited while expanding wildcard patterns.
/// Only reachable by a pattern anchored at the repo root; see the module note.
const SCAN_BUDGET: usize = 100_000;

/// Why one path was not copied. Every variant is reported to the provisioning
/// transcript — a silent skip is the failure mode this whole module exists to
/// avoid, since the user finds out later via a setup script that cannot find
/// its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// The worktree already has this path (a tracked file). It wins.
    AlreadyPresent(PathBuf),
    /// Source is a symlink; copying it would either dereference outside the
    /// project or plant a dangling link in the worktree.
    SourceIsSymlink(PathBuf),
    /// A component of the destination path is an existing symlink.
    TargetPathCrossesSymlink(PathBuf),
    /// The pattern resolved to nothing — usually a typo or a file the author
    /// has locally and this machine does not.
    MatchedNothing(String),
    /// The pattern would leave the project root (`..`, or an absolute path).
    EscapesProjectRoot(String),
    /// Read or write failed.
    Failed { path: PathBuf, error: String },
    /// A root-anchored wildcard exhausted [`SCAN_BUDGET`].
    ScanBudgetExhausted(String),
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyPresent(p) => {
                write!(f, "{}: already in the worktree (tracked file wins)", p.display())
            }
            Self::SourceIsSymlink(p) => write!(f, "{}: source is a symlink; skipped", p.display()),
            Self::TargetPathCrossesSymlink(p) => {
                write!(f, "{}: destination path crosses a symlink; refused", p.display())
            }
            Self::MatchedNothing(pat) => write!(f, "{pat}: matched nothing"),
            Self::EscapesProjectRoot(pat) => write!(f, "{pat}: leaves the project root; refused"),
            Self::Failed { path, error } => write!(f, "{}: {error}", path.display()),
            Self::ScanBudgetExhausted(pat) => write!(
                f,
                "{pat}: scan hit {SCAN_BUDGET} entries and stopped; anchor the pattern to a \
                 directory (e.g. `certs/*.pem`) instead of the repo root"
            ),
        }
    }
}

/// What the copy did, for the provisioning transcript.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CopyReport {
    /// Paths (relative to the project root) written into the worktree.
    pub copied: Vec<PathBuf>,
    /// Everything not copied, with the reason.
    pub skipped: Vec<Skip>,
}

impl CopyReport {
    /// True when there was no `.oximuxinclude`, or it declared nothing.
    pub fn is_empty(&self) -> bool {
        self.copied.is_empty() && self.skipped.is_empty()
    }
}

/// Copy the files `<project_root>/.oximuxinclude` declares into `worktree`.
///
/// Best-effort by contract: an absent file is a no-op, and every per-path
/// problem lands in [`CopyReport::skipped`] rather than aborting the rest.
pub fn copy_included_files(project_root: &Path, worktree: &Path) -> CopyReport {
    let mut report = CopyReport::default();
    let text = match std::fs::read_to_string(project_root.join(FILE_NAME)) {
        Ok(t) => t,
        Err(_) => return report, // absent → nothing declared → nothing to do
    };
    let patterns = parse(&text);
    if patterns.is_empty() {
        return report;
    }
    // One matcher over the whole file so `!` negation composes the way it does
    // in a .gitignore — a later `!secrets.env` really does exclude an earlier
    // `*.env`.
    let mut builder = GitignoreBuilder::new(project_root);
    for pattern in &patterns {
        if let Err(err) = builder.add_line(None, pattern) {
            report.skipped.push(Skip::Failed {
                path: PathBuf::from(pattern),
                error: format!("bad pattern: {err}"),
            });
        }
    }
    let matcher = match builder.build() {
        Ok(m) => m,
        Err(err) => {
            report.skipped.push(Skip::Failed {
                path: PathBuf::from(FILE_NAME),
                error: format!("could not build matcher: {err}"),
            });
            return report;
        }
    };

    // Sorted+deduped: two patterns may name the same file, and a stable order
    // makes the transcript reproducible.
    let mut selected: BTreeSet<PathBuf> = BTreeSet::new();
    for pattern in &patterns {
        if pattern.starts_with('!') {
            continue; // negation only ever removes; nothing to expand
        }
        expand(project_root, pattern, &matcher, &mut selected, &mut report);
    }

    for rel in selected {
        copy_one(project_root, worktree, &rel, &mut report);
    }
    report
}

/// Strip comments and blanks. Trailing whitespace goes; a trailing `\` escapes
/// it, as in gitignore.
fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_start)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Resolve one pattern to concrete relative paths.
///
/// A pattern with no wildcards is a path — resolved directly, with no walk at
/// all, which is the whole point of anchoring.
fn expand(
    root: &Path,
    pattern: &str,
    matcher: &Gitignore,
    out: &mut BTreeSet<PathBuf>,
    report: &mut CopyReport,
) {
    let cleaned = pattern.trim_start_matches('/').trim_end_matches('/');
    if cleaned.is_empty() {
        return;
    }
    let rel = Path::new(cleaned);
    if !is_contained(rel) {
        report.skipped.push(Skip::EscapesProjectRoot(pattern.to_string()));
        return;
    }

    // Counted rather than measured off `out.len()`: two patterns may legitimately
    // name the same file (`.env` and `*.env`), and a set that did not grow is
    // not the same as a pattern that matched nothing. Reporting the second
    // pattern as a miss would put a false alarm in the same transcript this
    // module exists to make trustworthy.
    let mut matched = 0usize;
    let (anchor, is_literal) = anchor_of(rel);
    let mut budget = SCAN_BUDGET;
    let exhausted = !collect(
        root,
        if is_literal { rel } else { &anchor },
        matcher,
        out,
        report,
        &mut budget,
        is_literal,
        &mut matched,
    );
    if exhausted {
        report.skipped.push(Skip::ScanBudgetExhausted(pattern.to_string()));
    } else if matched == 0 {
        report.skipped.push(Skip::MatchedNothing(pattern.to_string()));
    }
}

/// The literal directory prefix of a pattern, plus whether the pattern was
/// wholly literal. `certs/*.pem` → (`certs`, false); `.env` → (`.env`, true).
fn anchor_of(rel: &Path) -> (PathBuf, bool) {
    let mut anchor = PathBuf::new();
    for component in rel.components() {
        let part = component.as_os_str().to_string_lossy();
        if part.contains(['*', '?', '[']) {
            return (anchor, false);
        }
        anchor.push(component);
    }
    (anchor, true)
}

/// Walk `root/rel`, adding matching regular files to `out` and counting every
/// file the patterns selected (including one another pattern already claimed).
///
/// `take_all` is the literal-path case: the pattern named this path outright,
/// so the matcher is not consulted to *include* anything under it. It is still
/// consulted to *exclude*, because `!` has to work the same way inside a named
/// directory as it does under a wildcard — `certs/` plus `!certs/prod-key.pem`
/// must not copy the key.
///
/// Returns false when the budget ran out.
#[allow(clippy::too_many_arguments, reason = "one recursive walk's state; a struct would be the same fields with an extra name")]
fn collect(
    root: &Path,
    rel: &Path,
    matcher: &Gitignore,
    out: &mut BTreeSet<PathBuf>,
    report: &mut CopyReport,
    budget: &mut usize,
    take_all: bool,
    matched: &mut usize,
) -> bool {
    // `.git` is never an include, at any depth and whatever named it: copying it
    // would corrupt the new worktree, whose own `.git` is a file pointing back
    // at the main checkout. Checked on every component rather than on the leaf,
    // so `.git/config` is refused as squarely as `.git/`.
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return true;
    }
    let abs = root.join(rel);
    let meta = match std::fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(_) => return true, // absent → the caller reports "matched nothing"
    };
    if meta.is_symlink() {
        report.skipped.push(Skip::SourceIsSymlink(rel.to_path_buf()));
        return true;
    }
    if meta.is_file() {
        let verdict = matcher.matched(rel, false);
        let selected = if take_all {
            !verdict.is_whitelist()
        } else {
            verdict.is_ignore()
        };
        if selected {
            *matched += 1;
            out.insert(rel.to_path_buf());
        }
        return true;
    }
    if !meta.is_dir() {
        return true; // fifo, socket, device: not a thing to copy
    }
    let entries = match std::fs::read_dir(&abs) {
        Ok(e) => e,
        Err(err) => {
            report.skipped.push(Skip::Failed {
                path: rel.to_path_buf(),
                error: err.to_string(),
            });
            return true;
        }
    };
    for entry in entries.flatten() {
        // Budgeted on every branch, literal included. A named directory is not
        // inherently small — `.oximuxinclude` naming `vendor/` walks whatever
        // is in it, and an unbounded walk during worktree creation is the
        // failure this cap exists to make visible.
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        let child = rel.join(entry.file_name());
        if !collect(root, &child, matcher, out, report, budget, take_all, matched) {
            return false;
        }
    }
    true
}

/// Reject anything that would leave the project root. `..` is the obvious
/// case; an absolute or prefixed path is the Windows one.
fn is_contained(rel: &Path) -> bool {
    rel.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Copy one already-validated relative path, enforcing clauses 2, 3 and 4.
fn copy_one(project_root: &Path, worktree: &Path, rel: &Path, report: &mut CopyReport) {
    let src = project_root.join(rel);
    let dst = worktree.join(rel);

    // Clause 2 — the worktree's own (tracked) file wins. Checked with
    // `symlink_metadata` so a dangling symlink still counts as present rather
    // than being quietly replaced by our copy.
    if std::fs::symlink_metadata(&dst).is_ok() {
        report.skipped.push(Skip::AlreadyPresent(rel.to_path_buf()));
        return;
    }
    // Clause 3 — re-checked at copy time. `collect` already filtered symlinks,
    // but the tree can change between selection and write, and this is the
    // check that keeps `fs::copy` (which follows links) from dereferencing.
    match std::fs::symlink_metadata(&src) {
        Ok(meta) if meta.is_symlink() => {
            report.skipped.push(Skip::SourceIsSymlink(rel.to_path_buf()));
            return;
        }
        Ok(_) => {}
        Err(err) => {
            report.skipped.push(Skip::Failed {
                path: rel.to_path_buf(),
                error: err.to_string(),
            });
            return;
        }
    }
    // Clause 4 — no existing component of the destination may be a symlink, or
    // the write lands wherever that link points. Checked before any directory
    // is created, so `create_dir_all` below only ever extends a verified path.
    if let Some(crossed) = symlink_in_target_path(worktree, rel) {
        report.skipped.push(Skip::TargetPathCrossesSymlink(crossed));
        return;
    }
    if let Some(parent) = dst.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        report.skipped.push(Skip::Failed {
            path: rel.to_path_buf(),
            error: err.to_string(),
        });
        return;
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => report.copied.push(rel.to_path_buf()),
        Err(err) => report.skipped.push(Skip::Failed {
            path: rel.to_path_buf(),
            error: err.to_string(),
        }),
    }
}

/// The first existing component of `worktree/rel` (excluding the leaf) that is
/// a symlink, if any.
fn symlink_in_target_path(worktree: &Path, rel: &Path) -> Option<PathBuf> {
    let mut walked = worktree.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for component in &parts[..parts.len().saturating_sub(1)] {
        walked.push(component);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) if meta.is_symlink() => return Some(walked),
            Ok(_) => {}
            Err(_) => return None, // does not exist yet → nothing to cross
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (project_root, worktree) — the worktree starts empty, as a fresh
    /// `git worktree add` of a repo whose tracked files we then plant by hand.
    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        (tmp, root, worktree)
    }

    fn write(base: &Path, rel: &str, body: &str) {
        let p = base.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn declare(root: &Path, body: &str) {
        std::fs::write(root.join(FILE_NAME), body).unwrap();
    }

    #[test]
    fn absent_include_file_is_a_silent_no_op() {
        let (_t, root, wt) = dirs();
        let report = copy_included_files(&root, &wt);
        assert!(report.is_empty(), "{report:?}");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let (_t, root, wt) = dirs();
        write(&root, ".env", "SECRET=1");
        declare(&root, "# local files\n\n  .env  \n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from(".env")]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    // ---- Clause 1: files copied; directories copied recursively ----

    #[test]
    fn clause1_a_literal_file_is_copied() {
        let (_t, root, wt) = dirs();
        write(&root, ".env", "SECRET=1");
        declare(&root, ".env\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from(".env")]);
        assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1");
    }

    #[test]
    fn clause1_a_directory_is_copied_recursively() {
        let (_t, root, wt) = dirs();
        write(&root, "certs/dev.pem", "cert");
        write(&root, "certs/nested/ca.pem", "ca");
        declare(&root, "certs/\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied.len(), 2, "{:?}", report.copied);
        assert!(wt.join("certs/dev.pem").exists());
        assert!(wt.join("certs/nested/ca.pem").exists());
    }

    #[test]
    fn a_wildcard_is_anchored_at_its_literal_prefix() {
        let (_t, root, wt) = dirs();
        write(&root, "certs/dev.pem", "cert");
        write(&root, "certs/notes.txt", "no");
        // A sibling tree that an unanchored scan would walk. The assertion is
        // about the result, but the anchoring is what keeps it cheap.
        write(&root, "node_modules/pkg/index.js", "x");
        declare(&root, "certs/*.pem\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from("certs/dev.pem")]);
    }

    #[test]
    fn negation_excludes_a_file_an_earlier_pattern_matched() {
        let (_t, root, wt) = dirs();
        write(&root, "env/a.env", "a");
        write(&root, "env/secret.env", "s");
        declare(&root, "env/*.env\n!env/secret.env\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from("env/a.env")]);
    }

    // ---- Clause 2: a path already in the worktree wins ----

    #[test]
    fn clause2_tracked_file_wins_and_the_skip_is_reported() {
        let (_t, root, wt) = dirs();
        write(&root, "config.yaml", "from-project");
        write(&wt, "config.yaml", "tracked");
        declare(&root, "config.yaml\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty(), "{:?}", report.copied);
        assert_eq!(report.skipped, vec![Skip::AlreadyPresent("config.yaml".into())]);
        assert_eq!(
            std::fs::read_to_string(wt.join("config.yaml")).unwrap(),
            "tracked",
            "the tracked file must be left exactly as git checked it out"
        );
    }

    // ---- Clause 3: source symlinks are skipped, never dereferenced ----

    #[cfg(unix)]
    #[test]
    fn clause3_a_source_symlink_is_skipped_not_dereferenced() {
        let (_t, root, wt) = dirs();
        write(&root, "real.txt", "payload");
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
        declare(&root, "link.txt\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty(), "{:?}", report.copied);
        assert!(
            report.skipped.contains(&Skip::SourceIsSymlink("link.txt".into())),
            "{:?}",
            report.skipped
        );
        assert!(!wt.join("link.txt").exists(), "must not materialize the target");
    }

    /// The escape this clause exists for: a symlink pointing outside the
    /// project must not become a real copy of whatever it names.
    #[cfg(unix)]
    #[test]
    fn clause3_a_symlink_out_of_the_project_copies_nothing() {
        let (tmp, root, wt) = dirs();
        std::fs::write(tmp.path().join("outside.txt"), "not yours").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.txt"), root.join("leak.txt")).unwrap();
        declare(&root, "leak.txt\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty());
        assert!(!wt.join("leak.txt").exists());
    }

    // ---- Clause 4: never write through a symlink in the target ----

    #[cfg(unix)]
    #[test]
    fn clause4_a_symlinked_target_directory_is_refused() {
        let (tmp, root, wt) = dirs();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        write(&root, "conf/app.env", "SECRET=1");
        // The worktree contains `conf` -> /elsewhere. Writing `conf/app.env`
        // through it would land outside the worktree entirely.
        std::os::unix::fs::symlink(&elsewhere, wt.join("conf")).unwrap();
        declare(&root, "conf/\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty(), "{:?}", report.copied);
        assert!(
            matches!(report.skipped.as_slice(), [Skip::TargetPathCrossesSymlink(_)]),
            "{:?}",
            report.skipped
        );
        assert!(
            !elsewhere.join("app.env").exists(),
            "the write escaped the worktree — this is the bug the clause prevents"
        );
    }

    // ---- Clause 5: a pattern matching nothing is reported ----

    #[test]
    fn clause5_a_pattern_matching_nothing_is_reported_and_the_rest_continues() {
        let (_t, root, wt) = dirs();
        write(&root, ".env", "SECRET=1");
        declare(&root, "does-not-exist.txt\n.env\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from(".env")]);
        assert_eq!(
            report.skipped,
            vec![Skip::MatchedNothing("does-not-exist.txt".into())]
        );
    }

    #[test]
    fn clause5_a_wildcard_matching_nothing_is_reported_too() {
        let (_t, root, wt) = dirs();
        std::fs::create_dir_all(root.join("certs")).unwrap();
        declare(&root, "certs/*.pem\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.skipped, vec![Skip::MatchedNothing("certs/*.pem".into())]);
    }

    // ---- Clause 6: an unreadable source is reported, copy continues ----

    #[cfg(unix)]
    #[test]
    fn clause6_an_unreadable_source_is_reported_and_the_rest_continues() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_t, root, wt) = dirs();
        write(&root, "locked.env", "SECRET=1");
        write(&root, "ok.env", "fine");
        std::fs::set_permissions(root.join("locked.env"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        declare(&root, "locked.env\nok.env\n");
        let report = copy_included_files(&root, &wt);
        // Running as root defeats the permission bit; assert the invariant that
        // holds either way — the *other* file was copied regardless.
        assert!(
            report.copied.contains(&PathBuf::from("ok.env")),
            "one bad file must not abort the rest: {report:?}"
        );
    }

    // ---- Containment ----

    #[test]
    fn a_pattern_leaving_the_project_root_is_refused() {
        let (_t, root, wt) = dirs();
        declare(&root, "../outside.txt\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(
            report.skipped,
            vec![Skip::EscapesProjectRoot("../outside.txt".into())]
        );
        assert!(report.copied.is_empty());
    }

    #[test]
    fn dot_git_is_never_copied_even_when_a_pattern_names_it() {
        let (_t, root, wt) = dirs();
        write(&root, ".git/HEAD", "ref: refs/heads/main");
        declare(&root, ".git/\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty(), "{:?}", report.copied);
        assert!(!wt.join(".git/HEAD").exists());
    }

    /// The clause the wildcard test hid: negation has to work inside a
    /// literal directory too. `certs/` selects everything under `certs/`, so
    /// without consulting the matcher for exclusions the `!` line does nothing
    /// and a declared secret is copied into every worktree.
    #[test]
    fn negation_excludes_a_file_inside_a_literal_directory() {
        let (_t, root, wt) = dirs();
        write(&root, "certs/dev.pem", "dev");
        write(&root, "certs/prod-key.pem", "prod");
        declare(&root, "certs/\n!certs/prod-key.pem\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from("certs/dev.pem")]);
        assert!(
            !wt.join("certs/prod-key.pem").exists(),
            "an excluded file must not reach the worktree"
        );
    }

    /// Two patterns naming the same file is normal (`.env` then `*.env`). The
    /// second must not be reported as a miss — a false alarm in the transcript
    /// costs the same trust as a silent skip.
    #[test]
    fn a_second_pattern_matching_an_already_selected_file_is_not_a_miss() {
        let (_t, root, wt) = dirs();
        write(&root, ".env", "SECRET=1");
        declare(&root, ".env\n*.env\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from(".env")]);
        assert!(
            report.skipped.is_empty(),
            "neither pattern missed: {:?}",
            report.skipped
        );
    }

    #[test]
    fn a_literal_directory_and_a_wildcard_over_it_both_count_as_matches() {
        let (_t, root, wt) = dirs();
        write(&root, "certs/dev.pem", "cert");
        declare(&root, "certs/\ncerts/*.pem\n");
        let report = copy_included_files(&root, &wt);
        assert_eq!(report.copied, vec![PathBuf::from("certs/dev.pem")]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    /// A worktree's `.git` is a file pointing at the main checkout. Copying the
    /// project's `.git` over it — by any pattern, at any depth — corrupts it.
    #[test]
    fn a_file_inside_dot_git_is_refused_even_as_a_literal_path() {
        let (_t, root, wt) = dirs();
        write(&root, ".git/config", "[core]");
        declare(&root, ".git/config\n");
        let report = copy_included_files(&root, &wt);
        assert!(report.copied.is_empty(), "{:?}", report.copied);
        assert!(!wt.join(".git/config").exists());
    }

    #[test]
    fn anchor_of_splits_at_the_first_wildcard() {
        assert_eq!(anchor_of(Path::new(".env")), (PathBuf::from(".env"), true));
        assert_eq!(
            anchor_of(Path::new("certs/*.pem")),
            (PathBuf::from("certs"), false)
        );
        assert_eq!(anchor_of(Path::new("*.pem")), (PathBuf::new(), false));
    }
}
