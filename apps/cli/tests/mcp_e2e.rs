//! `oximux mcp` end to end: the compiled binary, spawned as an MCP client
//! would spawn it, against a live host with a stand-in simulator — JSON-RPC
//! in on stdin, JSON-RPC (and nothing else) out on stdout.
//!
//! The stand-in keeps consent per principal, as the desktop does: agents are
//! allowed from the start, the MCP client is not until the test says so. So
//! the first MCP call proves an agent's approval does not carry over.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oximux_agents::lineage::LineageStore;
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::{StubConnection, ThreadEvent};
use oximux_remote_host::launcher::ChildCtx;
use oximux_remote_host::{
    AuthStore, Dispatcher, HostConfig, HostCore, HostCoreDeps, LaunchError, LocalScope, PriorOwner, SessionLauncher,
    SimPrincipal, SimulatorControl,
};
use oximux_remote_local::{LocalClaim, LocalControlListener, generate_token};
use oximux_remote_proto::lineage::AgentRosterWire;
use oximux_remote_proto::simulator::*;
use serde_json::{Value, json};

/// Consent per principal; every call's principal recorded.
#[derive(Default)]
struct Stub {
    mcp_allowed: AtomicBool,
    principals: Mutex<Vec<SimPrincipal>>,
}

#[async_trait::async_trait]
impl SimulatorControl for Stub {
    async fn run(&self, worktree: &str, principal: SimPrincipal, cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        self.principals.lock().unwrap().push(principal);
        let allowed = principal == SimPrincipal::Agent || self.mcp_allowed.load(Ordering::SeqCst);
        if cmd.is_control() && !allowed {
            return Err(SimErrorWire::ConsentPending);
        }
        let button = SimAxNodeWire {
            depth: 0,
            role: "Button".into(),
            label: Some("Sign in".into()),
            identifier: None,
            value: None,
            enabled: true,
            frame: [16.0, 590.0, 370.0, 44.0],
        };
        let cmd = match cmd {
            SimCmdWire::AsCaller { cmd, .. } => *cmd,
            other => other,
        };
        Ok(match cmd {
            // Only `…/app` is a worktree this host knows.
            SimCmdWire::Status if !Path::new(worktree).ends_with("app") => return Err(SimErrorWire::NoDevice),
            SimCmdWire::Status => SimReplyWire::Status(SimStatusWire {
                available: true,
                reason: None,
                xcode: Some("26.3".into()),
                worktree: worktree.into(),
                device: Some(SimDeviceWire { udid: "U-1".into(), name: "iPhone 17".into(), runtime: "iOS 26.3".into(), state: "Booted".into() }),
                streaming: true,
                consent: if allowed { SimConsentWire::Allowed } else { SimConsentWire::Pending },
                agent_control: true,
            }),
            SimCmdWire::AxRefs { .. } => SimReplyWire::AxRefs(vec![SimRefNodeWire { r: 1, node: button }]),
            SimCmdWire::Settled { .. } => SimReplyWire::Settled(SimSettleWire {
                outcome: Some(SimSettleOutcomeWire::Settled),
                elapsed_ms: 400,
                diff: vec![],
                unchanged: 3,
                warnings: vec![],
                target: Some(button),
                at: Some(SimPointWire { x: 201.0, y: 612.0 }),
            }),
            SimCmdWire::Wait { .. } => SimReplyWire::Matched { node: Some(button), elapsed_ms: 250 },
            SimCmdWire::Screenshot { .. } => {
                let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
                png.extend_from_slice(&402u32.to_be_bytes());
                png.extend_from_slice(&874u32.to_be_bytes());
                SimReplyWire::Screenshot { png, width: 402, height: 874, scale: 1.0 }
            }
            _ => SimReplyWire::Done,
        })
    }
}

/// A host at `runtime_dir` that takes MCP clients (as the desktop does).
fn serve(rt: &tokio::runtime::Runtime, runtime_dir: &Path, stub: Arc<Stub>) {
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new())).with_simulator(stub));
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(runtime_dir, &generate_token()).unwrap()
    };
    listener.grant_mcp(runtime_dir).unwrap();
    accept(rt, listener, dispatcher);
}

fn accept(rt: &tokio::runtime::Runtime, listener: LocalControlListener, dispatcher: Arc<Dispatcher>) {
    rt.spawn(async move {
        while let Ok(pending) = listener.accept_pending().await {
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let Ok((transport, claim)) = pending.authenticate().await else { return };
                let scope = match claim {
                    LocalClaim::Operator => LocalScope::Full,
                    LocalClaim::Session(id) => LocalScope::Session(id.into()),
                    LocalClaim::Mcp => LocalScope::Mcp,
                };
                dispatcher.serve_local(transport.as_ref(), scope).await;
            });
        }
    });
}

fn cli(runtime_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", runtime_dir.to_str().unwrap(), "--timeout", "10"]);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

/// A running `oximux mcp`: requests in, each stdout line out.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Server {
    fn start(runtime_dir: &Path, worktree: &str) -> Self {
        Self::with_args(runtime_dir, &["mcp", "--worktree", worktree])
    }

    fn with_args(runtime_dir: &Path, args: &[&str]) -> Self {
        let mut child = cli(runtime_dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        Self { child, stdin, lines, next_id: 0 }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// A request's response (stdout must hold nothing but JSON-RPC).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let line = self.lines.recv_timeout(Duration::from_secs(30)).expect("an answer in time");
        let message: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout carried a non-JSON line ({e}): {line}"));
        assert_eq!((message["jsonrpc"].as_str(), message["id"].as_u64()), (Some("2.0"), Some(id)), "{message}");
        message
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({ "name": tool, "arguments": arguments }))["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value) -> String {
    result["content"].as_array().unwrap().iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

#[test]
fn an_mcp_client_drives_the_simulator_as_its_own_principal() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    let stub = Arc::new(Stub::default());
    serve(&rt, &runtime_dir, stub.clone());
    let worktree = dir.path().join("app");
    std::fs::create_dir_all(&worktree).unwrap();
    let wt = worktree.to_str().unwrap();
    let mut server = Server::start(&runtime_dir, wt);

    let init = server.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "oximux");
    server.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let tools = server.request("tools/list", json!({}));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);

    // A cancelled call is dropped: no answer comes for it, and the server
    // goes on (the next answer is the ping's).
    server.send(json!({ "jsonrpc": "2.0", "id": 900, "method": "tools/call", "params": { "name": "sim_wait_consent", "arguments": { "timeout_s": 20 } } }));
    std::thread::sleep(Duration::from_millis(300));
    server.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 900 } }));
    assert!(server.request("ping", json!({}))["result"].is_object(), "the ping's answer, not the cancelled call's");

    // An agent's approval does not carry over: the client is asked for itself.
    let pending = server.call("sim_act", json!({ "steps": [{ "tap": { "ref": "@e1" } }] }));
    assert_eq!(pending["isError"], true);
    assert!(text(&pending).contains("sim_wait_consent"), "{pending}");
    assert_eq!(stub.principals.lock().unwrap().last(), Some(&SimPrincipal::Mcp));

    stub.mcp_allowed.store(true, Ordering::SeqCst);
    let allowed = server.call("sim_wait_consent", json!({ "timeout_s": 2 }));
    assert_eq!(allowed["isError"], false, "{allowed}");

    // The same verb through the CLI and through MCP reads the same.
    let observed = server.call("sim_observe", json!({}));
    assert_eq!(observed["isError"], false, "{observed}");
    let cli_out = cli(&runtime_dir).args(["sim", "--worktree", wt, "ax", "-i"]).output().unwrap();
    assert_eq!(text(&observed).trim_end(), String::from_utf8_lossy(&cli_out.stdout).trim_end(), "MCP text is the CLI's");
    assert!(text(&observed).contains("@e1"));

    // Two steps, each reported; then a wait that cannot fit is refused.
    let acted = server.call("sim_act", json!({ "steps": [{ "tap": { "ref": 1 } }, { "wait": { "target": { "label": "Sign in" }, "timeout_ms": 3000 } }] }));
    assert_eq!(acted["isError"], false, "{acted}");
    let lines = text(&acted);
    assert!(lines.starts_with("1. tap: ") && lines.contains("\n2. wait: "), "{lines}");
    let too_long = server.call("sim_act", json!({ "steps": [{ "wait": { "target": { "label": "Home" }, "timeout_ms": 60000 } }] }));
    assert!(too_long["isError"] == true && text(&too_long).contains("split the wait"), "{too_long}");

    // A screenshot comes back as an image, and the file it was saved to.
    let shot = server.call("sim_screenshot", json!({}));
    assert_eq!(shot["content"][0]["type"], "image", "{shot}");
    assert_eq!(shot["content"][0]["mimeType"], "image/png");
    assert!(text(&shot).contains("oximux-sim"), "{shot}");

    // Unknown methods are JSON-RPC errors; the server stays up.
    assert_eq!(server.request("nope/nothing", json!({}))["error"]["code"], -32601);
    assert!(server.request("ping", json!({}))["result"].is_object());
    assert!(stub.principals.lock().unwrap().contains(&SimPrincipal::Agent), "the CLI's own calls stay agents'");
}

/// Started without `--worktree` in a directory the host does not know as a
/// worktree: refused, with why (a host that is not running is no reason).
#[test]
fn an_mcp_server_refuses_an_unknown_worktree() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("host");
    serve(&rt, &runtime_dir, Arc::new(Stub::default()));
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = cli(&runtime_dir).arg("mcp").current_dir(&elsewhere).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in a worktree OxiMux knows"));
    assert!(out.stdout.is_empty());
}

/// Started in the home directory without `--worktree`: refused, with why.
/// (Unix only: the home directory is `$HOME` there; Windows asks the shell.)
#[cfg(unix)]
#[test]
fn an_mcp_server_never_starts_in_the_home_directory() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oximux-cli"))
        .args(["--dir", home.path().join("none").to_str().unwrap(), "mcp"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--worktree"));
    assert!(out.stdout.is_empty(), "nothing on stdout but JSON-RPC");
}

// ── the delegation tools ────────────────────────────────────────────────────

/// Starts each child as a stub session that reports working.
struct ChildLauncher {
    registry: Arc<SessionRegistry>,
    cwd: std::path::PathBuf,
    launched: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl SessionLauncher for ChildLauncher {
    async fn create(&self, cwd: &str, agent: Option<&str>, model: Option<&str>) -> Result<String, LaunchError> {
        self.create_child(cwd, agent, model, ChildCtx::default()).await
    }

    async fn create_child(&self, _: &str, _: Option<&str>, _: Option<&str>, _: ChildCtx) -> Result<String, LaunchError> {
        let id = format!("child-{}", self.launched.fetch_add(1, Ordering::SeqCst));
        let handle = self.registry.register(id.clone(), Arc::new(StubConnection::default()));
        handle.set_meta(SessionMeta { cwd: Some(self.cwd.clone()), ..Default::default() });
        handle.set_working(true);
        Ok(id)
    }

    fn roster(&self) -> Vec<AgentRosterWire> {
        vec![AgentRosterWire {
            agent_id: "claude-code".into(),
            display: "Claude Code".into(),
            default_model: None,
            models: vec![],
            postures: vec![],
            installed: true,
        }]
    }
}

/// A desktop-like host with delegation on and one chat, `parent`, whose
/// agent holds the session credential written to the returned file.
fn serve_delegation(rt: &tokio::runtime::Runtime, dir: &Path) -> (Arc<SessionRegistry>, std::path::PathBuf) {
    let runtime_dir = dir.join("host");
    let registry = Arc::new(SessionRegistry::new());
    let parent = registry.register("parent".into(), Arc::new(StubConnection::default()));
    parent.set_meta(SessionMeta { cwd: Some(dir.to_path_buf()), ..Default::default() });
    let db = oximux_storage::db::open_memory().unwrap();
    let core = HostCore::new(HostCoreDeps {
        rt: rt.handle().clone(),
        lineage: LineageStore::new(db.conn()),
        registry: registry.clone(),
        config: HostConfig::default(),
    });
    core.start(PriorOwner::None);
    let launcher = Arc::new(ChildLauncher { registry: registry.clone(), cwd: dir.to_path_buf(), launched: Default::default() });
    let dispatcher = Arc::new(
        Dispatcher::new(registry.clone(), Arc::new(AuthStore::new())).with_launcher(launcher).with_host_core(core),
    );
    let listener = {
        let _guard = rt.enter();
        LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap()
    };
    let label = "agent-parent-credential";
    let secret = listener.grant_session(label);
    listener.bind_session(label, "parent");
    let credential = dir.join("parent.cred");
    oximux_remote_local::write_credential_file(&credential, label, &secret).unwrap();
    accept(rt, listener, dispatcher);
    (registry, credential)
}

#[test]
fn an_agent_delegates_over_mcp_with_its_own_credential() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (registry, credential) = serve_delegation(&rt, dir.path());
    let runtime_dir = dir.path().join("host");
    // No --worktree: the delegation set needs none.
    let mut server = Server::with_args(
        &runtime_dir,
        &["mcp", "--tools", "orchestrate", "--credential-file", credential.to_str().unwrap(), "--default-agent", "claude-code"],
    );

    let init = server.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} }));
    assert!(init["result"]["instructions"].as_str().unwrap().contains("helper agents"), "{init}");
    let tools = server.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"oximux_spawn_agent"), "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("oximux_")), "no simulator tools: {names:?}");

    let spawned = server.call("oximux_spawn_agent", json!({ "prompt": "summarize the README", "request_id": "r1" }));
    assert_eq!(spawned["isError"], false, "{spawned}");
    assert!(text(&spawned).contains("child-0"), "{spawned}");
    let children = server.call("oximux_list_children", json!({}));
    assert!(text(&children).contains("child-0"), "{children}");

    // Still working: the wait gives up at its timeout and says so.
    let waited = server.call("oximux_wait_agents", json!({ "ids": ["child-0"], "timeout_ms": 1000 }));
    assert_eq!(waited["isError"], false, "{waited}");
    let report: Value = serde_json::from_str(&text(&waited)).unwrap();
    assert_eq!(report["timed_out"], true, "{report}");
    assert_eq!(report["children"][0]["terminal"], false, "{report}");

    // Its turn ends: the next wait answers at once.
    registry.ingest("child-0", ThreadEvent::UserMessage { text: "summarize the README".into(), images: vec![] });
    registry.ingest("child-0", ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
    let waited = server.call("oximux_wait_agents", json!({ "ids": ["child-0"], "timeout_ms": 20000 }));
    let report: Value = serde_json::from_str(&text(&waited)).unwrap();
    assert_eq!(report["children"][0]["state"], "finished", "{report}");
    assert_eq!(report["timed_out"], false, "{report}");

    // Only its own subtree: the parent itself is not a child to wait on.
    let stranger = server.call("oximux_wait_agents", json!({ "ids": ["parent"] }));
    assert!(stranger["isError"] == true && text(&stranger).contains("not an agent you started"), "{stranger}");
}

/// The delegation set is for an agent OxiMux started. A process holding no
/// agent credential (an outside client, or the operator) is refused at start.
#[test]
fn the_delegation_set_needs_an_agent_credential() {
    let dir = tempfile::tempdir().unwrap();
    let out = cli(&dir.path().join("host")).args(["mcp", "--tools", "orchestrate"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("agent session credential"));
    assert!(out.stdout.is_empty());
}
