//! Which credential a process presents, as decided by its environment.
//!
//! A test binary of its own, holding exactly ONE test, and that is the whole
//! point. `std::env::set_var` mutates process-global state — unsafe in Rust
//! 2024 precisely because a concurrent `getenv` on another thread can observe a
//! reallocated `environ`. libtest runs a binary's tests on concurrent threads,
//! and this crate's other tests call `tempfile::tempdir()`, which reads
//! `TMPDIR` through `std::env::temp_dir()`. Sharing a binary with them was a
//! real data race; a binary with a single test has no second thread to race.
//!
//! Keep it that way: adding a second `#[test]` here reintroduces the hazard.

use oximux_remote_local::{
    LocalIdentity, SESSION_ENV_VAR, SESSION_TOKEN_ENV_VAR, credential, token_path,
    write_token_file,
};

/// The three cases in one test for the same reason they are in one binary:
/// they share process-global state, so they must not run beside each other.
#[test]
fn credential_follows_the_environment_and_never_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path();
    write_token_file(&token_path(runtime_dir), "operator-secret").unwrap();

    // SAFETY: this binary holds one test, so nothing else is running.
    let clear = || unsafe {
        std::env::remove_var(SESSION_ENV_VAR);
        std::env::remove_var(SESSION_TOKEN_ENV_VAR);
    };

    // No agent variables: the operator credential from the token file.
    clear();
    let (identity, secret) = credential(runtime_dir).unwrap();
    assert_eq!(identity, LocalIdentity::Operator);
    assert_eq!(secret, "operator-secret");

    // Both agent variables: that session's credential, never the file's.
    unsafe {
        std::env::set_var(SESSION_ENV_VAR, "sess-7");
        std::env::set_var(SESSION_TOKEN_ENV_VAR, "session-secret");
    }
    let (identity, secret) = credential(runtime_dir).unwrap();
    assert_eq!(identity, LocalIdentity::Session("sess-7".into()));
    assert_eq!(secret, "session-secret");

    // Session id without its secret: refused outright. Falling back to the
    // operator token here would hand an agent the operator's authority
    // whenever its credential failed to arrive.
    unsafe { std::env::remove_var(SESSION_TOKEN_ENV_VAR) };
    let err = credential(runtime_dir).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains(SESSION_TOKEN_ENV_VAR));

    clear();
}
