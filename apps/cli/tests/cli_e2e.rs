//! The compiled binary against a live host: a real dispatcher behind a real
//! owner-only socket, driven exactly as a user or an agent would drive it —
//! argv in, JSON out, exit codes as the contract says.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope};
use oximux_remote_local::{
    LocalClaim, LocalControlListener, generate_token, token_path, write_token_file,
};

fn bin(runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "10"]);
    // The test runner's own environment must not leak a credential in.
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// The binary as an agent child sees it: session id AND the per-session
/// secret the host minted for that agent.
fn agent_bin(runtime_dir: &Path, session_id: &str, secret: &str) -> Command {
    let mut cmd = bin(runtime_dir);
    cmd.env(oximux_remote_local::SESSION_ENV_VAR, session_id);
    cmd.env(oximux_remote_local::SESSION_TOKEN_ENV_VAR, secret);
    cmd
}

fn json_stdout(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// A host with two sessions, serving on a fresh runtime dir until dropped.
/// Returns the per-session secret minted for `sess-1`, as the host would hand
/// it to that agent's process at spawn.
fn serve_host(rt: &tokio::runtime::Runtime, runtime_dir: &Path) -> String {
    let registry = Arc::new(SessionRegistry::new());
    for id in ["sess-1", "sess-2"] {
        registry.register(id.into(), Arc::new(StubConnection::default()));
    }
    let dispatcher = Arc::new(Dispatcher::new(registry, Arc::new(AuthStore::new())));
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    let agent_secret = listener.grant_session("sess-1");
    rt.spawn(async move {
        loop {
            match listener.accept().await {
                Ok((transport, claim)) => {
                    let dispatcher = dispatcher.clone();
                    let scope = match claim {
                        LocalClaim::Operator => LocalScope::Full,
                        LocalClaim::Session(id) => LocalScope::Session(id),
                    };
                    tokio::spawn(async move {
                        dispatcher.serve_local(transport.as_ref(), scope).await;
                    });
                }
                Err(_) => continue,
            }
        }
    });
    agent_secret
}

/// `status` and `ls --json` against a live host: exit 0, honest counts —
/// and the same invocations from an agent-scoped environment see exactly one
/// session and are refused everything session-less.
#[test]
fn status_ls_and_scope_against_a_live_host() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let agent_secret = serve_host(&rt, &runtime_dir);

    // Operator: everything visible.
    let out = bin(&runtime_dir).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["sessions"]["total"], 2);

    let out = bin(&runtime_dir).args(["--json", "ls"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json_stdout(&out);
    assert_eq!(v["data"].as_array().unwrap().len(), 2);

    // Agent-scoped: holding the per-session secret narrows the same binary…
    let out = agent_bin(&runtime_dir, "sess-1", &agent_secret)
        .args(["--json", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json_stdout(&out);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "a scoped caller must not enumerate other sessions");
    assert_eq!(rows[0]["session_id"], "sess-1");

    // …and is refused the session-less surface, with the denied exit code.
    let out = agent_bin(&runtime_dir, "sess-1", &agent_secret)
        .args(["--json", "projects", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "denied is exit 5");
    let v = json_stdout(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "denied");
    assert!(!v["error"]["next_steps"].as_array().unwrap().is_empty());

    // The escalation an injected agent would attempt through the protocol:
    // present its own secret while naming a session it was not given. Denied —
    // the label is bound to a secret it does not hold.
    let out = agent_bin(&runtime_dir, "sess-2", &agent_secret)
        .args(["--json", "ls"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "wrong-session credential is denied");

    // An agent whose per-session secret failed to arrive does NOT quietly fall
    // back to the operator token file: it is refused, rather than running the
    // whole session with the operator's authority.
    let out = bin(&runtime_dir)
        .args(["--json", "ls"])
        .env(oximux_remote_local::SESSION_ENV_VAR, "sess-1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "no fallback to operator scope");
    let v = json_stdout(&out);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains(oximux_remote_local::SESSION_TOKEN_ENV_VAR),
        "the error names the missing credential: {v}"
    );
}

/// No host at the dir → exit 3 with actionable next steps; a rotated (stale)
/// token → exit 5. The two factors fail distinguishably.
#[test]
fn unreachable_is_3_and_stale_token_is_5() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // Nothing ever served here.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = bin(&empty).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "unreachable is exit 3");
    let v = json_stdout(&out);
    assert_eq!(v["error"]["code"], "unreachable");
    assert!(!v["error"]["next_steps"].as_array().unwrap().is_empty());

    // A live host whose on-disk token we rotate out from under the CLI.
    let runtime_dir = dir.path().join("host");
    serve_host(&rt, &runtime_dir);
    write_token_file(&token_path(&runtime_dir), &generate_token()).unwrap();
    let out = bin(&runtime_dir).args(["--json", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(5), "token mismatch is exit 5");
    assert_eq!(json_stdout(&out)["error"]["code"], "denied");
}

/// The offline verbs cost nothing: no host anywhere, still exit 0.
#[test]
fn offline_verbs_need_no_host() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(dir.path()).arg("version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));

    let out = bin(dir.path()).arg("agent-context").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"]["name"], "oximux");

    let out = bin(dir.path()).arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));

    // A typo is a usage error: exit 2, from clap itself.
    let out = bin(dir.path()).arg("no-such-verb").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
