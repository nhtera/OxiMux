//! The fleet surface against the compiled binary: the hosts book, the
//! resolution order, and the fan-out's partial-failure contract.
//!
//! Every invocation points `OXIMUX_CONFIG_DIR` at a temp directory. Without
//! that the suite would read — and `pair`/`hosts rm` would *write* — the
//! developer's real hosts file.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope};
use oximux_remote_local::{LocalClaim, LocalControlListener, generate_token};

/// The binary, with its config isolated and a short timeout so an unreachable
/// host fails in seconds rather than at the dial ceiling.
fn bin(config: &Path, runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "2"]);
    cmd.env(oximux_cli_config_var(), config);
    // The runner's own environment must not select a host or a credential.
    cmd.env_remove("OXIMUX_HOST");
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// Spelled out rather than imported: the binary crate is not a library, so the
/// constant it defines is not reachable from an integration test. The name is
/// pinned by `a_config_override_is_honoured` below.
fn oximux_cli_config_var() -> &'static str {
    "OXIMUX_CONFIG_DIR"
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

/// A minimal local host with one session, so the local path can be exercised.
fn serve_local(rt: &tokio::runtime::Runtime, runtime_dir: &Path) {
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    let dispatcher = Arc::new(Dispatcher::new(registry, Arc::new(AuthStore::new())));
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    rt.spawn(async move {
        loop {
            let Ok(pending) = listener.accept_pending().await else { return };
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let Ok((transport, claim)) = pending.authenticate().await else { return };
                let scope = match claim {
                    LocalClaim::Operator => LocalScope::Full,
                    LocalClaim::Session(id) => LocalScope::Session(id.into()),
                };
                dispatcher.serve_local(transport.as_ref(), scope).await;
            });
        }
    });
}

/// Write a hosts file naming a host that cannot be dialled. The endpoint id is
/// well-formed hex, so the failure is a real dial failure rather than a parse
/// one — which is what the fan-out's warning path has to survive.
fn write_unreachable_host(config: &Path, name: &str, default: bool) {
    std::fs::create_dir_all(config).unwrap();
    let mut toml = String::new();
    if default {
        toml.push_str(&format!("default = \"{name}\"\n"));
    }
    toml.push_str(&format!(
        "[[hosts]]\nname = \"{name}\"\nendpoint_id = \"{}\"\nread_only = false\n",
        "1a".repeat(32)
    ));
    std::fs::write(config.join("hosts.toml"), toml).unwrap();
}

/// The zero-config promise: a machine that has never paired talks to itself,
/// with no hosts file and nothing to configure.
#[test]
fn with_no_hosts_everything_talks_to_this_machine() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (config, runtime_dir) = (dir.path().join("config"), dir.path().join("host"));
    serve_local(&rt, &runtime_dir);

    let out = bin(&config, &runtime_dir).args(["--json", "ls"]).output().expect("run");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let rows = json_stdout(&out);
    assert_eq!(rows["data"].as_array().expect("rows").len(), 1);
    assert!(!config.join("hosts.toml").exists(), "and nothing was written");
}

#[test]
fn hosts_ls_on_a_fresh_machine_says_how_to_add_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(&dir.path().join("config"), dir.path())
        .args(["hosts", "ls"])
        .output()
        .expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("oximux pair"), "points at the next step: {text}");
}

/// A typo'd `--host` must be an error. Silently driving the machine the user is
/// sitting at instead of the one they named is the worst available outcome.
#[test]
fn an_unknown_host_is_a_usage_error_not_a_fallback_to_local() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (config, runtime_dir) = (dir.path().join("config"), dir.path().join("host"));
    serve_local(&rt, &runtime_dir);

    let out = bin(&config, &runtime_dir)
        .args(["--json", "--host", "prod", "ls"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2), "usage");
    let err = json_stdout(&out);
    assert_eq!(err["error"]["code"], "unknown-host");
    assert!(err["error"]["message"].as_str().unwrap().contains("prod"));
}

/// `OXIMUX_HOST` selects a host too, and is subject to the same rule.
#[test]
fn the_environment_can_select_a_host_and_a_bad_one_still_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let out = bin(&config, dir.path())
        .env("OXIMUX_HOST", "ghost")
        .args(["--json", "ls"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_stdout(&out)["error"]["code"], "unknown-host");
}

/// The fan-out's contract: an unreachable host becomes a warning row and the
/// reachable ones still print. A fleet view that failed whenever any host was
/// down would be unusable on a real fleet.
#[test]
fn all_hosts_reports_partial_results_and_never_drops_a_host() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (config, runtime_dir) = (dir.path().join("config"), dir.path().join("host"));
    serve_local(&rt, &runtime_dir);
    write_unreachable_host(&config, "offsite", false);

    let out = bin(&config, &runtime_dir)
        .args(["--json", "ls", "--all-hosts"])
        .output()
        .expect("run");
    assert!(out.status.success(), "partial results still exit 0");
    let rows = json_stdout(&out);
    let rows = rows["data"].as_array().expect("rows");
    assert!(
        rows.iter().any(|r| r["host"] == "local" && r["session_id"] == "sess-1"),
        "the reachable host's sessions are there: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["host"] == "offsite" && r["unreachable"] == true),
        "and the unreachable one is named, not dropped: {rows:?}"
    );
}

/// `--strict` is the scripting mode: the same fan-out, but a down host is a
/// failure with the unreachable-host exit code.
#[test]
fn all_hosts_strict_fails_when_a_host_is_down() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (config, runtime_dir) = (dir.path().join("config"), dir.path().join("host"));
    serve_local(&rt, &runtime_dir);
    write_unreachable_host(&config, "offsite", false);

    let out = bin(&config, &runtime_dir)
        .args(["--json", "ls", "--all-hosts", "--strict"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(3), "unreachable");
    let err = json_stdout(&out);
    assert_eq!(err["error"]["code"], "partial");
    assert!(
        err["error"]["next_steps"]
            .as_array()
            .expect("steps")
            .iter()
            .any(|s| s.as_str().unwrap_or_default().contains("offsite")),
        "and says which host: {err}"
    );
}

/// `--strict` without `--all-hosts` is rejected by clap, so it cannot look like
/// it did something on a single-host call.
#[test]
fn strict_requires_all_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(&dir.path().join("config"), dir.path())
        .args(["ls", "--strict"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn setting_a_default_requires_a_host_that_exists() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let out = bin(&config, dir.path())
        .args(["--json", "hosts", "default", "ghost"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_stdout(&out)["error"]["code"], "unknown-host");
}

/// Removing a host works even when the host cannot be reached — a laptop that
/// has finished with a host must not need the host's cooperation to say so —
/// and it says plainly that the host may still list this device.
#[test]
fn removing_an_unreachable_host_still_clears_it_locally_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    write_unreachable_host(&config, "offsite", true);

    let out = bin(&config, dir.path())
        .args(["--json", "hosts", "rm", "offsite"])
        .output()
        .expect("run");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let data = json_stdout(&out);
    assert_eq!(data["data"]["removed"], "offsite");
    assert_eq!(data["data"]["unpaired_host_side"], false, "honest about what it could not do");

    let after = bin(&config, dir.path()).args(["--json", "hosts", "ls"]).output().expect("run");
    assert!(json_stdout(&after)["data"].as_array().expect("rows").is_empty());
    // And the dangling default is gone with it, so a later bare call falls
    // back to local rather than naming a host that no longer exists.
    let toml = std::fs::read_to_string(config.join("hosts.toml")).unwrap();
    assert!(!toml.contains("default"), "the default was cleared: {toml}");
}

/// The config directory and the hosts file are owner-only after a write.
///
/// Same reason `oximux serve` hardens its data dir, and the same deployment:
/// a shared server is exactly where other accounts exist. The signing keys
/// beside this file were already 0600 and readback-verified, so what an open
/// directory leaked was not the credential but the fleet — which hosts this
/// account is paired with, under what names, at which endpoint ids.
///
/// `create_dir_all` applies the umask, which on a typical box leaves 0755, so
/// this only holds because the write path restricts it explicitly.
#[cfg(unix)]
#[test]
fn the_config_directory_and_hosts_file_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    write_unreachable_host(&config, "offsite", true);

    // A verb that WRITES the hosts file — the seeding helper above uses plain
    // `fs::write`, so asserting before this would test the fixture, not the code.
    let out = bin(&config, dir.path()).args(["--json", "hosts", "rm", "offsite"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let dir_mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "config dir is {dir_mode:o}, want 700");

    let file = config.join("hosts.toml");
    let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode & 0o077, 0, "hosts.toml is {file_mode:o} — readable off-owner");
}

#[test]
fn removing_a_host_that_was_never_paired_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(&dir.path().join("config"), dir.path())
        .args(["--json", "hosts", "rm", "ghost"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_stdout(&out)["error"]["code"], "unknown-host");
}

/// A malformed ticket is refused **without echoing it**. A paste that failed to
/// parse is still a credential-shaped string, and CI logs capture stderr.
#[test]
fn a_bad_ticket_is_refused_without_echoing_it_anywhere() {
    let dir = tempfile::tempdir().unwrap();
    let secretish = "AAAAsupersecretlookingvalueAAAA";
    let out = bin(&dir.path().join("config"), dir.path())
        .args(["--json", "pair", secretish])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let whole = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!whole.contains(secretish), "the ticket must not appear in output: {whole}");
}

/// The override is what keeps this suite off the developer's real hosts file;
/// pin its name so a rename cannot silently start writing there.
#[test]
fn a_config_override_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("elsewhere");
    write_unreachable_host(&config, "offsite", false);
    let out = bin(&config, dir.path()).args(["--json", "hosts", "ls"]).output().expect("run");
    assert!(out.status.success());
    let rows = json_stdout(&out);
    assert_eq!(rows["data"][0]["name"], "offsite", "read from the override, not the real dir");
}
