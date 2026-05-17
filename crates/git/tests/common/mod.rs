//! Shared helpers for integration tests that drive real `git` against a
//! tempdir-backed repo. `tests/common/mod.rs` is excluded from cargo's
//! test-target discovery (the `/mod.rs` form), so this is not a runnable
//! test file by itself.
//!
//! MSRV-git: 2.28+ (callers use `git init -b main`).

use std::path::Path;
use std::process::Command;

/// Run `git <args>` synchronously from `cwd`. Panics on non-zero exit so
/// callers can inline the call without `unwrap()` noise.
pub fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .expect("git not on PATH");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// `git init -b main` + disable commit signing so the tests work on
/// developer boxes that have `commit.gpgsign=true` globally.
pub fn init_repo(cwd: &Path) {
    run_git(cwd, &["init", "-b", "main"]);
    run_git(cwd, &["config", "commit.gpgsign", "false"]);
}

/// Convenience wrapper — same as `std::fs::write` but panics with a
/// readable message on failure.
pub fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write file");
}
