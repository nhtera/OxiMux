//! `Repository` — handle for one git working tree.
//!
//! Validates on open via `git rev-parse --show-toplevel` and stores the canonical
//! working-tree root. All subsequent git commands run from that root, so callers
//! can pass any subdirectory of the tree when opening.

use crate::diff::parse_unified_diff;
use crate::error::{GitError, Result};
use crate::numstat;
use crate::process::GitCmd;
use crate::status;
use oximux_core::{BranchInfo, CombinedDiff, CombinedDiffScope, FileDiff, FileGroup, GitState};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Diff command timeout. Diffs on monorepos can be several MB and slower
/// than the 10 s default; keep status calls fast while giving diff room.
const DIFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Whole-file context for patch output. `git` has no "infinite context"
/// flag; a number larger than any real source file makes every unchanged
/// line render as context, so the diff VIEW shows the FULL file with
/// change markers (scroll the whole document) instead of just the changed
/// hunks with 3 lines around them.
///
/// Applied only to the diff-view fetch paths (`diff_for_path`,
/// `commit_files`) — NOT to `diff_unstaged`/`diff_staged`, which feed
/// hunk-count-sensitive callers and tests with git's default 3-line
/// context. The diff view re-derives stageable hunks from the full-context
/// diff via `oximux_core::change_regions`, so per-hunk staging keeps its
/// `git add -p` granularity. The large-diff guard
/// (`LARGE_DIFF_LINE_THRESHOLD`) still collapses oversized files, and the
/// virtualized renderer keeps a fully expanded big file cheap to paint.
const FULL_FILE_CONTEXT: &str = "--unified=1000000";

/// Common args shared across all `git diff` invocations:
/// - `-p`: produce a patch (we only parse this format)
/// - `--no-color`: ANSI codes would corrupt our line parser
/// - `--no-ext-diff`: ignore user `diff.external` config so output is the
///   format we parse
///
/// `-z` (which the plan listed) is intentionally omitted: it only affects
/// `--raw`/`--name-only`/`--name-status`/`--numstat` output. For `-p` it's
/// a no-op, so we drop the noise.
const DIFF_BASE_ARGS: &[&str] = &["diff", "-p", "--no-color", "--no-ext-diff"];

/// Only the first N untracked rows (status order = render order) get line
/// counts — the panel caps collapsed sections well below this anyway.
const UNTRACKED_COUNT_CAP: usize = 20;
/// Untracked files above this size are not counted (cached negative).
const UNTRACKED_SIZE_CAP: u64 = 1_000_000;
/// Ceiling on concurrent `--no-index` numstat processes per poll.
const UNTRACKED_CONCURRENCY: usize = 4;

/// Cache slot for `list_remote_branches`. `Arc<RwLock<_>>` so cloned
/// `Repository` handles share the same cache rather than each
/// re-running `git for-each-ref` independently.
pub(crate) type RemoteBranchCache = Arc<RwLock<Option<(Instant, Vec<BranchInfo>)>>>;

/// Cache for untracked-file line counts keyed by path → (mtime, counts).
/// `counts = None` is a cached negative (binary or oversized file) so the
/// poll doesn't re-stat-and-count it every tick. Shared across cloned
/// `Repository` handles like the other caches.
pub(crate) type UntrackedCountCache =
    Arc<RwLock<HashMap<PathBuf, (std::time::SystemTime, Option<(u32, u32)>)>>>;

/// One freshly counted untracked file: (path, mtime at count time, counts).
type CountedUntracked = (PathBuf, std::time::SystemTime, Option<(u32, u32)>);

#[derive(Debug, Clone)]
pub struct Repository {
    workdir: PathBuf,
    /// Resolved `.git` directory — the linked-worktree-aware path, NOT
    /// `workdir.join(".git")`. For a primary worktree this is
    /// `<workdir>/.git`. For a linked worktree (created via
    /// `git worktree add`) this is `<main>/.git/worktrees/<name>/`,
    /// which is where sentinel files like `MERGE_HEAD` /
    /// `REBASE_HEAD` / `CHERRY_PICK_HEAD` actually live for THIS
    /// worktree (not the main one's `.git/`). Captured at
    /// [`Repository::open`] so `current_operation` can stat them
    /// without re-shelling out per poll tick.
    pub(crate) git_dir: PathBuf,
    pub(crate) remote_branch_cache: RemoteBranchCache,
    /// Cache slot for `lease_status`. Shared across cloned handles for
    /// the same reason as `remote_branch_cache`.
    pub(crate) lease_status_cache: crate::remote::LeaseStatusCache,
    /// Per-path line counts for untracked files (see [`UntrackedCountCache`]).
    pub(crate) untracked_count_cache: UntrackedCountCache,
}

impl Repository {
    /// Open the repository that contains `path`. Returns `NotARepo` if `path`
    /// is not inside a git working tree (or the directory doesn't exist).
    /// Returns `NotInstalled` if the `git` binary is missing from PATH.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // Single rev-parse fetches both --show-toplevel (workdir root)
        // and --git-dir (the worktree-specific .git directory, which
        // differs from `workdir/.git` for linked worktrees). Output is
        // two lines: toplevel first, git-dir second. Issuing the two
        // queries in one process saves a spawn on every Repository
        // open.
        let res = match GitCmd::new(path)
            .args(["rev-parse", "--show-toplevel", "--git-dir"])
            .run_raw()
            .await
        {
            Ok(r) => r,
            Err(GitError::Spawn {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => {
                // Spawn returned NotFound — could be a missing `git` binary OR
                // a missing current_dir. If the path exists at this point, the
                // cwd is fine, so git itself is missing. (Pure error-path
                // disambiguation; not a TOCTOU pre-check on the success path.)
                if path.exists() {
                    return Err(GitError::NotInstalled);
                }
                return Err(GitError::NotARepo {
                    path: path.to_path_buf(),
                });
            }
            Err(_) => {
                return Err(GitError::NotARepo {
                    path: path.to_path_buf(),
                });
            }
        };
        if !res.status.success() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        // Path bytes are macOS-native UTF-8 (v1 is macOS-only). On other
        // platforms a non-UTF-8 toplevel would Parse-error here; revisit if
        // we ever port to Linux.
        let stdout = String::from_utf8(res.stdout)
            .map_err(|e| GitError::parse(format!("rev-parse output not utf-8: {e}")))?;
        let mut lines = stdout.lines();
        let toplevel = lines.next().unwrap_or("").trim().to_string();
        let git_dir_raw = lines.next().unwrap_or("").trim().to_string();
        if toplevel.is_empty() || git_dir_raw.is_empty() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        // `git rev-parse --git-dir` is relative to its `current_dir`
        // by default — resolve against the toplevel so callers (and
        // `current_operation`'s stat checks) get an absolute path
        // that works regardless of process cwd.
        let git_dir_path = PathBuf::from(&git_dir_raw);
        let git_dir = if git_dir_path.is_absolute() {
            git_dir_path
        } else {
            PathBuf::from(&toplevel).join(git_dir_path)
        };
        Ok(Self {
            workdir: PathBuf::from(toplevel),
            git_dir,
            remote_branch_cache: Arc::new(RwLock::new(None)),
            lease_status_cache: Arc::new(RwLock::new(None)),
            untracked_count_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Canonical working-tree root.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Run `git status --porcelain=v2 --branch --ignored=matching
    /// --untracked-files=all -z` and parse into `GitState`.
    ///
    /// `--ignored=matching` emits one `!` record per path that directly
    /// matches a `.gitignore` rule, instead of the traditional `--ignored`
    /// which recursively enumerates every file inside ignored trees
    /// (catastrophically slow in repos with large `target/`,
    /// `node_modules/`, etc.). The file explorer hides whole ignored
    /// subtrees anyway — leaf-level enumeration is wasted work.
    ///
    /// `--untracked-files=all` expands untracked DIRECTORIES into their
    /// individual files. Without it git's default (`normal`) collapses a
    /// new directory to a single `dir/` record, which the diff view then
    /// tries to read as a file — failing with "Is a directory". Listing
    /// leaf files instead makes every untracked row a real, diffable path
    /// (and respects `.gitignore`, so ignored trees are still skipped).
    pub async fn status(&self) -> Result<GitState> {
        // The status query, the `+N -N` numstat enrichment, and the
        // "Committed on Branch" range are three independent read-only git
        // queries — none consumes another's output. Running them
        // concurrently makes each poll cost the SLOWEST query instead of
        // the sum of all three, which matters most on the first poll at
        // startup (when the runtime is also spawning the relay, the editor
        // LSP, and the commit-graph stats) and on large repos where
        // `branch_committed`'s five-shellout chain dominates. `git`'s own
        // index lock is sidestepped by `GIT_OPTIONAL_LOCKS=0` (set in the
        // process wrapper), so concurrent reads don't contend.
        let status_fut = GitCmd::new(&self.workdir)
            .args([
                "status",
                "--porcelain=v2",
                "--branch",
                "--ignored=matching",
                "--untracked-files=all",
                "-z",
            ])
            .run();
        let (status_out, unstaged_res, staged_res, branch_res) = tokio::join!(
            status_fut,
            numstat::diff_numstat_unstaged(&self.workdir),
            numstat::diff_numstat_staged(&self.workdir),
            self.branch_committed(),
        );

        let out = status_out?;
        let mut state = status::parse_porcelain_v2(&out.stdout)?;

        // Enrich with per-side `+N -N` counts: `line_counts` carries the
        // UNSTAGED diff (worktree vs index) and `staged_line_counts` the
        // STAGED diff (index vs HEAD), so a partially staged file shows
        // each section's own figure instead of one combined vs-HEAD count
        // on both rows. Best effort: a numstat failure leaves the counts
        // at `None`, the UI keeps rendering, and the panel still gives
        // users every other piece of information. The `warn!` on the
        // failure path is the only signal a malformed numstat parse
        // leaves — without it, the symptom "no counts ever show" is
        // undiagnosable.
        match unstaged_res {
            Ok(counts) => {
                for f in state.files.iter_mut() {
                    if let Some(&(added, removed)) = counts.get(f.path.as_path()) {
                        f.line_counts = Some((added, removed));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    workdir = %self.workdir.display(),
                    "diff --numstat failed; status returned without unstaged line counts"
                );
            }
        }
        match staged_res {
            Ok(counts) => {
                for f in state.files.iter_mut() {
                    if let Some(&(added, removed)) = counts.get(f.path.as_path()) {
                        f.staged_line_counts = Some((added, removed));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    workdir = %self.workdir.display(),
                    "diff --cached --numstat failed; status returned without staged line counts"
                );
            }
        }
        self.fill_untracked_counts(&mut state).await;
        // Enrich with the "Committed on Branch" file list (commits this
        // branch carries vs its base). Best-effort, same contract as
        // numstat: a failure leaves the section empty and never fails the
        // poll.
        match branch_res {
            Ok((range, files)) => {
                state.branch_range = range;
                state.branch_committed = files;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    workdir = %self.workdir.display(),
                    "branch-committed enrichment failed; section will be empty"
                );
            }
        }
        Ok(state)
    }

    /// Fill `line_counts` for untracked files via `git diff --no-index`
    /// against `/dev/null` — bounded so a freshly-cloned dependency dump
    /// can't turn the status poll into a process storm:
    /// - only the first [`UNTRACKED_COUNT_CAP`] untracked rows (status
    ///   order = render order) are counted;
    /// - files over [`UNTRACKED_SIZE_CAP`] are skipped (cached negative);
    /// - results are cached by (path, mtime), so steady-state polls cost
    ///   one `fs::metadata` per row and zero git spawns;
    /// - fresh counts run at most [`UNTRACKED_CONCURRENCY`] at a time.
    ///
    /// Best-effort throughout: any failure simply leaves that row's count
    /// at `None` (no badge), never fails the poll.
    async fn fill_untracked_counts(&self, state: &mut GitState) {
        use futures::StreamExt;
        use oximux_core::WorktreeStatus;

        let candidates: Vec<PathBuf> = state
            .files
            .iter()
            .filter(|f| matches!(f.worktree, WorktreeStatus::Untracked))
            .map(|f| f.path.clone())
            .take(UNTRACKED_COUNT_CAP)
            .collect();
        if candidates.is_empty() {
            // Drop stale entries so a cleaned-up tree releases the memory.
            if let Ok(mut cache) = self.untracked_count_cache.write()
                && !cache.is_empty()
            {
                cache.clear();
            }
            return;
        }

        let mut resolved: HashMap<PathBuf, Option<(u32, u32)>> = HashMap::new();
        let mut to_count: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        for rel in &candidates {
            let abs = self.workdir.join(rel);
            let Ok(meta) = tokio::fs::metadata(&abs).await else {
                continue;
            };
            let Ok(mtime) = meta.modified() else { continue };
            let cached = self
                .untracked_count_cache
                .read()
                .ok()
                .and_then(|c| c.get(rel).copied());
            match cached {
                Some((cached_mtime, counts)) if cached_mtime == mtime => {
                    resolved.insert(rel.clone(), counts);
                }
                _ if meta.len() > UNTRACKED_SIZE_CAP => {
                    // Cached negative: don't re-stat the decision next poll.
                    if let Ok(mut cache) = self.untracked_count_cache.write() {
                        cache.insert(rel.clone(), (mtime, None));
                    }
                }
                _ => to_count.push((rel.clone(), mtime)),
            }
        }

        let fresh: Vec<CountedUntracked> =
            futures::stream::iter(to_count.into_iter().map(|(rel, mtime)| async move {
                let counts = numstat::diff_numstat_untracked_file(&self.workdir, &rel).await;
                (rel, mtime, counts)
            }))
            .buffer_unordered(UNTRACKED_CONCURRENCY)
            .collect()
            .await;

        if let Ok(mut cache) = self.untracked_count_cache.write() {
            for (rel, mtime, counts) in &fresh {
                cache.insert(rel.clone(), (*mtime, *counts));
            }
            // Evict paths that are no longer untracked (committed, deleted,
            // or beyond the cap) so the cache tracks the live set.
            let live: std::collections::HashSet<&PathBuf> = candidates.iter().collect();
            cache.retain(|k, _| live.contains(k));
        }
        for (rel, _, counts) in fresh {
            resolved.insert(rel, counts);
        }

        for f in state.files.iter_mut() {
            if let Some(counts) = resolved.get(&f.path) {
                f.line_counts = *counts;
            }
        }
    }

    /// Patch-format diff of the working tree against the index (changes not
    /// yet staged for commit).
    pub async fn diff_unstaged(&self) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&[], None).await
    }

    /// Patch-format diff of the index against HEAD (changes already
    /// `git add`ed but not committed).
    pub async fn diff_staged(&self) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&["--cached"], None).await
    }

    /// Diff a single path. `path` is interpreted relative to the working
    /// tree root; absolute paths inside the working tree also work because
    /// git resolves them itself.
    ///
    /// Returns an empty `Vec` when the path has no diff in the requested
    /// stage (e.g. asking for staged changes on a file that's only modified
    /// in the worktree). Caller distinguishes "no changes" from "file
    /// missing" via `git status` on the same path — `git diff` does not
    /// error on an unmodified path.
    pub async fn diff_for_path(&self, path: &Path, staged: bool) -> Result<Vec<FileDiff>> {
        // Full-file context so the diff view can scroll the whole document;
        // per-hunk staging granularity is recovered downstream via
        // `oximux_core::change_regions`.
        let extra: &[&str] = if staged {
            &[FULL_FILE_CONTEXT, "--cached"]
        } else {
            &[FULL_FILE_CONTEXT]
        };
        self.diff_with_args(extra, Some(path)).await
    }

    /// Full-context diff of a single path across an arbitrary revision
    /// range (`<base> <head>`). Powers the read-only per-file diff opened
    /// from the "Committed on Branch" section, where `base` is the
    /// merge-base and `head` is the branch tip. Same full-file context and
    /// patch shape as `diff_for_path`, so the diff view renders it
    /// identically (minus the staging chips, which the caller suppresses).
    pub async fn diff_for_range(
        &self,
        base: &str,
        head: &str,
        path: &Path,
    ) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&[FULL_FILE_CONTEXT, base, head], Some(path))
            .await
    }

    /// Synthesize an "all-additions" diff for an untracked file by reading
    /// its content directly off disk. Git's normal `diff` ignores untracked
    /// files (they're not in the index), so a vanilla `diff_for_path` on a
    /// new file returns an empty `Vec` and the diff view shows "No diff" —
    /// not useful when the user clicks an untracked row to inspect it.
    ///
    /// `path` may be relative-to-workdir or absolute inside the workdir;
    /// resolution mirrors `diff_for_path`.
    pub async fn diff_for_untracked(&self, path: &Path) -> Result<Vec<FileDiff>> {
        use oximux_core::{
            DiffHunk, DiffLine, DiffLineKind, DiffStatus, LARGE_DIFF_LINE_THRESHOLD,
        };

        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workdir.join(path)
        };
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => return Err(GitError::parse(format!("read {}: {e}", abs.display()))),
        };
        // Binary detection: same heuristic as `git diff` — if the first 8K
        // contains a NUL byte, treat as binary and surface the same status
        // git would emit.
        let preview = &bytes[..bytes.len().min(8192)];
        if preview.contains(&0u8) {
            return Ok(vec![FileDiff {
                path: path.to_path_buf(),
                status: DiffStatus::Binary,
                hunks: Vec::new(),
                large: false,
                mode: None,
            }]);
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| GitError::parse(format!("untracked file not utf-8: {e}")))?;
        let lines: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            // `split('\n')` keeps a trailing empty after a final '\n' which
            // would render as an extra blank line; trim it out. For files
            // without a trailing newline the marker line is appended below.
            let mut v: Vec<&str> = text.split('\n').collect();
            if text.ends_with('\n') {
                v.pop();
            }
            v
        };
        let line_count = lines.len() as u32;
        let mut diff_lines: Vec<DiffLine> = lines
            .into_iter()
            .map(|l| DiffLine {
                kind: DiffLineKind::Added,
                content: l.to_string(),
            })
            .collect();
        // Mirror git's "\ No newline at end of file" hint when applicable so
        // the diff view can render the EOF marker the same way it does for
        // tracked files. Empty files don't need the hint.
        if !text.is_empty() && !text.ends_with('\n') {
            diff_lines.push(DiffLine {
                kind: DiffLineKind::NoNewlineHint,
                content: "\\ No newline at end of file".to_string(),
            });
        }
        let hunks = if diff_lines.is_empty() {
            // Zero-byte file: no hunk; renderer falls back to the empty
            // state but the file is correctly classified as Added.
            Vec::new()
        } else {
            vec![DiffHunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: line_count,
                header_suffix: String::new(),
                lines: diff_lines,
            }]
        };
        let large = line_count as usize > LARGE_DIFF_LINE_THRESHOLD;
        Ok(vec![FileDiff {
            path: path.to_path_buf(),
            status: DiffStatus::Added,
            hunks,
            large,
            mode: None,
        }])
    }

    /// Raw bytes of the git blob at `<rev>:<rel_path>`, or `Ok(None)` when no
    /// such object exists at that rev (e.g. a freshly added file has no `HEAD`
    /// blob). Reads via `git cat-file blob` so smudge filters and the pager
    /// never touch the bytes — the diff view uses it to preview the "before"
    /// side of an image-binary change.
    ///
    /// `<rev>:<path>` treats `<path>` as a literal repo-relative pathname (not
    /// a pathspec), so glob metacharacters (`[ * ? {`) in the filename stay
    /// literal — matching the path-correctness discipline of the staging ops.
    pub async fn read_blob_at(&self, rev: &str, rel_path: &Path) -> Result<Option<Vec<u8>>> {
        // git uses forward slashes in object specs regardless of platform; on
        // the macOS/Linux targets here `Path` already separates with `/`.
        let spec = format!("{rev}:{}", rel_path.to_string_lossy());
        let out = GitCmd::new(&self.workdir)
            .timeout(DIFF_TIMEOUT)
            .arg("cat-file")
            .arg("blob")
            .arg(&spec)
            .run_raw()
            .await?;
        // A missing object exits non-zero ("fatal: path … does not exist") —
        // that's the added/untracked case, not an error to surface.
        Ok(out.status.success().then_some(out.stdout))
    }

    /// Per-file patch diff for a single commit. `sha` may be any
    /// ref-or-OID `git` accepts (short SHA, full SHA, branch, tag,
    /// `HEAD`, etc.). Returns one `FileDiff` per file the commit
    /// touches.
    ///
    /// Implementation uses `git show --format= -p --first-parent
    /// <sha>` — the empty `--format=` suppresses the commit metadata
    /// header so only the patch body lands in stdout (parseable by
    /// the same `parse_unified_diff` the regular diff methods use).
    /// `git show` handles root commits (no parent) natively, so a
    /// dedicated initial-commit fallback isn't needed.
    ///
    /// `--first-parent` makes merge commits emit a standard unified
    /// diff against their first parent (mainline) instead of the
    /// combined diff format (`@@@ -a,b -c,d +e,f @@@`) which
    /// `parse_unified_diff` doesn't speak. The trade-off: a merge
    /// commit's detail view shows "what landed from the merged
    /// branch", not the conflict resolutions baked into the merge —
    /// matches GitHub's PR view and `git log -p` default behavior.
    /// Combined-diff support is a v1.1 follow-up.
    pub async fn commit_files(&self, sha: &str) -> Result<Vec<FileDiff>> {
        let out = GitCmd::new(&self.workdir)
            .timeout(DIFF_TIMEOUT)
            .args([
                "show",
                "--no-color",
                "--no-ext-diff",
                FULL_FILE_CONTEXT,
                "--format=",
                "-p",
                "--first-parent",
                sha,
            ])
            .run()
            .await?;
        let raw = std::str::from_utf8(&out.stdout)
            .map_err(|e| GitError::parse(format!("show stdout not utf-8: {e}")))?;
        Ok(parse_unified_diff(raw)?)
    }

    /// Full-context diff of an arbitrary revision range (`<base> <head>`) for
    /// EVERY changed file — the all-files counterpart to `diff_for_range`.
    /// Powers the combined "Branch Diff" view. Read-only (already committed),
    /// so callers suppress staging chips.
    pub async fn diff_range_all(&self, base: &str, head: &str) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&[FULL_FILE_CONTEXT, base, head], None)
            .await
    }

    /// Assemble a combined multi-file diff for `scope`, returning the merged
    /// `Vec<FileDiff>` plus a parallel `Vec<FileGroup>` tagging each file's
    /// partition. Uses FULL-file context everywhere (like `diff_for_path`) so
    /// the combined view scrolls whole documents and per-region staging via
    /// `change_regions` keeps `git add -p` granularity.
    ///
    /// `AllChanges` order is unstaged → staged → untracked (the SCM panel's
    /// section order). A file changed in both index and worktree appears in
    /// BOTH the unstaged and staged slices — each stages/unstages its own
    /// side independently, so they're kept as separate entries.
    pub async fn diff_combined(&self, scope: CombinedDiffScope) -> Result<CombinedDiff> {
        let mut diffs: Vec<FileDiff> = Vec::new();
        let mut groups: Vec<FileGroup> = Vec::new();
        let mut push_all = |ds: Vec<FileDiff>, g: FileGroup| {
            for d in ds {
                diffs.push(d);
                groups.push(g);
            }
        };
        match scope {
            CombinedDiffScope::AllChanges => {
                // Concurrent: worktree-vs-index, index-vs-HEAD, and a status
                // scan to enumerate untracked paths (git diff ignores those).
                let (unstaged, staged, state) = tokio::join!(
                    self.diff_with_args(&[FULL_FILE_CONTEXT], None),
                    self.diff_with_args(&[FULL_FILE_CONTEXT, "--cached"], None),
                    self.status(),
                );
                push_all(unstaged?, FileGroup::Unstaged);
                push_all(staged?, FileGroup::Staged);
                push_all(self.untracked_diffs(&state?).await, FileGroup::Untracked);
            }
            CombinedDiffScope::Unstaged => {
                // The SCM "Changes" section: worktree-vs-index only (no
                // --cached, no untracked).
                let unstaged = self.diff_with_args(&[FULL_FILE_CONTEXT], None).await?;
                push_all(unstaged, FileGroup::Unstaged);
            }
            CombinedDiffScope::Staged => {
                let staged = self
                    .diff_with_args(&[FULL_FILE_CONTEXT, "--cached"], None)
                    .await?;
                push_all(staged, FileGroup::Staged);
            }
            CombinedDiffScope::Untracked => {
                let state = self.status().await?;
                push_all(self.untracked_diffs(&state).await, FileGroup::Untracked);
            }
            CombinedDiffScope::Branch { base, head } => {
                let ranged = self.diff_range_all(&base, &head).await?;
                push_all(ranged, FileGroup::Committed);
            }
        }
        Ok(CombinedDiff { diffs, groups })
    }

    /// Synthesize all-additions diffs for every untracked file in `state`.
    /// Untracked = porcelain `??` (Untracked on BOTH sides); git's `diff`
    /// skips these, so each is read off disk via `diff_for_untracked`. A
    /// single file's read failure is logged and skipped — the rest of the
    /// combined view still renders.
    async fn untracked_diffs(&self, state: &GitState) -> Vec<FileDiff> {
        use oximux_core::{IndexStatus, WorktreeStatus};
        // Read + synthesize every untracked file CONCURRENTLY rather than one
        // at a time — a combined "Untracked" view over many files otherwise
        // blocks on a long sequential chain of disk reads. `join_all`
        // preserves input order, so the section stays in file order.
        let paths: Vec<&std::path::Path> = state
            .files
            .iter()
            .filter(|f| {
                matches!(f.index, IndexStatus::Untracked)
                    && matches!(f.worktree, WorktreeStatus::Untracked)
            })
            .map(|f| f.path.as_path())
            .collect();
        let results = futures::future::join_all(paths.iter().map(|p| self.diff_for_untracked(p)))
            .await;
        let mut out = Vec::new();
        for (path, res) in paths.iter().zip(results) {
            match res {
                Ok(mut ds) => out.append(&mut ds),
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "untracked diff synth failed; skipping in combined view"
                ),
            }
        }
        out
    }

    async fn diff_with_args(&self, extra: &[&str], path: Option<&Path>) -> Result<Vec<FileDiff>> {
        let mut cmd = GitCmd::new(&self.workdir)
            .timeout(DIFF_TIMEOUT)
            .args(DIFF_BASE_ARGS.iter().copied())
            .args(extra.iter().copied());
        if let Some(p) = path {
            cmd = cmd.arg("--").arg(p.as_os_str());
        }
        let out = cmd.run().await?;
        // git diff output is conceptually UTF-8 on macOS (filenames + patch
        // text); non-UTF-8 patches in binary files are filtered out via the
        // "Binary files X and Y differ" line, so the residue is always text.
        let raw = std::str::from_utf8(&out.stdout)
            .map_err(|e| GitError::parse(format!("diff stdout not utf-8: {e}")))?;
        let diffs = parse_unified_diff(raw)?;
        Ok(diffs)
    }
}
