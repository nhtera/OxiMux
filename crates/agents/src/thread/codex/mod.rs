//! `CodexAppServerConnection` — drives a `codex app-server` subprocess over its
//! native JSON-RPC (v2) and surfaces decoded [`ThreadEvent`]s on a channel, so
//! Codex lights up the same chat UI as Claude with no view changes.
//!
//! Shape (mirrors the Claude connection's ownership, but with a handshake):
//! - a **reader thread** (in [`transport`]) owns stdout, routes responses to the
//!   pending-request map, and forwards notifications / server-requests as [`Inbound`];
//! - a **mapper thread** turns `Inbound` into `ThreadEvent`s (and answers the
//!   Phase-1 approval stub);
//! - a **worker thread** runs the async handshake (`initialize` → `initialized` →
//!   `thread/start`) so `spawn` never blocks the UI, then forwards prompts as
//!   `turn/start`. Interrupts go direct (bypassing the worker queue).
//!
//! Phase 1 maps only the text-round-trip slice (`item/agentMessage/delta`,
//! `turn/started`, `turn/completed`, `error`); Phase 2 completes the mapping and
//! Phase 3 makes approvals/usage/pickers real. Fixed posture: `on-request`
//! approvals + `workspace-write` sandbox (`supports_modes = false`).

mod approvals;
mod image_items;
mod map;
pub mod protocol;
pub mod transport;

use std::collections::HashMap;
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use approvals::ServerRequestAction;
use super::connection::{
    AgentCapabilities, AgentConnection, EffortChoice, FeatureControl, FeatureKind,
    FeatureSelectOption, FeatureValue, ModelChoice,
};
use super::event::{AuthMethodInfo, AuthMethodKind, ThreadEvent, TurnUsage};
use super::question::{AskQuestion, QuestionAnswers};
use super::tool_call::PermissionDecision;
use transport::{Inbound, RpcClient};

/// Handshake round-trips (`initialize`, `thread/start`) block the worker; a
/// generous ceiling so a cold `codex` start (auth/network) still connects.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stable feature-control ids echoed back on a posture change (`set_feature`).
const FEATURE_APPROVALS: &str = "codex_approval_policy";
const FEATURE_SANDBOX: &str = "codex_sandbox";

/// Build one `FeatureSelectOption` for a posture select.
fn select_opt(wire: &str, label: &str, description: &str) -> FeatureSelectOption {
    FeatureSelectOption {
        wire: wire.to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
    }
}

/// Commands the sync `AgentConnection` methods push to the worker thread.
enum Outbound {
    Prompt(String),
    /// Kick off a ChatGPT browser sign-in (`account/login/start`) on the worker
    /// so the calling UI thread never blocks on the RPC; the worker emits the
    /// resulting `AuthUrl` (or an `Error`) for the app to open.
    StartLogin,
    Shutdown,
}

/// Session state shared reader(mapper)↔worker↔interrupt: the thread + in-flight
/// turn ids, and the latest token usage (attached to the next `TurnEnded`).
#[derive(Default)]
struct CodexState {
    thread_id: Option<String>,
    current_turn_id: Option<String>,
    last_usage: Option<TurnUsage>,
    /// The session's working directory, seeded at spawn. Used to contain the
    /// file paths `imageView`/`imageGeneration` items carry — an image is only
    /// inlined when its canonicalized path stays within this directory.
    cwd: std::path::PathBuf,
    /// Tool items (commandExecution / fileChange) by itemId, so a later approval
    /// request — which carries only the itemId — can show the command / changes.
    cmd_items: HashMap<String, Value>,
    /// Pending approvals: our permission-card `request_id` → the Codex JSON-RPC
    /// request id we must answer with a `{decision}` once the user chooses.
    pending_approvals: HashMap<String, Value>,
    /// Pending MCP elicitations: our card `request_id` → the JSON-RPC request id.
    /// Kept separate from `pending_approvals` because an elicitation is answered
    /// with a different reply shape (`{action}`, not `{decision}`).
    pending_elicitations: HashMap<String, Value>,
    /// Codex's model catalog (from `model/list`), cached after the handshake so
    /// the pickers render it.
    models: Vec<protocol::CodexModel>,
    /// The model/effort applied to each `turn/start` (a switch respawns with
    /// these seeded). `None` = Codex's own default.
    current_model: Option<String>,
    current_effort: Option<String>,
    /// The approval policy + sandbox mode this session runs under (the composer's
    /// Approvals/Sandbox selects). Seeded to the default posture at spawn (or a
    /// restored posture), updated in place by `set_feature`, and sent on every
    /// `turn/start` as a per-turn override so a change takes effect on the next
    /// send with no respawn.
    approval_policy: String,
    sandbox: String,
    /// Set when Stop was pressed before the turn id was known (the `turn/started`
    /// notification hadn't landed yet). The mapper interrupts as soon as the id
    /// arrives, so a fast Send→Stop doesn't silently no-op.
    cancel_requested: bool,
    /// Ordered ledger of turn ids, one per turn started this session (each user
    /// prompt drives exactly one turn). Indexed by user-message ordinal so a
    /// conversation-rewind can address `thread/fork`'s `lastTurnId` — to rewind
    /// *before* user message N, fork through `user_turn_ids[N-1]`. Empty on a
    /// freshly-restored session (turns aren't replayed), which makes rewind a
    /// no-op there until the first new turn.
    user_turn_ids: Vec<String>,
    /// Sub-agent routing: a spawned child thread id → the root-thread collab tool
    /// call item id that spawned it (from `collabAgentToolCall.receiverThreadIds`
    /// or `subAgentActivity.agentThreadId`). A foreign notification whose thread is
    /// registered here routes into that parent tool card's log instead of dropping.
    subagent_parent_by_thread: HashMap<String, String>,
    /// Foreign child `item/completed` payloads that arrived before their thread was
    /// registered (the collab spawn item raced), buffered per child thread and
    /// replayed into the log on registration. Bounded per thread so a runaway child
    /// can't grow it without limit.
    subagent_buffer: HashMap<String, std::collections::VecDeque<Value>>,
}

pub struct CodexAppServerConnection {
    outbound: Sender<Outbound>,
    rpc: RpcClient,
    state: Arc<Mutex<CodexState>>,
    child: Arc<Mutex<Child>>,
    _worker: JoinHandle<()>,
    _mapper: JoinHandle<()>,
}

impl CodexAppServerConnection {
    /// Spawn `codex app-server` in `cwd` and start streaming decoded events.
    /// Returns immediately; the handshake runs on the worker thread and emits
    /// [`ThreadEvent::SessionInit`] once `thread/start` resolves.
    pub fn spawn(
        cwd: &Path,
        model: Option<&str>,
        resume_session_id: Option<&str>,
        effort: Option<&str>,
        posture: Option<(&str, &str)>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (rpc, inbound_rx, child) = RpcClient::spawn(cwd)?;
        let (event_tx, event_rx) = mpsc::channel::<ThreadEvent>();
        let (out_tx, out_rx) = mpsc::channel::<Outbound>();
        let state = Arc::new(Mutex::new(CodexState::default()));
        // Seed the current model/effort + posture so the first turn — and the
        // picker labels — reflect the tab's choice. A restored posture overrides
        // the default; a fresh session starts at on-request/workspace-write.
        if let Ok(mut s) = state.lock() {
            s.current_model = model.map(str::to_string);
            s.current_effort = effort.map(str::to_string);
            s.cwd = cwd.to_path_buf();
            s.approval_policy = posture
                .and_then(|(a, _)| (!a.is_empty()).then(|| a.to_string()))
                .unwrap_or_else(|| protocol::DEFAULT_APPROVAL_POLICY.to_string());
            s.sandbox = posture
                .and_then(|(_, sb)| (!sb.is_empty()).then(|| sb.to_string()))
                .unwrap_or_else(|| protocol::DEFAULT_SANDBOX.to_string());
        }

        let mapper = {
            let event_tx = event_tx.clone();
            let state = state.clone();
            let rpc = rpc.clone();
            thread::spawn(move || map_inbound(inbound_rx, event_tx, state, rpc))
        };
        let worker = {
            let rpc = rpc.clone();
            let state = state.clone();
            let cwd = cwd.to_path_buf();
            let model = model.map(str::to_string);
            let resume = resume_session_id.map(str::to_string);
            thread::spawn(move || worker_loop(rpc, event_tx, state, out_rx, cwd, model, resume))
        };

        Ok((
            Self {
                outbound: out_tx,
                rpc,
                state,
                child: Arc::new(Mutex::new(child)),
                _worker: worker,
                _mapper: mapper,
            },
            event_rx,
        ))
    }

    /// Kill + reap the codex process group — the direct child AND its sandbox
    /// helper processes (spawned in the same group, see `spawn_command`).
    fn reap(&self) {
        let mut child = match self.child.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        #[cfg(unix)]
        {
            // SAFETY: `pgid` is our own child's process group (it leads the group
            // via `process_group(0)`); `kill(2)` with a negative pid signals the
            // whole group. Best-effort — a gone group returns ESRCH, ignored.
            let pgid = child.id() as libc::pid_t;
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl AgentConnection for CodexAppServerConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.outbound
            .send(Outbound::Prompt(text.to_string()))
            .map_err(|_| anyhow!("codex worker is gone"))
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        // Look up the stashed Codex JSON-RPC request id and answer it directly
        // (the reader never blocked on the decision). A no-op if already answered.
        // An MCP elicitation is answered with a different reply shape (`{action}`),
        // so check that map first.
        let (id, elicitation) = {
            let mut st = self.state.lock().map_err(|_| anyhow!("codex state poisoned"))?;
            match st.pending_elicitations.remove(request_id) {
                Some(id) => (Some(id), true),
                None => (st.pending_approvals.remove(request_id), false),
            }
        };
        match id {
            Some(id) if elicitation => self.rpc.respond(id, approvals::to_codex_elicitation(&decision)),
            Some(id) => self
                .rpc
                .respond(id, json!({ "decision": approvals::to_codex_decision(&decision) })),
            None => Ok(()),
        }
    }

    fn answer_question(
        &self,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<()> {
        // Look up the stashed JSON-RPC id for this question (shares the pending
        // map with permissions — ids are unique) and reply with the Codex
        // `{answers: {<qid>: {answers: [..]}}}` shape. A no-op if already answered.
        let id = self
            .state
            .lock()
            .map_err(|_| anyhow!("codex state poisoned"))?
            .pending_approvals
            .remove(request_id);
        match id {
            Some(id) => self.rpc.respond(id, approvals::codex_answers_json(questions, answers)),
            None => Ok(()),
        }
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            supports_modes: false, // fixed on-request posture in P1 (no mode picker)
            supports_slash: false, // no slash-command palette wired for Codex yet
            supports_config: true, // reasoning-effort picker (from model/list)
            emits_usage: true,     // thread/tokenUsage/updated → the usage footer
            // Conversation rewind via server-side `thread/fork` (no ~/.claude
            // JSONL needed). The rewind UI gates light up; the flow branches on
            // `rewind_is_server_side` to fork the live thread instead of a file.
            supports_rewind: true,
            supports_steer: false, // no mid-turn queue in app-server
        }
    }

    fn rewind_is_server_side(&self) -> bool {
        true
    }

    /// Rewind by forking the thread before user message `user_ordinal`: map the
    /// ordinal to the ledger's turn ids (fork through the turn BEFORE it, so that
    /// message and everything after is dropped), send `thread/fork`, swap the
    /// session's thread id to the fork, truncate the ledger, and return the new
    /// thread id. The original thread is untouched (recoverable). Errors if the
    /// ledger can't address the ordinal (e.g. a freshly-restored session with no
    /// new turns yet).
    fn fork_conversation(&self, user_ordinal: usize, total_user_messages: usize) -> Result<String> {
        let (thread_id, last_turn_id) = {
            let s = self.state.lock().map_err(|_| anyhow!("codex state poisoned"))?;
            let thread_id = s.thread_id.clone().ok_or_else(|| anyhow!("no codex thread to rewind"))?;
            let last = fork_last_turn_id(&s.user_turn_ids, user_ordinal, total_user_messages)?;
            (thread_id, last)
        };
        let res = self.rpc.request(
            protocol::M_THREAD_FORK,
            protocol::thread_fork_params(&thread_id, last_turn_id.as_deref()),
            HANDSHAKE_TIMEOUT,
        )?;
        let new_id = protocol::thread_id_from_start_response(&res)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("thread/fork returned no thread id"))?;
        if let Ok(mut s) = self.state.lock() {
            s.thread_id = Some(new_id.clone());
            s.user_turn_ids.truncate(user_ordinal);
            s.current_turn_id = None;
        }
        Ok(new_id)
    }

    /// Kick off the ChatGPT browser OAuth. Non-blocking: hands the work to the
    /// worker (which sends `account/login/start` and emits the resulting
    /// [`ThreadEvent::AuthUrl`]) so the UI click that calls this never blocks on
    /// the RPC. `account/login/completed` later resolves the flow via
    /// [`ThreadEvent::AuthOutcome`].
    fn begin_browser_login(&self) -> Result<()> {
        self.outbound
            .send(Outbound::StartLogin)
            .map_err(|_| anyhow!("codex worker gone; cannot start sign-in"))
    }

    fn models(&self) -> Vec<ModelChoice> {
        self.state
            .lock()
            .map(|s| {
                s.models
                    .iter()
                    .map(|m| ModelChoice {
                        wire: m.wire.clone(),
                        label: m.display.clone(),
                        description: m.description.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn efforts(&self) -> Vec<EffortChoice> {
        self.state
            .lock()
            .map(|s| {
                current_or_default_model(&s)
                    .map(|m| {
                        m.efforts
                            .iter()
                            .map(|e| EffortChoice { wire: e.clone(), label: title_case(e) })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    fn default_model(&self) -> Option<String> {
        let s = self.state.lock().ok()?;
        s.current_model
            .clone()
            .or_else(|| current_or_default_model(&s).map(|m| m.wire.clone()))
    }

    fn default_effort(&self) -> Option<String> {
        let s = self.state.lock().ok()?;
        s.current_effort
            .clone()
            .or_else(|| current_or_default_model(&s).map(|m| m.default_effort.clone()))
    }

    /// The composer's two posture selects — Approvals and Sandbox — seeded with
    /// the session's current posture. The `danger-full-access` / `never` options
    /// carry a ⚠ blurb; defaults stay on-request/workspace-write.
    fn features(&self) -> Vec<FeatureControl> {
        let (approval, sandbox) = self
            .state
            .lock()
            .map(|s| (s.approval_policy.clone(), s.sandbox.clone()))
            .unwrap_or_default();
        vec![
            FeatureControl {
                id: FEATURE_APPROVALS.to_string(),
                label: "Approvals".to_string(),
                description: Some("When Codex asks before running commands or edits".to_string()),
                icon: Some("shield".to_string()),
                kind: FeatureKind::Select {
                    options: vec![
                        select_opt(protocol::APPROVAL_ON_REQUEST, "On request", "Ask before commands & edits"),
                        select_opt(protocol::APPROVAL_NEVER, "Never ask", "⚠ Run without approval prompts"),
                    ],
                    selected: Some(approval),
                },
            },
            FeatureControl {
                id: FEATURE_SANDBOX.to_string(),
                label: "Sandbox".to_string(),
                description: Some("What Codex's commands may touch".to_string()),
                icon: Some("box".to_string()),
                kind: FeatureKind::Select {
                    options: vec![
                        select_opt(protocol::SANDBOX_READ_ONLY, "Read only", "No writes"),
                        select_opt(protocol::SANDBOX_WORKSPACE_WRITE, "Workspace write", "Write within the project"),
                        select_opt(
                            protocol::SANDBOX_DANGER_FULL_ACCESS,
                            "Full access",
                            "⚠ Unsandboxed — full disk & network",
                        ),
                    ],
                    selected: Some(sandbox),
                },
            },
        ]
    }

    /// Apply a posture change: stash the chosen wire value on the session so the
    /// next `turn/start` carries it as a per-turn override (Codex applies it for
    /// that turn and onward). Returns `Ok` so the app skips a respawn — the switch
    /// takes effect on the next send. An unknown id or non-select value is ignored.
    fn set_feature(&self, id: &str, value: FeatureValue) -> Result<()> {
        let FeatureValue::Choice(wire) = value else {
            return Ok(());
        };
        let mut s = self.state.lock().map_err(|_| anyhow!("codex state poisoned"))?;
        match id {
            FEATURE_APPROVALS => s.approval_policy = wire,
            FEATURE_SANDBOX => s.sandbox = wire,
            _ => {}
        }
        Ok(())
    }

    /// Interrupt the in-flight turn (`turn/interrupt`). Fire-and-forget so the
    /// Stop button never blocks; no-op when nothing is in flight.
    fn cancel(&self) -> Result<()> {
        let (tid, turn) = match self.state.lock() {
            Ok(mut s) => {
                // Turn id not known yet (turn/started still in flight) → remember
                // the request so the mapper interrupts the moment it arrives.
                if s.current_turn_id.is_none() {
                    s.cancel_requested = true;
                }
                (s.thread_id.clone(), s.current_turn_id.clone())
            }
            Err(_) => return Ok(()),
        };
        match (tid, turn) {
            (Some(tid), Some(turn)) => self
                .rpc
                .fire(protocol::M_TURN_INTERRUPT, protocol::turn_interrupt_params(&tid, &turn)),
            _ => Ok(()),
        }
    }

    fn shutdown(&self) {
        let _ = self.outbound.send(Outbound::Shutdown);
        self.reap();
    }
}

impl Drop for CodexAppServerConnection {
    fn drop(&mut self) {
        // Guard against orphaned `codex` + sandbox helpers if the connection is
        // dropped without an explicit shutdown().
        let _ = self.outbound.send(Outbound::Shutdown);
        self.reap();
    }
}

/// Resolve the `thread/fork` `lastTurnId` for rewinding before the user message
/// at `user_ordinal` (0-based), given the session's turn `ledger` and the
/// transcript's full user-message count. Pure so the ordinal→turn-id math is
/// testable without a live connection.
///
/// - Fails closed when the ledger doesn't cover every user message (a restored
///   session whose earlier turns weren't replayed — ordinals can't be mapped, and
///   forking on a partial ledger could drop history).
/// - Fails when the ordinal is past the ledger (out of sync).
/// - `Ok(None)` = fork through no turns (rewind before the first message).
/// - `Ok(Some(id))` = fork through the turn BEFORE the target message.
fn fork_last_turn_id(
    ledger: &[String],
    user_ordinal: usize,
    total_user_messages: usize,
) -> Result<Option<String>> {
    if ledger.len() != total_user_messages {
        anyhow::bail!(
            "rewind is unavailable for this session (its earlier turns aren't tracked — \
             restored sessions can't be rewound until the conversation is replayed)"
        );
    }
    if user_ordinal > ledger.len() {
        anyhow::bail!("rewind target is out of sync with the conversation");
    }
    Ok(user_ordinal.checked_sub(1).and_then(|i| ledger.get(i)).cloned())
}

/// The current model (if the picker chose one), else the catalog default, else
/// the first entry.
fn current_or_default_model(st: &CodexState) -> Option<&protocol::CodexModel> {
    let cur = st.current_model.as_deref();
    st.models
        .iter()
        .find(|m| Some(m.wire.as_str()) == cur)
        .or_else(|| st.models.iter().find(|m| m.is_default))
        .or_else(|| st.models.first())
}

/// Capitalize the first letter for an effort label ("low" → "Low").
fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The single sign-in method a logged-out Codex advertises: a browser OAuth pill
/// ("Sign in with ChatGPT") that opens the merged ChatGPT auth flow.
fn chatgpt_signin_method() -> AuthMethodInfo {
    AuthMethodInfo {
        id: "chatgpt".to_string(),
        name: "Sign in with ChatGPT".to_string(),
        description: Some("Opens ChatGPT in your browser to authorize Codex".to_string()),
        kind: AuthMethodKind::BrowserOauth,
    }
}

/// The worker: async handshake (`initialize` → cache `model/list` → `initialized`
/// → `thread/resume` or `thread/start`), then forward prompts as `turn/start`.
fn worker_loop(
    rpc: RpcClient,
    event_tx: Sender<ThreadEvent>,
    state: Arc<Mutex<CodexState>>,
    out_rx: Receiver<Outbound>,
    cwd: std::path::PathBuf,
    model: Option<String>,
    resume: Option<String>,
) {
    if let Err(e) = rpc.request(protocol::M_INITIALIZE, protocol::initialize_params(), HANDSHAKE_TIMEOUT) {
        let _ = event_tx.send(ThreadEvent::Error(format!("codex initialize failed: {e}")));
        return;
    }
    // Fetch the model catalog BEFORE emitting SessionInit, so the composer's
    // controls seed (which runs on that event) already has the picker vocab.
    if let Ok(res) = rpc.request(protocol::M_MODEL_LIST, json!({}), HANDSHAKE_TIMEOUT) {
        let models = protocol::parse_model_list(&res);
        if let Ok(mut s) = state.lock() {
            s.models = models;
        }
    }
    let _ = rpc.notify(protocol::N_INITIALIZED, Value::Null);

    // Snapshot the seeded posture (owned, so it outlives the state lock) and
    // send it on thread/start & thread/resume.
    let (approval, sandbox) = match state.lock() {
        Ok(s) => (s.approval_policy.clone(), s.sandbox.clone()),
        Err(_) => (protocol::DEFAULT_APPROVAL_POLICY.into(), protocol::DEFAULT_SANDBOX.into()),
    };
    let posture = protocol::Posture { approval_policy: &approval, sandbox: &sandbox };

    // Resume the persisted thread when restoring; else start fresh. A failed
    // resume (thread gone / experimental) degrades to a fresh start.
    let resume_id = resume.as_deref().filter(|s| !s.is_empty());
    let started = match resume_id {
        Some(tid) => rpc.request(
            protocol::M_THREAD_RESUME,
            protocol::thread_resume_params(tid, model.as_deref(), posture),
            HANDSHAKE_TIMEOUT,
        ),
        None => rpc.request(
            protocol::M_THREAD_START,
            protocol::thread_start_params(model.as_deref(), &cwd, posture),
            HANDSHAKE_TIMEOUT,
        ),
    };
    let res = match started {
        Ok(r) => Some(r),
        Err(e) if resume_id.is_some() => {
            tracing::warn!(error = %e, "codex thread/resume failed; starting a fresh thread");
            rpc.request(
                protocol::M_THREAD_START,
                protocol::thread_start_params(model.as_deref(), &cwd, posture),
                HANDSHAKE_TIMEOUT,
            )
            .ok()
        }
        Err(e) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("codex thread/start failed: {e}")));
            return;
        }
    };
    let Some(res) = res else {
        let _ = event_tx.send(ThreadEvent::Error("codex thread/start failed after resume".into()));
        return;
    };
    // A handshake with no thread id is a hard failure — every later turn/cancel
    // would target an empty id. Surface it rather than pretending to connect.
    let Some(tid) = protocol::thread_id_from_start_response(&res).filter(|t| !t.is_empty()) else {
        let _ = event_tx.send(ThreadEvent::Error("codex handshake returned no thread id".into()));
        return;
    };
    let resolved_model = protocol::model_from_start_response(&res).or(model.clone()).unwrap_or_default();
    if let Ok(mut s) = state.lock() {
        s.thread_id = Some(tid.clone());
    }
    let _ = event_tx.send(ThreadEvent::SessionInit {
        session_id: tid,
        model: resolved_model,
        permission_mode: String::new(),
        slash_commands: Vec::new(),
            // Codex/ACP init advertises no tool/MCP/agent inventory.
            meta: Default::default(),
    });

    // Proactive sign-in detection: `thread/start` succeeds even when logged out,
    // so `account/read` is what tells us the session can't actually run a turn
    // (`{account: null, requiresOpenaiAuth: true}`). Emit the sign-in card AFTER
    // SessionInit (which the app treats as "auth done" for ACP and would clear a
    // card shown earlier). The turn-time 401 is the defensive fallback (map.rs).
    if let Ok(acc) = rpc.request(protocol::M_ACCOUNT_READ, json!({}), HANDSHAKE_TIMEOUT)
        && protocol::account_read_needs_login(&acc)
    {
        let _ = event_tx.send(ThreadEvent::AuthRequired {
            methods: vec![chatgpt_signin_method()],
            error: None,
        });
    }

    // Poll for commands, but wake periodically to notice a subprocess crash:
    // if the child died, break so `event_tx` drops and the app's disconnect
    // handler fires (a plain blocking `recv` would park here forever on a crash).
    loop {
        match out_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Outbound::Prompt(text)) => {
                let (tid, model, effort, approval, sandbox) = match state.lock() {
                    Ok(s) => (
                        s.thread_id.clone().unwrap_or_default(),
                        s.current_model.clone(),
                        s.current_effort.clone(),
                        s.approval_policy.clone(),
                        s.sandbox.clone(),
                    ),
                    Err(_) => (
                        String::new(),
                        None,
                        None,
                        protocol::DEFAULT_APPROVAL_POLICY.into(),
                        protocol::DEFAULT_SANDBOX.into(),
                    ),
                };
                // Fire-and-forget: the turn's text + turnId arrive as notifications;
                // we don't block the worker on the turn's lifetime. The current
                // posture rides as a per-turn override, so a mid-session Approvals/
                // Sandbox switch takes effect on this send with no respawn.
                let posture = protocol::Posture { approval_policy: &approval, sandbox: &sandbox };
                let params =
                    protocol::turn_start_params(&tid, &text, model.as_deref(), effort.as_deref(), posture);
                if let Err(e) = rpc.fire(protocol::M_TURN_START, params) {
                    let _ = event_tx.send(ThreadEvent::Error(format!("codex turn/start failed: {e}")));
                }
            }
            // A sign-in click: send `account/login/start` here (off the UI thread)
            // and emit the browser URL for the app to open. A failure surfaces as
            // a plain error rather than wedging the card.
            Ok(Outbound::StartLogin) => {
                match rpc.request(
                    protocol::M_ACCOUNT_LOGIN_START,
                    protocol::account_login_start_chatgpt_params(),
                    HANDSHAKE_TIMEOUT,
                ) {
                    Ok(res) => {
                        if let Some(url) = protocol::login_url_from_start_response(&res) {
                            let _ = event_tx.send(ThreadEvent::AuthUrl { url });
                        }
                    }
                    Err(e) => {
                        let _ = event_tx.send(ThreadEvent::Error(format!("codex sign-in failed: {e}")));
                    }
                }
            }
            Ok(Outbound::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !rpc.is_alive() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// The mapper: `Inbound` → `ThreadEvent` via [`map::map_notification`], plus the
/// Phase-1 approval auto-decline (Phase 3 makes approvals interactive).
fn map_inbound(
    rx: Receiver<Inbound>,
    event_tx: Sender<ThreadEvent>,
    state: Arc<Mutex<CodexState>>,
    rpc: RpcClient,
) {
    for inbound in rx {
        match inbound {
            Inbound::Notification { method, params } => {
                let (events, deferred_interrupt) = {
                    let mut st = match state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let events = map::map_notification(&method, &params, &mut st);
                    // Honor a Stop pressed before the turn id was known, now that
                    // this notification (turn/started) may have set it.
                    let deferred = if st.cancel_requested {
                        match (st.thread_id.clone(), st.current_turn_id.clone()) {
                            (Some(tid), Some(turn)) => {
                                st.cancel_requested = false;
                                Some((tid, turn))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    (events, deferred)
                };
                if let Some((tid, turn)) = deferred_interrupt {
                    let _ = rpc.fire(protocol::M_TURN_INTERRUPT, protocol::turn_interrupt_params(&tid, &turn));
                }
                for ev in events {
                    if event_tx.send(ev).is_err() {
                        return; // consumer gone
                    }
                }
            }
            Inbound::ServerRequest { id, method, params } => {
                let action = {
                    let mut st = match state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    approvals::map_server_request(&id, &method, &params, &mut st)
                };
                match action {
                    // A permission card was emitted; the reply follows the user's
                    // decision (via resolve_permission) — don't answer here.
                    ServerRequestAction::Emit(events) => {
                        for ev in events {
                            if event_tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                    ServerRequestAction::AutoRespond { result, events } => {
                        let _ = rpc.respond(id, result);
                        for ev in events {
                            if event_tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fork_last_turn_id;

    #[test]
    fn fork_last_turn_id_maps_ordinal_to_prior_turn() {
        let ledger = vec!["t0".to_string(), "t1".to_string(), "t2".to_string()];
        // Rewind before the first message → fork through nothing.
        assert_eq!(fork_last_turn_id(&ledger, 0, 3).unwrap(), None);
        // Rewind before message 2 → fork through the turn that produced message 1.
        assert_eq!(fork_last_turn_id(&ledger, 2, 3).unwrap().as_deref(), Some("t1"));
        // Rewind before the last (3rd) message → fork through message 2's turn.
        assert_eq!(fork_last_turn_id(&ledger, 2, 3).unwrap().as_deref(), Some("t1"));
    }

    #[test]
    fn fork_last_turn_id_fails_closed_on_incomplete_ledger() {
        // Restored session: ledger shorter than the transcript's user-message count.
        let ledger = vec!["t0".to_string()];
        assert!(fork_last_turn_id(&ledger, 1, 4).is_err(), "partial ledger must fail closed");
    }

    #[test]
    fn fork_last_turn_id_rejects_out_of_range_ordinal() {
        let ledger = vec!["t0".to_string(), "t1".to_string()];
        assert!(fork_last_turn_id(&ledger, 3, 2).is_err(), "ordinal past the ledger is out of sync");
    }
}
