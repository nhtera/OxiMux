//! Stash operations on `Repository`. Thin wrappers over `git stash` +
//! `git status --porcelain` (for `is_dirty`). All ops are async.
//!
//! # Identity vs. address
//!
//! A stash has two names and they are not interchangeable:
//!
//! | Command | Accepts a raw sha? |
//! |---|---|
//! | `git stash apply <sha>` | yes |
//! | `git stash branch <n> <sha>` | yes |
//! | `git stash drop <sha>` | **no** — `error: '<sha>' is not a stash reference` |
//! | `git stash pop <sha>` | **no** |
//!
//! So the *identity* is [`StashEntry::sha`] (immutable), but `drop`/`pop` must
//! be handed the *address* `stash@{N}` — whose `N` shifts on every mutation
//! anywhere in the repository. [`Repository::resolve_stash_index`] converts
//! identity → current address, and is the reason a destructive op can be fired
//! safely; [`Repository::stash_drop`] then returns the sha git says it actually
//! removed, so nothing has to trust that the address was still correct.
//!
//! The stack itself is shared: `refs/stash` lives in the git *common*
//! directory, so every worktree of a repo sees one stack, and OxiMux is a
//! second in-app writer alongside the user's terminal.
//!
//! # Rename is not a git verb
//!
//! There is no `git stash rename`. [`Repository::stash_rename`] composes one
//! out of `drop` + `store`, which is why it is the only op here that mutates
//! entries it was not asked about: every entry above the target comes off and
//! goes back. Its docs carry the full sequence, the recovery contract, and
//! what a concurrent push does to it.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::{FileDiff, StashEntry, StashFile, StashFileOrigin, StashRef};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Fields of one `stash list` record, NUL-separated; `-z` NUL-terminates each
/// record. See [`parse_stash_list`] for why NUL and nothing else.
const STASH_LIST_FORMAT: &str = "--format=%H%x00%ct%x00%cr%x00%gs";

/// Fields each `stash list` record carries, per [`STASH_LIST_FORMAT`].
const FIELDS_PER_RECORD: usize = 4;

const STASH_LIST_CACHE_TTL: Duration = Duration::from_secs(15);

impl Repository {
    /// Working tree or index has any tracked changes (untracked files do NOT
    /// count — that mirrors `git stash push` default behavior, so callers can
    /// chain `is_dirty()` → `stash_push(_, false, &[])` without surprises).
    ///
    /// Uses porcelain v1 (`--untracked-files=no`) rather than reusing
    /// `self.status()` because we only need an empty/non-empty signal — v1 is
    /// one line per file and parses faster than the full v2 record stream.
    pub async fn is_dirty(&self) -> Result<bool> {
        let out = GitCmd::new(self.workdir())
            .args(["status", "--porcelain", "--untracked-files=no"])
            .run()
            .await?;
        Ok(!out.stdout.is_empty())
    }

    /// Push a new stash entry. Returns the freshly minted ref (`stash@{0}`).
    ///
    /// `include_untracked = true` adds `-u` (stashes untracked files too).
    /// `paths` scopes the push to a subset; **empty means the whole tree**,
    /// which is the pre-existing behavior every current caller wants.
    ///
    /// Returns `Err(InvalidInput)` when there is nothing to stash — git itself
    /// exits 0 in this case, so we detect via stdout text rather than exit code.
    ///
    /// The returned `stash@{0}` is only valid until the next mutation of the
    /// stack; see the module docs. Prefer re-reading `stash_list` and carrying
    /// the sha if the ref outlives the call.
    pub async fn stash_push(
        &self,
        msg: Option<&str>,
        include_untracked: bool,
        paths: &[&Path],
    ) -> Result<StashRef> {
        let mut cmd = GitCmd::new(self.workdir()).args(["stash", "push"]);
        if include_untracked {
            cmd = cmd.arg("-u");
        }
        if let Some(m) = msg {
            cmd = cmd.args(["-m", m]);
        }
        // Path-scoped pushes go through the shared pathspec machinery so each
        // path is wrapped `:(literal)`. A bare pathspec would glob: pushing
        // `a[1].rs` would also capture `a1.rs`, which the user never named.
        // Note this is the ONE path-scoped op that cannot chunk — a stash is a
        // single commit, so splitting the argv would split the stash. The
        // pathspec wrapping is what matters here; `PATH_ARG_CHUNK` does not
        // apply.
        //
        // **That means `paths` has a hard ceiling this op cannot raise.**
        // Measured on macOS (`ARG_MAX` = 1_048_576): a single exec carrying
        // 12_000 paths (756 KB of argv) succeeds; 20_000 (1.26 MB) is refused
        // by the kernel with `E2BIG` before git runs, surfacing as
        // `GitError::Spawn { kind: ArgumentListTooLong, .. }`. It fails
        // cleanly — nothing is stashed and the worktree is untouched — but the
        // message is an exec error, not an explanation.
        //
        // The caller owns the selection, so the caller owns the cap: a
        // "stash the N files I selected" UI should refuse or warn well before
        // ~1 MB of accumulated path bytes rather than let the user click into
        // a spawn failure.
        let out = if paths.is_empty() {
            cmd.run().await?
        } else {
            let mut cmd = cmd.arg("--");
            for p in paths {
                cmd = cmd.arg(crate::stage::literal_pathspec(p));
            }
            cmd.run().await?
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        // `git stash push` on a clean tree exits 0 with stdout "No local changes
        // to save" — that's a no-op, not a stash, so surface as InvalidInput.
        // A path-scoped push whose paths are all CLEAN prints the same thing
        // and is caught here too (verified). The other shape — a pathspec
        // matching nothing git knows about — prints "did not match" and exits
        // NON-zero, so it never reaches this check; `.run()` has already
        // returned `NonZero` by then.
        if stdout.contains("No local changes to save") {
            return Err(GitError::invalid_input("nothing to stash"));
        }
        self.invalidate_stash_list_cache();
        Ok(StashRef { index: 0 })
    }

    /// List all stash entries, most-recent first (index 0 = top of stack).
    ///
    /// Results are cached for [`STASH_LIST_CACHE_TTL`]; pass
    /// `force_refresh = true` to bypass it. **Any caller whose correctness
    /// depends on the live stack must pass `true`** — re-resolving a stranded
    /// auto-stash, or comparing stack depth across a mutation, are both wrong
    /// against a cached list. Our own mutating ops invalidate the cache, so
    /// `false` is stale with respect to any writer outside **this**
    /// `Repository` handle. That includes the user's terminal and a sibling
    /// worktree — but also a second handle inside this very process: the cache
    /// is per-handle (`Repository::open` mints a fresh one), and the app opens
    /// handles at several call sites. A merge auto-stash taken through its own
    /// handle is therefore invisible to the panel's handle until the TTL
    /// lapses.
    ///
    /// Returns an empty Vec when the stash stack is empty.
    pub async fn stash_list(&self, force_refresh: bool) -> Result<Vec<StashEntry>> {
        if !force_refresh
            && let Some(cached) = self.cached_stash_list()
        {
            return Ok(cached);
        }
        let out = GitCmd::new(self.workdir())
            .args(["stash", "list", "-z", STASH_LIST_FORMAT])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git stash list`: {e}")))?;
        let fresh = parse_stash_list(&text)?;
        self.store_stash_list_cache(fresh.clone());
        Ok(fresh)
    }

    fn cached_stash_list(&self) -> Option<Vec<StashEntry>> {
        let guard = self.stash_list_cache.read().ok()?;
        let (recorded_at, entries) = guard.as_ref()?;
        if recorded_at.elapsed() < STASH_LIST_CACHE_TTL {
            Some(entries.clone())
        } else {
            None
        }
    }

    fn store_stash_list_cache(&self, entries: Vec<StashEntry>) {
        // Lock poisoning here is benign — the cache is best-effort. Drop the
        // update rather than panicking the caller's task.
        if let Ok(mut guard) = self.stash_list_cache.write() {
            *guard = Some((Instant::now(), entries));
        }
    }

    /// Force the next `stash_list` to re-read. Called by every op in this
    /// module that mutates the stack.
    ///
    /// Scope: this clears the cache shared by clones of THIS handle. It does
    /// not reach a `Repository` opened separately elsewhere in the process, so
    /// it is not an app-wide invalidation — see `stash_list`.
    fn invalidate_stash_list_cache(&self) {
        if let Ok(mut guard) = self.stash_list_cache.write() {
            *guard = None;
        }
    }

    /// Files inside a stash: the tracked diff from `^1`, plus the untracked
    /// files `git stash push -u` parked in `^3`.
    ///
    /// **`--first-parent` is not optional.** A stash commit has 2–3 parents, so
    /// without it `git show` emits a *combined* diff whose status column is one
    /// char per parent (`MMA`) and which omits files that did not change
    /// against every parent. Verified: a 3-file stash reported one file and the
    /// status `MMA` — the naive form silently loses rows.
    ///
    /// `-z` is this crate's uniform convention (`branch_diff.rs`, `log.rs`,
    /// `numstat.rs`) and `core.quotePath=false` is set unconditionally
    /// (`process.rs`), so without it a path containing a newline splits into
    /// phantom records — which would then render as clickable rows and be
    /// handed to a destructive per-file restore. `-M` makes renames arrive as
    /// `R<score>\0<old>\0<new>\0` instead of an unrelated add/delete pair.
    ///
    /// A stash created without `-u` has **no** `^3` at all (verified: `git
    /// show` hard-errors `unknown revision`), so that query failing means "no
    /// untracked files", never an error.
    pub async fn stash_files(&self, sha: &str) -> Result<Vec<StashFile>> {
        let tracked_rev = sha.to_string();
        let untracked_rev = format!("{sha}^3");
        // Concurrent — the two queries are independent, matching how
        // `diff_combined` fans out its pair.
        let (tracked, untracked) = tokio::join!(
            self.name_status_at(&tracked_rev, true),
            self.untracked_name_status(&untracked_rev),
        );
        let mut out: Vec<StashFile> = tracked?
            .into_iter()
            .map(|(status, path)| StashFile {
                path,
                status,
                origin: StashFileOrigin::Tracked,
            })
            .collect();
        out.extend(untracked?.into_iter().map(|(status, path)| StashFile {
            path,
            status,
            origin: StashFileOrigin::Untracked,
        }));
        Ok(out)
    }

    /// Every file a stash touches, as full patch diffs — the "Open All
    /// Changes" tab's fetch.
    ///
    /// **Composes [`Repository::commit_files`]; never modifies it.** That
    /// method uses `--first-parent`, so on a stash commit it reports the
    /// tracked side and nothing else: a `-u` stash's untracked files live in
    /// the parentless `^3` and are invisible to it. The obvious repair —
    /// teaching `commit_files` about `^3` — would silently change the file set
    /// of every commit-detail tab in the app, which is why this is a sibling
    /// and not an edit.
    ///
    /// `^3` is a root commit, so `git show -p` on it emits an ordinary
    /// new-file diff per untracked path (verified, including a path with glob
    /// metacharacters) — the same shape `parse_unified_diff` already reads.
    /// A stash pushed without `-u` has no `^3` at all; see
    /// [`Repository::untracked_commit_files`] for why absence is settled by
    /// exit code rather than by error text.
    pub async fn stash_all_files(&self, sha: &str) -> Result<Vec<FileDiff>> {
        let untracked_rev = format!("{sha}^3");
        // Concurrent — independent queries, matching `stash_files`.
        let (tracked, untracked) = tokio::join!(
            self.commit_files(sha),
            self.untracked_commit_files(&untracked_rev),
        );
        let mut out = tracked?;
        out.extend(untracked?);
        Ok(out)
    }

    /// Patch diffs for a stash's `^3`, or an empty list when it has none.
    ///
    /// Same absence-is-not-failure rule as
    /// [`Repository::untracked_name_status`]: a stash pushed without `-u` has
    /// no third parent and `git show` hard-errors on it, but a `^3` that
    /// exists and cannot be read is a real failure the caller must see —
    /// otherwise the tab quietly shows the tracked half and the user believes
    /// their untracked files were never stashed.
    async fn untracked_commit_files(&self, rev: &str) -> Result<Vec<FileDiff>> {
        if !self.rev_exists(rev).await {
            return Ok(Vec::new());
        }
        self.commit_files(rev).await
    }

    /// The `^3` half of [`Repository::stash_files`]: untracked files, or an
    /// empty list when the stash simply has no third parent.
    ///
    /// **Absence and failure are different things.** A stash pushed without
    /// `-u` has no `^3` at all, and `git show` on it errors — that is the
    /// normal shape and must read as "no untracked files". But blanket-
    /// swallowing every error here would also hide a `^3` that EXISTS and
    /// cannot be read (a corrupt or pruned object): the expanded row would
    /// silently show tracked files only, and the user would believe their
    /// untracked files were never stashed.
    ///
    /// So existence is settled by `rev-parse --verify --quiet`, whose exit
    /// code is a stable, documented interface — unlike matching git's error
    /// prose, which would bind us to one wording.
    async fn untracked_name_status(
        &self,
        rev: &str,
    ) -> Result<Vec<(oximux_core::DiffStatus, PathBuf)>> {
        if !self.rev_exists(rev).await {
            return Ok(Vec::new());
        }
        self.name_status_at(rev, false).await
    }

    /// Whether `rev` resolves. `--quiet` suppresses output and the exit code
    /// carries the answer, so this never depends on message text.
    async fn rev_exists(&self, rev: &str) -> bool {
        GitCmd::new(self.workdir())
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(rev)
            .arg("--")
            .run_raw()
            .await
            .is_ok_and(|out| out.status.success())
    }

    /// `git show --name-status` at one revision, parsed by the shared
    /// `-z` parser. `first_parent` selects `--first-parent` (required for the
    /// multi-parent stash commit itself; irrelevant for the parentless `^3`).
    async fn name_status_at(
        &self,
        rev: &str,
        first_parent: bool,
    ) -> Result<Vec<(oximux_core::DiffStatus, PathBuf)>> {
        let mut cmd = GitCmd::new(self.workdir()).args([
            "show",
            "--no-color",
            "--format=",
            "--name-status",
            "-M",
            "-z",
        ]);
        if first_parent {
            cmd = cmd.arg("--first-parent");
        }
        // `--` terminates revisions so a branch/file name collision cannot
        // reinterpret `rev` as a path.
        let raw = cmd.arg(rev).arg("--").run_raw().await?;
        if !raw.status.success() {
            return Err(GitError::parse(format!(
                "`git show --name-status` failed at {rev}"
            )));
        }
        Ok(crate::branch_diff::parse_name_status_z(&raw.stdout))
    }

    /// Current `stash@{N}` address of `sha`, or `None` when it is gone.
    ///
    /// This is what makes a destructive op safe, and it deliberately
    /// **self-heals** rather than refusing: if someone stashed or dropped
    /// elsewhere, the index has moved but the user's target still exists, so
    /// re-deriving the address does what they asked instead of toasting an
    /// error at them. `None` — the stash genuinely no longer exists — is the
    /// only case that should abort.
    ///
    /// # A sha does not have to be unique on the stack
    ///
    /// `hint` is the address the caller's row was *painted* with, and it is
    /// what breaks the tie when it isn't.
    ///
    /// Two entries can point at one commit, because `git stash store` writes
    /// a reflog entry for whatever sha it is handed and never checks whether
    /// that commit is already on the stack. **Verified** — and not by the
    /// obvious route: storing the same sha twice in a row leaves ONE entry,
    /// since git skips the reflog append when the ref value does not change.
    /// It takes an interleaved store:
    ///
    /// ```text
    /// git stash store -m A $A     # refs/stash -> A
    /// git stash store -m B $B     #            -> B
    /// git stash store -m A2 $A    #            -> A, now at {0} AND {2}
    /// ```
    ///
    /// Resolving by "first match" then silently retargets: Drop on the second
    /// row removes the first one instead, and the sha assertion downstream
    /// cannot tell the difference because both entries carry the same sha.
    /// Honouring `hint` when it still names this sha keeps each row pointed
    /// at its own entry; falling back to the first match when it does not
    /// preserves the self-healing behaviour above, which is the common case.
    ///
    /// Always reads the live stack; a cached list would defeat the purpose.
    pub async fn resolve_stash_index(
        &self,
        sha: &str,
        hint: Option<usize>,
    ) -> Result<Option<StashRef>> {
        let out = GitCmd::new(self.workdir())
            .args(["stash", "list", "--format=%H"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git stash list`: {e}")))?;
        let shas: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        // The painted address, if it still holds this sha. Costs nothing when
        // the sha is unique — it is the same answer — and is the whole fix
        // when it is not.
        if let Some(index) = hint
            && shas.get(index).is_some_and(|s| *s == sha)
        {
            return Ok(Some(StashRef { index }));
        }
        Ok(shas
            .iter()
            .position(|l| *l == sha)
            .map(|index| StashRef { index }))
    }

    /// Apply stash without removing it from the stack.
    ///
    /// `with_index` adds `--index`, which restores the staged/unstaged split
    /// the stash was taken with instead of dumping everything into the
    /// worktree as unstaged. That split is real and recoverable: a stash
    /// commit's second parent (`^2`) IS the index at push time, which is
    /// where `--index` reads it back from. Without the flag the information
    /// is not lost, it is simply not used — so the two are one flag, not two
    /// code paths.
    ///
    /// git refuses `--index` when the index cannot be reinstated cleanly
    /// (typically because something is already staged); the error surfaces to
    /// the caller rather than being retried without the flag, because a
    /// silent downgrade would restage nothing and claim success.
    pub async fn stash_apply(&self, stash_ref: &StashRef, with_index: bool) -> Result<()> {
        let mut cmd = GitCmd::new(self.workdir()).args(["stash", "apply"]);
        if with_index {
            cmd = cmd.arg("--index");
        }
        cmd.arg("--").arg(stash_ref.ref_string()).run().await?;
        // Applying does not change the stack, but it does change the worktree;
        // the entry itself survives, so no invalidation is needed.
        Ok(())
    }

    /// Apply stash and remove from stack.
    pub async fn stash_pop(&self, stash_ref: &StashRef) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["stash", "pop", "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        self.invalidate_stash_list_cache();
        Ok(())
    }

    /// Remove a stash entry without applying it. Returns the sha git reports
    /// it **actually** dropped.
    ///
    /// That return value is the point. Resolving an index by sha and then
    /// handing git the *index* still races — something can land in the gap —
    /// and a caller that only checked the index beforehand fails OPEN. git
    /// prints exactly what is needed on stdout:
    /// `Dropped stash@{0} (c8ff104cb8494345ff53d9eb88421d7a03a993b6)`.
    /// Compare it against the sha you meant to drop; if they differ, the wrong
    /// stash died and `stash_store` can put it back.
    ///
    /// Returns `Err(Parse)` when git succeeds but prints no recognizable sha,
    /// because a caller that cannot verify the outcome must not be told the
    /// drop was confirmed.
    pub async fn stash_drop(&self, stash_ref: &StashRef) -> Result<String> {
        let out = GitCmd::new(self.workdir())
            .args(["stash", "drop", "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        self.invalidate_stash_list_cache();
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_dropped_sha(&stdout).ok_or_else(|| {
            GitError::parse(format!(
                "`git stash drop` succeeded but printed no sha: {:?}",
                stdout.trim()
            ))
        })
    }

    /// `git stash branch <name> <stash@{N}>` — create a branch at the stash's
    /// base commit and apply the stash onto it.
    ///
    /// **Consumes the stash on success** (git prints
    /// `Dropped stash@{1} (…)`), which is a surprise worth disclosing in any
    /// confirm copy.
    ///
    /// **On failure the stash survives — but the branch may not be undone.**
    /// Verified, with a dirty worktree blocking the apply: git exits 1 having
    /// ALREADY created and checked out the new branch, leaving the stash on
    /// the stack and the worktree dirty (`Switched to a new branch 'fresh'` …
    /// `Index was not unstashed.`). So an `Err` from this call does not mean
    /// "nothing happened": the user may now be on a different branch. A caller
    /// must refresh HEAD/branch state on the error path, not just the stash
    /// list, and its copy should not promise the operation is atomic.
    ///
    /// The one clean refusal is a name collision — `fatal: a branch named
    /// '<x>' already exists` — which fails before touching anything.
    ///
    /// **It restores the staged/unstaged split**, unlike a plain
    /// `stash_apply(_, false)`: it applies with `--index`, and it can do so
    /// unconditionally because the branch it just created sits at the stash's
    /// base commit, so there is never a staged change for the index to
    /// conflict with. Verified in both directions — a change stashed unstaged
    /// comes back unstaged, one stashed staged comes back staged.
    pub async fn stash_branch(&self, name: &str, stash_ref: &StashRef) -> Result<()> {
        if name.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        GitCmd::new(self.workdir())
            .args(["stash", "branch", name, "--"])
            .arg(stash_ref.ref_string())
            .run()
            .await?;
        self.invalidate_stash_list_cache();
        Ok(())
    }

    /// Restore one tracked file out of a stash into the worktree AND the index.
    ///
    /// **DESTRUCTIVE** — overwrites whatever is at `path` with no backup. The
    /// caller owns the confirm UX.
    ///
    /// Routes through [`Repository::run_pathspec_op`] so the path is wrapped
    /// `:(literal)`. Verified: a bare pathspec makes `git checkout <sha> --
    /// 'a[1].rs'` ALSO overwrite `a1.rs`, a file the user never named and
    /// never confirmed. Every destructive sibling in `stage.rs` already routes
    /// this way; this one is not an exception.
    ///
    /// Only valid for [`StashFileOrigin::Tracked`]. An untracked file is not in
    /// the stash commit's tree, so `checkout <sha> -- <path>` errors; callers
    /// must gate on origin.
    pub async fn stash_restore_file(&self, sha: &str, path: &Path) -> Result<()> {
        self.run_pathspec_op(&["checkout", sha, "--"], &[path]).await
    }

    /// `git stash store -m <msg> <sha>` — push an existing stash commit back
    /// onto the top of the stack. Never rewrites the commit, only the reflog
    /// pointer, so a stash dropped by sha stays recoverable.
    ///
    /// Two verified behaviors callers must know:
    /// - The message is written **literally** — no `On <branch>: ` prefix is
    ///   synthesized, so a stored entry parses with an empty `branch`.
    /// - It is a **no-op when `sha` is already on the stack** (exit 0, stack
    ///   unchanged, message untouched). It cannot be used to relabel a live
    ///   entry; drop it first.
    pub async fn stash_store(&self, sha: &str, msg: &str) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["stash", "store", "-m", msg, "--"])
            .arg(sha)
            .run()
            .await?;
        self.invalidate_stash_list_cache();
        Ok(())
    }

    /// Rename a stash entry's message **at any depth in the stack**.
    ///
    /// Git has no `stash rename`, so this is composed from porcelain:
    /// everything down to and including the target is dropped, the target is
    /// re-stored under its new message, and the entries above it are re-stored
    /// on top of it in reverse order. Net effect: the same shas, in the same
    /// positions, with one message changed.
    ///
    /// Returns the entries step 5 could **not** put back, already formatted for
    /// display — empty on a clean run. `Ok` with a non-empty vec means the
    /// rename landed but the stack is short; see "if a restore fails" below.
    ///
    /// # Why not rewrite `.git/logs/refs/stash`
    ///
    /// Editing the reflog directly was sandbox-verified to work and is still
    /// wrong, four times over: it takes no `refs/stash.lock`, so a concurrent
    /// `git stash push` landing between the read and the rename is silently
    /// erased; in a linked worktree the reflog is under `--git-common-dir`, not
    /// `.git/`, which is precisely this app's normal environment; under git
    /// 2.45+'s reftable backend `.git/logs/` does not exist at all; and
    /// rename-over-open-file plus CRLF normalisation make it hostile to the
    /// Windows port. Porcelain costs 2N+2 subprocesses and has none of that.
    ///
    /// # The target is resolved by sha on EVERY iteration
    ///
    /// Not `git stash drop stash@{0}` repeated N+1 times. The stack is shared
    /// with every worktree and with the user's terminal, so a push landing
    /// mid-sequence takes index 0 and a blind second drop deletes it — an entry
    /// that cannot even be in the recovery log below, because it did not exist
    /// when the log was written. Re-resolving means such an intruder
    /// **survives**. It does not keep its position: it settles below the
    /// restored entries rather than on top. That reordering is accepted and
    /// documented, not eliminated — the window is 2N+2 subprocesses wide and
    /// there is no lock to close it.
    ///
    /// # The recovery log is written before the first drop
    ///
    /// Every entry this call will touch is logged with its sha before anything
    /// mutates, because a crash mid-sequence leaves a stack that is short by
    /// however many entries had been dropped. Stash commits are never rewritten
    /// — `store` only writes a reflog pointer — so every one of them stays
    /// recoverable by sha for as long as gc leaves it alone.
    ///
    /// # If a restore fails, the loop keeps going
    ///
    /// A failed `store` in step 5 is recorded and the remaining entries are
    /// still restored. Aborting there would cost the entries below the failure
    /// as well, turning one missing stash into several.
    ///
    /// # `store` writes the message literally
    ///
    /// Verified: `git stash store -m "x"` records `stash@{0}: x`, with no
    /// `On <branch>: ` prefix synthesized. So the prefix is re-applied here,
    /// from the branch the original subject carried, to keep the list uniform.
    /// An entry that never had one (itself written by `store`) is left bare
    /// rather than given an invented branch.
    pub async fn stash_rename(&self, sha: &str, new_message: &str) -> Result<Vec<String>> {
        self.stash_rename_hooked(sha, new_message, || async {}).await
    }

    /// [`stash_rename`] with a hook fired after each drop.
    ///
    /// Exists **only** so a test can push a stash into the middle of the loop
    /// and prove the per-iteration sha resolution above; the claim that an
    /// intruder survives is not one to take on faith. `stash_rename` is this
    /// with a hook that does nothing. Not a supported API.
    ///
    /// [`stash_rename`]: Self::stash_rename
    #[doc(hidden)]
    pub async fn stash_rename_hooked<F, Fut>(
        &self,
        sha: &str,
        new_message: &str,
        after_each_drop: F,
    ) -> Result<Vec<String>>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let subjects = self.stash_subjects().await?;
        let Some(target_pos) = subjects.iter().position(|(s, _)| s == sha) else {
            return Err(GitError::invalid_input(format!(
                "stash {} is no longer on the stack",
                short_sha(sha)
            )));
        };
        // Everything from the top down to and including the target: the
        // entries that have to come off before the target is reachable.
        let touched = &subjects[..=target_pos];

        // ── Step 2. The recovery sequence, before anything mutates.
        //
        // Structured fields, not an interpolated `-m <msg>`: a stash message
        // legally contains quotes, `$` and backslashes, so a rendered command
        // line would look copy-pasteable and not be. The sha is the part that
        // matters — `git stash store <sha>` takes it alone. Same rule the drop
        // path follows.
        for (i, (s, subject)) in touched.iter().enumerate() {
            tracing::warn!(
                target: "oximux_git::stash",
                sha = %s,
                index = i,
                subject = %subject,
                "stash rename will drop this entry; recover with: git stash store {}",
                s,
            );
        }

        // ── Step 3. Drop the prefix, top first, resolving by sha each time.
        for (s, _) in touched {
            let Some(stash_ref) = self.resolve_stash_index(s, None).await? else {
                // Already gone — someone else dropped it while we worked. Not
                // an abort: the entry we were going to put back simply is not
                // ours to put back any more.
                tracing::warn!(
                    target: "oximux_git::stash",
                    sha = %s,
                    "stash vanished mid-rename; skipping it",
                );
                continue;
            };
            let dropped = self.stash_drop(&stash_ref).await?;
            if &dropped != s {
                // git removed something else in the gap between resolve and
                // fire. Put it straight back, then abort — a rename that
                // continues from here is destroying entries it never listed.
                let label = subjects
                    .iter()
                    .find(|(x, _)| *x == dropped)
                    .map(|(_, g)| g.clone())
                    .unwrap_or_else(|| format!("recovered stash {}", short_sha(&dropped)));
                let restored = self.stash_store(&dropped, &label).await;
                return Err(GitError::unexpected_outcome(match restored {
                    Ok(()) => format!(
                        "rename aborted: git removed {} instead of {}. It has been restored to \
                         the TOP of the stack, so the order has changed. Nothing was lost.",
                        short_sha(&dropped),
                        short_sha(s),
                    ),
                    Err(e) => format!(
                        "rename aborted: git removed {} instead of {}, and restoring it FAILED: \
                         {e}. Recover it by hand: git stash store {}",
                        short_sha(&dropped),
                        short_sha(s),
                        dropped,
                    ),
                }));
            }
            after_each_drop().await;
        }

        // ── Step 4. The target, under its new message, prefix restored.
        let (branch, _) = split_branch_prefix(&touched[target_pos].1);
        let new_subject = if branch.is_empty() {
            new_message.to_string()
        } else {
            format!("On {branch}: {new_message}")
        };
        let target_stored = self.stash_store(sha, &new_subject).await;

        // ── Step 5. Everything that was above it, back on top, in reverse.
        let mut failures = Vec::new();
        for (s, subject) in touched[..target_pos].iter().rev() {
            if let Err(e) = self.stash_store(s, subject).await {
                tracing::error!(
                    target: "oximux_git::stash",
                    sha = %s,
                    %e,
                    "could not restore a stash during rename; recover with: git stash store {}",
                    s,
                );
                failures.push(format!("{}: {e}", short_sha(s)));
            }
        }

        // Reported after step 5 ran, not instead of it: the target failing to
        // come back is the loudest outcome, but it is not a reason to strand
        // the entries above it as well.
        target_stored.map_err(|e| {
            GitError::unexpected_outcome(format!(
                "the renamed stash could not be put back: {e}. \
                 Recover it by hand: git stash store {sha}"
            ))
        })?;
        Ok(failures)
    }

    /// `(sha, raw reflog subject)` for every entry, top first.
    ///
    /// The **raw** `%gs`, not [`StashEntry`]'s split `(branch, message)` pair:
    /// step 5 puts each subject back verbatim, and reassembling one from the
    /// split would turn `WIP on main: x` into `On main: x` — a silent rewrite
    /// of an entry the user never asked to touch.
    ///
    /// Same NUL framing and the same fails-closed field count as
    /// [`parse_stash_list`]; see its docs for why nothing printable will do.
    async fn stash_subjects(&self) -> Result<Vec<(String, String)>> {
        let out = GitCmd::new(self.workdir())
            .args(["stash", "list", "-z", "--format=%H%x00%gs"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git stash list`: {e}")))?;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let body = text.strip_suffix('\0').unwrap_or(&text);
        let tokens: Vec<&str> = body.split('\0').collect();
        if !tokens.len().is_multiple_of(2) {
            return Err(GitError::parse(format!(
                "`git stash list` returned {} fields, not a multiple of 2 — is `-z` being \
                 honoured?",
                tokens.len()
            )));
        }
        Ok(tokens
            .chunks_exact(2)
            .map(|c| (c[0].trim().to_string(), c[1].to_string()))
            .collect())
    }
}

/// First 7 chars of a sha, for copy that has to stay readable. Slices on a
/// char boundary by construction — a sha is ASCII hex.
fn short_sha(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

/// Pull the sha out of git's drop confirmation,
/// `Dropped stash@{0} (<sha>)` — or `Dropped refs/stash@{0} (<sha>)` on some
/// versions. Takes the last parenthesized run of hex so neither the ref
/// spelling nor surrounding chatter matters.
fn parse_dropped_sha(stdout: &str) -> Option<String> {
    let open = stdout.rfind('(')?;
    let rest = &stdout[open + 1..];
    let close = rest.find(')')?;
    let sha = rest[..close].trim();
    if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
    }
}

/// Parse `git stash list -z --format=%H%x00%ct%x00%cr%x00%gs`.
///
/// Four NUL-separated fields per record — sha, commit timestamp, git's own
/// relative-date string, reflog subject — with `-z` NUL-terminating each
/// record. So the whole stream is just NUL-delimited tokens, read four at a
/// time.
///
/// **NUL is the only delimiter a message cannot forge.** Every printable
/// candidate is reachable: a stash message legally contains `|`, `:`, newlines
/// — and, verified, raw `\x1f` and `\x1e` too, which git round-trips
/// untouched. A crafted message could therefore inject a whole extra record,
/// silently shifting every positional index below it so that a Drop destroyed
/// a stash the user never selected. NUL closes this structurally rather than
/// heuristically: git refuses to write one
/// (`error: a NUL byte in commit log message not allowed`) and it cannot even
/// survive argv. It is also this crate's uniform convention already
/// (`branch_diff.rs`, `log.rs`, `numstat.rs`).
///
/// The reflog subject normally reads `On <branch>: <message>` or
/// `WIP on <branch>: <message>`. **The prefix is not guaranteed.** Verified: an
/// entry created by `git stash store -m <msg>` records `<msg>` verbatim with no
/// branch at all. A missing prefix is therefore data — `branch = ""`, whole
/// string as the message — and never a parse error.
///
/// The index is positional, which is safe precisely because the record count
/// is now un-forgeable: `%gd` would give `stash@{N}`, but `N` is just this
/// record's position, and deriving it keeps the two from ever disagreeing.
pub(crate) fn parse_stash_list(text: &str) -> Result<Vec<StashEntry>> {
    // Empty stack: `-z` emits zero bytes (verified).
    if text.is_empty() {
        return Ok(Vec::new());
    }
    // `git log -z` is documented as *separating* records with NUL, but what it
    // actually does is *terminate* each one — so a trailing NUL is present in
    // practice. Strip at most one, rather than popping a trailing empty token:
    // that is a no-op on the documented "separate" shape, so both behaviours
    // parse, and it stays correct when the final record's `%gs` is
    // legitimately empty (where an unconditional pop would miscount).
    let body = text.strip_suffix('\0').unwrap_or(text);
    let tokens: Vec<&str> = body.split('\0').collect();
    // Fails CLOSED. If a git build ever ignored `-z` here, records would be
    // newline-separated, the count would not divide evenly, and this errors
    // loudly instead of silently mis-indexing the stack — which is the failure
    // that destroys the wrong stash. Loud beats subtle for a destructive path.
    if !tokens.len().is_multiple_of(FIELDS_PER_RECORD) {
        return Err(GitError::parse(format!(
            "`git stash list` returned {} fields, not a multiple of \
             {FIELDS_PER_RECORD} — is `-z` being honoured?",
            tokens.len()
        )));
    }

    let mut out = Vec::with_capacity(tokens.len() / FIELDS_PER_RECORD);
    for chunk in tokens.chunks_exact(FIELDS_PER_RECORD) {
        let [sha, ct, relative, subject] = chunk else {
            unreachable!("chunks_exact({FIELDS_PER_RECORD}) yields {FIELDS_PER_RECORD} items")
        };
        let sha = sha.trim();
        // `%H` is always a full object name: 40 hex (SHA-1) or 64 (SHA-256).
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(GitError::parse(format!(
                "stash list record {}: cannot parse sha {sha:?}",
                out.len()
            )));
        }
        let created_at = ct.trim().parse::<i64>().map_err(|e| {
            GitError::parse(format!(
                "stash list record {}: bad timestamp {ct:?}: {e}",
                out.len()
            ))
        })?;

        let (branch, message) = split_branch_prefix(subject);
        out.push(StashEntry {
            stash_ref: StashRef { index: out.len() },
            branch,
            message,
            sha: sha.to_string(),
            created_at,
            relative: relative.trim().to_string(),
        });
    }
    Ok(out)
}

/// Split a reflog subject into `(branch, message)`.
///
/// `WIP on main: abc123 subject` → `("main", "abc123 subject")`
/// `On main: my message`         → `("main", "my message")`
/// `stored literal message`      → `("", "stored literal message")`
///
/// Only the branch name is cut at the first `": "`; everything after it is the
/// message, colons and all.
fn split_branch_prefix(subject: &str) -> (String, String) {
    let body = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "));
    match body.and_then(|b| b.split_once(": ")) {
        Some((branch, message)) => (branch.to_string(), message.to_string()),
        // Either no `On `/`WIP on ` prefix (a `stash store` entry), or a
        // prefix with no `: ` to close it. Both keep the subject whole.
        None => (String::new(), subject.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one NUL-separated record, NUL-terminated, exactly as
    /// `git stash list -z --format=STASH_LIST_FORMAT` emits it.
    fn rec(sha: &str, ct: &str, rel: &str, subject: &str) -> String {
        format!("{sha}\0{ct}\0{rel}\0{subject}\0")
    }

    const SHA_A: &str = "c8ff104cb8494345ff53d9eb88421d7a03a993b6";
    const SHA_B: &str = "b83b3f6aa1d44e0b9c2f5e8d7a6b4c3d2e1f0a9b";

    #[test]
    fn parse_empty_list() {
        // `-z` on an empty stack emits nothing at all.
        assert_eq!(parse_stash_list("").unwrap().len(), 0);
    }

    #[test]
    fn parse_accepts_both_separate_and_terminate_shapes() {
        // `git log -z` is DOCUMENTED as separating records with NUL but
        // observed to terminate them. Parse both, so a git that implements the
        // documented wording literally does not eat the last record's message.
        let terminated = format!(
            "{}{}",
            rec(SHA_A, "1789853406", "now", "On main: first"),
            rec(SHA_B, "1789850000", "now", "On main: second"),
        );
        let separated = terminated.strip_suffix('\0').unwrap();

        for (shape, text) in [("terminated", terminated.as_str()), ("separated", separated)] {
            let entries = parse_stash_list(text).unwrap_or_else(|e| panic!("{shape}: {e}"));
            assert_eq!(entries.len(), 2, "{shape}");
            assert_eq!(entries[0].message, "first", "{shape}");
            assert_eq!(entries[1].message, "second", "{shape}: last record truncated");
        }
    }

    #[test]
    fn parse_keeps_a_trailing_empty_message() {
        // The last record's `%gs` being empty looks exactly like the stream
        // terminator. Stripping at most one trailing NUL keeps them distinct;
        // popping every trailing empty token would drop this record's message.
        let text = format!(
            "{}{}",
            rec(SHA_A, "1789853406", "now", "On main: first"),
            rec(SHA_B, "1789850000", "now", ""),
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 2, "got {entries:#?}");
        assert_eq!(entries[1].message, "");
        assert_eq!(entries[1].sha, SHA_B);
    }

    #[test]
    fn parse_rejects_a_truncated_record() {
        // Fields must arrive in complete groups of four; a short tail means
        // the stream was cut, which must surface rather than parse partially.
        let truncated = format!("{SHA_A}\x001789853406\0");
        assert!(parse_stash_list(&truncated).is_err());
    }

    #[test]
    fn parse_keeps_an_empty_message_field() {
        // An empty trailing field is real data, not the stream terminator.
        // The old "skip empty pieces" splitter could not tell them apart.
        let text = rec(SHA_A, "1789853406", "now", "");
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "");
        assert_eq!(entries[0].branch, "");
    }

    #[test]
    fn parse_default_wip_messages() {
        let text = format!(
            "{}{}",
            rec(SHA_A, "1789853406", "2 hours ago", "WIP on main: abc1234 commit subject"),
            rec(SHA_B, "1789850000", "3 hours ago", "WIP on feature: def5678 another subject"),
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].stash_ref.index, 0);
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "abc1234 commit subject");
        assert_eq!(entries[0].sha, SHA_A);
        assert_eq!(entries[0].created_at, 1789853406);
        assert_eq!(entries[0].relative, "2 hours ago");
        assert_eq!(entries[1].stash_ref.index, 1);
        assert_eq!(entries[1].branch, "feature");
        assert_eq!(entries[1].sha, SHA_B);
    }

    #[test]
    fn parse_custom_message() {
        let text = rec(SHA_A, "1789853406", "1 second ago", "On main: my custom message");
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "my custom message");
    }

    #[test]
    fn parse_message_with_colons() {
        // Only the branch prefix is cut; colons in the message survive.
        let text = rec(
            SHA_A,
            "1789853406",
            "1 second ago",
            "On main: feat(api): handle nested colons",
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].message, "feat(api): handle nested colons");
    }

    #[test]
    fn parse_message_with_pipe_and_newline() {
        // The old parser split on `: ` over `\n`-delimited lines, so a message
        // containing a newline became two bogus records. `\x1e` records and
        // `\x1f` fields make both legal characters inert.
        let text = rec(
            SHA_A,
            "1789853406",
            "1 second ago",
            "On main: msg with | pipe and\nan embedded newline",
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1, "one record, not two");
        assert_eq!(
            entries[0].message,
            "msg with | pipe and\nan embedded newline"
        );
    }

    #[test]
    fn parse_message_containing_the_old_record_separator() {
        // `\x1e` used to delimit records, and git round-trips it verbatim, so
        // one such entry failed the whole list. Under NUL it is ordinary text.
        let text = format!(
            "{}{}",
            rec(SHA_A, "1789853406", "now", "On main: evil\x1erecord"),
            rec(SHA_B, "1789850000", "now", "On main: innocent"),
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 2, "got {entries:#?}");
        assert_eq!(entries[0].message, "evil\x1erecord", "separator preserved");
        assert_eq!(entries[0].sha, SHA_A);
        // The neighbour must survive intact — that is the whole point.
        assert_eq!(entries[1].message, "innocent");
        assert_eq!(entries[1].sha, SHA_B);
        assert_eq!(entries[1].stash_ref.index, 1);
    }

    #[test]
    fn parse_message_cannot_forge_a_record_boundary() {
        // The worst case under the old scheme: a message carrying a full
        // 40-hex object name and a valid timestamp between separators. It
        // parsed as a REAL extra record, silently shifting every index below
        // it — so Drop destroyed a stash the user never selected. No content
        // check could tell it from a genuine record; only a delimiter the
        // message cannot contain can.
        let text = format!(
            "{}{}",
            rec(
                SHA_A,
                "1789853406",
                "now",
                &format!("On main: craft\x1e{SHA_B}\x1f1700000000\x1f9 days ago\x1fOn main: PHANTOM"),
            ),
            rec(SHA_B, "1789850000", "now", "On main: innocent"),
        );
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 2, "no phantom record: {entries:#?}");
        assert_eq!(
            entries[0].message,
            format!("craft\x1e{SHA_B}\x1f1700000000\x1f9 days ago\x1fOn main: PHANTOM")
        );
        // Indices stay true to git, so a Drop cannot land on the wrong stash.
        assert_eq!(entries[0].stash_ref.index, 0);
        assert_eq!(entries[1].message, "innocent");
        assert_eq!(entries[1].stash_ref.index, 1);
    }

    #[test]
    fn parse_accepts_a_sha256_object_name() {
        // `--object-format=sha256` yields a 64-char %H. It must still be
        // recognized as a record header.
        let sha256 = "a".repeat(64);
        let text = rec(&sha256, "1789853406", "now", "On main: wide");
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sha, sha256);
    }

    #[test]
    fn parse_message_containing_the_old_unit_separator() {
        // Likewise `\x1f`, which used to separate fields.
        let text = rec(SHA_A, "1789853406", "now", "On main: evil\x1funit");
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "evil\x1funit");
    }

    #[test]
    fn parse_rejects_bad_sha() {
        let text = rec("not-a-sha", "1789853406", "now", "On main: x");
        assert!(parse_stash_list(&text).is_err());
    }

    #[test]
    fn parse_rejects_bad_timestamp() {
        let text = rec(SHA_A, "not-a-number", "now", "On main: x");
        assert!(parse_stash_list(&text).is_err());
    }

    #[test]
    fn parse_stored_entry_has_no_branch() {
        // Verified: `git stash store -m "solo literal msg"` records the
        // message with NO `On <branch>: ` prefix. That is data, not an error —
        // the old parser rejected it as "missing branch field".
        let text = rec(SHA_A, "1789853406", "now", "solo literal msg");
        let entries = parse_stash_list(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch, "");
        assert_eq!(entries[0].message, "solo literal msg");
    }

    #[test]
    fn index_is_positional() {
        let text = format!(
            "{}{}{}",
            rec(SHA_A, "3", "now", "On main: a"),
            rec(SHA_B, "2", "now", "On main: b"),
            rec(SHA_A, "1", "now", "On main: c"),
        );
        let entries = parse_stash_list(&text).unwrap();
        let idx: Vec<usize> = entries.iter().map(|e| e.stash_ref.index).collect();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn dropped_sha_parsed_from_git_stdout() {
        // Verified real output.
        let out = format!("Dropped stash@{{0}} ({SHA_A})\n");
        assert_eq!(parse_dropped_sha(&out).as_deref(), Some(SHA_A));
    }

    #[test]
    fn dropped_sha_tolerates_full_ref_spelling() {
        let out = format!("Dropped refs/stash@{{2}} ({SHA_B})\n");
        assert_eq!(parse_dropped_sha(&out).as_deref(), Some(SHA_B));
    }

    #[test]
    fn dropped_sha_absent_is_none() {
        assert_eq!(parse_dropped_sha(""), None);
        assert_eq!(parse_dropped_sha("Dropped stash@{0}\n"), None);
        // Parenthesized but not hex — must not be mistaken for a sha.
        assert_eq!(parse_dropped_sha("Dropped stash@{0} (nope)\n"), None);
    }

    #[test]
    fn stash_ref_renders_braced() {
        assert_eq!(StashRef { index: 0 }.ref_string(), "stash@{0}");
        assert_eq!(StashRef { index: 12 }.ref_string(), "stash@{12}");
    }
}
