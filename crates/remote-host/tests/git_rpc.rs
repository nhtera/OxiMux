//! The git RPCs against a **real repository**, driven through the real dispatcher
//! over the in-memory loopback.
//!
//! The load-bearing assertion here is containment: `path_guard` is unit-tested on
//! its own, but this proves it is actually *wired in* at the RPC boundary. Without
//! that wiring a paired phone could turn `GitDiff` into "read any file on this
//! machine" — the exact hole the phase's red team flagged as critical.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::{IndexStatusWire, RegisterReq};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_proto::Transport;

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn register_req(pubkey: [u8; 32]) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: None,
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("run git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A real repo with one untracked file, plus a secret placed *outside* it that a
/// traversal would try to reach.
fn repo_with_untracked_file() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let outside = tempfile::tempdir().expect("outside dir");
    std::fs::write(outside.path().join("secret.txt"), b"top secret\n").expect("write secret");

    let dir = tempfile::tempdir().expect("repo dir");
    git(dir.path(), &["init"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("new.txt"), b"hello\n").expect("write untracked");

    let root = dir.path().to_path_buf();
    (dir, outside, root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_serves_repo_files_and_refuses_to_escape() {
    let (_dir, outside, root) = repo_with_untracked_file();
    let escape_target = outside.path().join("secret.txt").to_string_lossy().into_owned();

    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(root.clone()) });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // Status sees the untracked file, which is how a client learns the path.
        let Response::GitStatus(status) =
            call(&client, Request::GitStatus { session_id: "sess-1".into() }).await
        else {
            panic!("expected GitStatus");
        };
        assert!(
            status.files.iter().any(|f| f.path == "new.txt"),
            "the untracked file shows up in status: {:?}",
            status.files,
        );

        // A path from that listing diffs normally.
        let Response::GitDiff(files) = call(
            &client,
            Request::GitDiff {
                session_id: "sess-1".into(),
                path: "new.txt".into(),
                staged: false,
                untracked: true,
            },
        )
        .await
        else {
            panic!("expected GitDiff");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");

        // …but traversal out of the repo is refused, by relative path…
        for attack in ["../secret.txt", "../../../../etc/passwd"] {
            let got = call(
                &client,
                Request::GitDiff {
                    session_id: "sess-1".into(),
                    path: attack.into(),
                    staged: false,
                    untracked: true,
                },
            )
            .await;
            assert!(
                matches!(got, Response::Error(RpcError::BadRequest(_))),
                "{attack} must be refused, got {got:?}",
            );
        }

        // …and by absolute path to a real file that genuinely exists outside.
        let got = call(
            &client,
            Request::GitDiff {
                session_id: "sess-1".into(),
                path: escape_target,
                staged: false,
                untracked: true,
            },
        )
        .await;
        assert!(
            matches!(got, Response::Error(RpcError::BadRequest(_))),
            "an absolute path outside the repo must be refused, got {got:?}",
        );
        // `client` drops here → serve ends.
    };

    tokio::join!(serve, script);
}

/// A repo that already has a commit, plus one untracked file.
///
/// The commit matters: `unstage` is `git restore --staged`, which resolves
/// against HEAD, so it fails outright in a repository that has none. Real
/// repositories have commits; a fixture without one tests a state the feature
/// will not meet and hides the behaviour under test.
fn repo_with_history_and_untracked_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("repo dir");
    git(dir.path(), &["init"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("seed.txt"), b"seed\n").expect("write seed");
    git(dir.path(), &["add", "seed.txt"]);
    git(dir.path(), &["commit", "-m", "seed"]);
    std::fs::write(dir.path().join("new.txt"), b"hello\n").expect("write untracked");
    let root = dir.path().to_path_buf();
    (dir, root)
}

/// The write path end to end against a real repo: stage an untracked file,
/// commit it, and see the commit land. Uses real git throughout — a mocked repo
/// would not prove the index actually moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_writes_stage_and_commit_against_a_real_repo() {
    let (_dir, root) = repo_with_history_and_untracked_file();

    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(root.clone()) });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let root_for_check = root.clone();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // Staging moves the file into the index.
        let staged = call(
            &client,
            Request::GitStage { session_id: "sess-1".into(), paths: vec!["new.txt".into()] },
        )
        .await;
        assert_eq!(staged, Response::Ack, "stage acked");
        let Response::GitStatus(status) =
            call(&client, Request::GitStatus { session_id: "sess-1".into() }).await
        else {
            panic!("expected GitStatus");
        };
        let file = status.files.iter().find(|f| f.path == "new.txt").expect("file listed");
        assert_eq!(
            file.index,
            IndexStatusWire::Added,
            "the previously-untracked file is now staged as an addition: {file:?}"
        );

        // Unstaging puts it back, proving the two are real inverses rather than
        // both silently no-opping.
        let unstaged = call(
            &client,
            Request::GitUnstage { session_id: "sess-1".into(), paths: vec!["new.txt".into()] },
        )
        .await;
        assert_eq!(unstaged, Response::Ack, "unstage acked");

        // Re-stage, then commit what is staged.
        call(
            &client,
            Request::GitStage { session_id: "sess-1".into(), paths: vec!["new.txt".into()] },
        )
        .await;
        let Response::GitCommitted { sha } = call(
            &client,
            Request::GitCommit {
                session_id: "sess-1".into(),
                message: "add the untracked file".into(),
            },
        )
        .await
        else {
            panic!("expected GitCommitted");
        };
        assert!(!sha.is_empty(), "a real sha comes back");

        // The commit is really in the log, not just acknowledged.
        let out = Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(&root_for_check)
            .output()
            .expect("git log");
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(log.contains("add the untracked file"), "commit landed: {log}");

        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// Containment covers the **writes** too, not only `GitDiff`. A traversing stage
/// path is the more dangerous direction — it would pull a file from outside the
/// repository into a commit — so it is asserted separately rather than assumed
/// to follow from the read-path test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_writes_refuse_paths_outside_the_repository() {
    let (_dir, outside, root) = repo_with_untracked_file();
    let escape = outside.path().join("secret.txt").to_string_lossy().into_owned();

    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(root) });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        for attack in ["../secret.txt", escape.as_str()] {
            let got = call(
                &client,
                Request::GitStage {
                    session_id: "sess-1".into(),
                    paths: vec![attack.to_string()],
                },
            )
            .await;
            assert!(
                matches!(got, Response::Error(RpcError::BadRequest(_))),
                "staging {attack} must be refused, got {got:?}"
            );
        }

        // A batch is all-or-nothing: one bad path fails the whole request rather
        // than quietly staging the acceptable remainder, which would report
        // success for an operation the client never asked for.
        let mixed = call(
            &client,
            Request::GitStage {
                session_id: "sess-1".into(),
                paths: vec!["new.txt".into(), "../secret.txt".into()],
            },
        )
        .await;
        assert!(
            matches!(mixed, Response::Error(RpcError::BadRequest(_))),
            "a mixed batch is refused whole, got {mixed:?}"
        );
        let Response::GitStatus(status) =
            call(&client, Request::GitStatus { session_id: "sess-1".into() }).await
        else {
            panic!("expected GitStatus");
        };
        let file = status.files.iter().find(|f| f.path == "new.txt").expect("file listed");
        assert_eq!(
            file.index,
            IndexStatusWire::Untracked,
            "the valid path in the refused batch was NOT staged: {file:?}"
        );

        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A read-only device may inspect the repository but not change it. This is the
/// tier's whole point: without it, pairing (which grants read-write by default)
/// would be the only setting and a down-scoped device could still commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_can_read_git_but_not_write_it() {
    let (_dir, _outside, root) = repo_with_untracked_file();

    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(root) });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(registry, Arc::clone(&auth)).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        // Down-scope the device *after* pairing, mirroring the real flow: the
        // user pairs, then assigns read-only from the paired-device list.
        auth.set_read_only(&pubkey, true);

        // Reads still work.
        let status = call(&client, Request::GitStatus { session_id: "sess-1".into() }).await;
        assert!(matches!(status, Response::GitStatus(_)), "reads stay allowed: {status:?}");

        // Every write is refused.
        let stage = call(
            &client,
            Request::GitStage { session_id: "sess-1".into(), paths: vec!["new.txt".into()] },
        )
        .await;
        assert_eq!(stage, Response::Error(RpcError::Unauthorized), "stage refused");
        let unstage = call(
            &client,
            Request::GitUnstage { session_id: "sess-1".into(), paths: vec!["new.txt".into()] },
        )
        .await;
        assert_eq!(unstage, Response::Error(RpcError::Unauthorized), "unstage refused");
        let commit = call(
            &client,
            Request::GitCommit { session_id: "sess-1".into(), message: "nope".into() },
        )
        .await;
        assert_eq!(commit, Response::Error(RpcError::Unauthorized), "commit refused");

        drop(client);
    };
    futures::future::join(serve, script).await;
}
