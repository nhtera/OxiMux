//! Integration tests for `Repository::stash_rename` — the composed verb git
//! does not have.
//!
//! Its own file rather than another section of `stash_ops.rs`: rename is the
//! one stash op that mutates entries it was not asked about (everything above
//! the target comes off and goes back), so what has to be proved is not "the
//! message changed" but "nothing else did".

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;

/// `stash@{N}: <subject>` for every entry, top first — the raw reflog subject,
/// which is what the panel parses and what a rename has to leave alone for
/// every entry but one.
fn subjects(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["stash", "list", "--format=%gs"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn shas(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["stash", "list", "--format=%H"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

/// Commit timestamps, top first. The rename must not touch them: `store`
/// writes a reflog pointer, never a commit, so `%ct` is the stash's own.
fn timestamps(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["stash", "list", "--format=%ct"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

/// A repo on `main` with a base commit.
fn seed(p: &Path) {
    init_repo(p);
    write(&p.join("a.txt"), "base\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
}

/// Push `n` stashes, oldest first, each named `s0`…`s{n-1}` and each touching
/// its own file so they are distinct commits. Returns nothing: the test reads
/// the stack back through `shas`.
fn push_stack(p: &Path, n: usize) {
    for i in 0..n {
        write(&p.join(format!("f{i}.txt")), &format!("v{i}\n"));
        run_git(p, &["add", "-A"]);
        run_git(p, &["stash", "push", "-m", &format!("s{i}")]);
    }
}

// ── The headline claim: rename in the middle, nothing else moves ──────────

#[tokio::test]
async fn renaming_a_mid_stack_stash_leaves_it_at_its_own_index() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    // 5 pushes ⇒ s4 on top, s0 at the bottom. `stash@{3}` is therefore `s1`.
    push_stack(p, 5);

    let before_shas = shas(p);
    let before_times = timestamps(p);
    let target = before_shas[3].clone();

    let repo = Repository::open(p).await.unwrap();
    let failures = repo.stash_rename(&target, "renamed").await.unwrap();
    assert!(failures.is_empty(), "unexpected restore failures: {failures:?}");

    // Same shas, same order — the whole point of the drop/store dance.
    assert_eq!(shas(p), before_shas, "the stack was reordered");
    // `store` writes a reflog pointer, not a commit, so dates are untouched.
    assert_eq!(timestamps(p), before_times, "commit dates changed");
    assert_eq!(
        subjects(p),
        vec![
            "On main: s4".to_string(),
            "On main: s3".to_string(),
            "On main: s2".to_string(),
            // Step 4 re-applies the `On <branch>: ` prefix itself — `store`
            // writes the message literally and would otherwise leave this one
            // entry shaped unlike every other.
            "On main: renamed".to_string(),
            "On main: s0".to_string(),
        ],
    );
}

#[tokio::test]
async fn renaming_the_top_of_the_stack_touches_nothing_below_it() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 3);

    let before = shas(p);
    let repo = Repository::open(p).await.unwrap();
    repo.stash_rename(&before[0], "top").await.unwrap();

    assert_eq!(shas(p), before);
    assert_eq!(
        subjects(p),
        vec![
            "On main: top".to_string(),
            "On main: s1".to_string(),
            "On main: s0".to_string(),
        ],
    );
}

#[tokio::test]
async fn renaming_the_only_stash_works() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 1);

    let before = shas(p);
    let repo = Repository::open(p).await.unwrap();
    repo.stash_rename(&before[0], "solo").await.unwrap();

    assert_eq!(shas(p), before);
    assert_eq!(subjects(p), vec!["On main: solo".to_string()]);
}

// ── The renamed entry is still a usable stash ─────────────────────────────

#[tokio::test]
async fn a_renamed_stash_still_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "changed\n");
    run_git(p, &["stash", "push", "-m", "wip"]);

    let repo = Repository::open(p).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    repo.stash_rename(&sha, "still good").await.unwrap();

    // Re-resolve: the address is the same here, but reading it back is the
    // point — a rename that produced an entry `stash_list` cannot parse would
    // show up as an empty list rather than an error.
    let entries = repo.stash_list(true).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].message, "still good");
    assert_eq!(entries[0].branch, "main");
    assert_eq!(entries[0].sha, sha, "rename must not rewrite the commit");

    repo.stash_apply(&entries[0].stash_ref, false).await.unwrap();
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "changed\n");
}

#[tokio::test]
async fn an_entry_written_by_store_has_no_branch_and_is_not_given_one() {
    // `git stash store -m x` records `x` verbatim — no `On <branch>: `. A
    // rename must not invent a branch for such an entry: the prefix it would
    // write is a claim about where the work came from, and there is none.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "changed\n");
    run_git(p, &["stash", "push", "-m", "wip"]);

    let repo = Repository::open(p).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    // Drop and re-store so the entry carries a bare, prefix-less subject.
    repo.stash_drop(&oximux_core::StashRef { index: 0 })
        .await
        .unwrap();
    repo.stash_store(&sha, "bare subject").await.unwrap();
    assert_eq!(subjects(p), vec!["bare subject".to_string()]);

    repo.stash_rename(&sha, "still bare").await.unwrap();
    assert_eq!(subjects(p), vec!["still bare".to_string()]);

    // And it still parses: a missing prefix is data, not a parse error.
    let entries = repo.stash_list(true).await.unwrap();
    assert_eq!(entries[0].message, "still bare");
    assert_eq!(entries[0].branch, "");
}

#[tokio::test]
async fn a_wip_prefix_is_not_silently_rewritten_on_the_entries_around_it() {
    // A plain `git stash` (no `-m`) records `WIP on <branch>: <sha> <subject>`.
    // Step 5 puts subjects back VERBATIM, so a neighbour's `WIP on` must
    // survive — reassembling it from `StashEntry`'s split (branch, message)
    // would quietly turn it into `On main:`.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "one\n");
    run_git(p, &["stash"]); // WIP-shaped, ends up at the bottom
    write(&p.join("a.txt"), "two\n");
    run_git(p, &["stash", "push", "-m", "named"]);

    let before = subjects(p);
    assert!(before[1].starts_with("WIP on main:"), "{before:?}");

    let repo = Repository::open(p).await.unwrap();
    let top = shas(p)[0].clone();
    repo.stash_rename(&top, "renamed").await.unwrap();

    let after = subjects(p);
    assert_eq!(after[0], "On main: renamed");
    assert_eq!(after[1], before[1], "the WIP subject was rewritten");
}

// ── Concurrency: an intruder pushed mid-loop is not destroyed ─────────────

#[tokio::test]
async fn a_stash_pushed_mid_rename_survives() {
    // The claim that justifies resolving by sha on every iteration rather
    // than dropping `stash@{0}` N+1 times. The hook fires between drops, so
    // the intruder lands exactly where a fixed-index loop would delete it.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 3);
    let target = shas(p)[2].clone(); // the bottom entry, `s0`

    let repo = Repository::open(p).await.unwrap();
    let fired = std::cell::Cell::new(false);
    let failures = repo
        .stash_rename_hooked(&target, "renamed", || async {
            // Once, after the first drop — a terminal in another window.
            if fired.replace(true) {
                return;
            }
            write(&p.join("intruder.txt"), "x\n");
            run_git(p, &["add", "-A"]);
            run_git(p, &["stash", "push", "-m", "intruder"]);
        })
        .await
        .unwrap();
    assert!(failures.is_empty(), "{failures:?}");

    let after = subjects(p);
    assert!(
        after.iter().any(|s| s.ends_with("intruder")),
        "the intruder was destroyed: {after:?}",
    );
    // Every original entry is still there, in order, with the rename applied.
    assert_eq!(
        after
            .iter()
            .filter(|s| !s.ends_with("intruder"))
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "On main: s2".to_string(),
            "On main: s1".to_string(),
            "On main: renamed".to_string(),
        ],
    );
    // Documented, not claimed fixed: the intruder is REORDERED. It was pushed
    // to the top and settles below the restored entries, because step 5 puts
    // them back on top of whatever the stack holds by then.
    assert_eq!(
        after.last().map(String::as_str),
        Some("On main: intruder"),
        "the intruder's resting position changed: {after:?}",
    );
}

// ── Refusals ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn renaming_a_sha_that_is_not_on_the_stack_is_refused_and_mutates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 2);
    let before = subjects(p);

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .stash_rename("0000000000000000000000000000000000000000", "nope")
        .await
        .expect_err("a sha that is not on the stack must be refused");
    assert!(
        err.to_string().contains("no longer on the stack"),
        "{err}",
    );
    assert_eq!(subjects(p), before, "a refused rename touched the stack");
}

#[tokio::test]
async fn renaming_on_an_empty_stack_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    let repo = Repository::open(p).await.unwrap();
    assert!(
        repo.stash_rename("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "x")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_message_containing_shell_metacharacters_round_trips() {
    // The recovery log deliberately does not render `-m <msg>`, because a
    // message legally contains quotes and `$`. The message itself still has to
    // survive the store/read round trip untouched.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 1);
    let sha = shas(p)[0].clone();

    let nasty = r#"fix "quoted" $VAR and \back\slash"#;
    let repo = Repository::open(p).await.unwrap();
    repo.stash_rename(&sha, nasty).await.unwrap();

    let entries = repo.stash_list(true).await.unwrap();
    assert_eq!(entries[0].message, nasty);
}

#[tokio::test]
async fn a_stash_dropped_out_from_under_the_rename_is_skipped_not_aborted() {
    // Step 3's `resolve → None` branch: something else removed an entry the
    // rename was going to take off. The loop must not treat that as a failure
    // — the entry is simply not ours to put back any more.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    push_stack(p, 3); // s2 on top, s0 at the bottom
    let all = shas(p);
    let target = all[2].clone(); // rename the bottom entry
    let victim = all[1].clone(); // and have something drop the middle one

    let repo = Repository::open(p).await.unwrap();
    let fired = std::cell::Cell::new(false);
    let failures = repo
        .stash_rename_hooked(&target, "renamed", || async {
            // After the first drop (s2), remove s1 the way a terminal would.
            if fired.replace(true) {
                return;
            }
            run_git(p, &["stash", "drop", "stash@{0}"]);
        })
        .await
        .unwrap();
    assert!(failures.is_empty(), "{failures:?}");

    let after = subjects(p);
    assert!(
        after.contains(&"On main: renamed".to_string()),
        "the rename did not land: {after:?}",
    );
    // The commit object survives a reflog drop, so step 5's `store` puts the
    // victim back rather than failing — the entry is recovered, not lost. Its
    // POSITION is what the concurrent drop cost: it returns wherever the
    // reverse-order restore leaves it.
    assert!(
        shas(p).contains(&victim),
        "the concurrently-dropped entry was not recovered: {after:?}",
    );
}
