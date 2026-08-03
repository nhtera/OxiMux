//! Screen-control enforcement, hung off the permission round-trip.
//!
//! The driver runs as its own process that the agent talks to directly, so
//! OxiMux is not in the tool-dispatch path and cannot gate a call by wrapping
//! it. The one place we are in the path is `can_use_tool`: the agent asks
//! before running a tool, and that ask arrives as
//! [`ThreadEvent::PermissionRequested`]. Everything here hangs off that single
//! interception point — a check placed anywhere else would simply never run.
//!
//! [`ThreadEvent::PermissionRequested`]: oximux_agents::thread::ThreadEvent::PermissionRequested
//!
//! ## What this does not cover
//!
//! Only tools from the server *OxiMux* declares. A user who wires the same
//! driver into their own agent config under a different server name gets tool
//! names this never matches, and no policy at all. That is a real gap and it
//! belongs to the broader safety work, not here — the point of noting it is
//! that the enforcement is scoped to what OxiMux itself hands the agent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;

use gpui::App;
use oximux_agents::thread::{AgentConnection, ConnectSpec, ThreadEvent, Transport};
use oximux_computer_use::grants::{GrantTable, Provenance, Verdict};
use oximux_computer_use::mcp::Declaration;
use oximux_computer_use::policy::{PolicyContext, decide};
use oximux_computer_use::session::SessionId;
use oximux_settings::ComputerUseSettings;
use serde_json::Value;

pub use oximux_computer_use::policy::Decision;

/// Where grants live for the whole app.
///
/// A file rather than an in-memory map because enforcement runs in two
/// processes — this one, and the short-lived hook the agent's CLI spawns per
/// tool call. Both open the same path; the store's `flock` makes the
/// check-then-insert atomic between them.
///
/// Shared by handle rather than reached as a static, so a test can scope one to
/// the chats it creates. Two tests that both claim the same pid in one store
/// are not testing anything about chats — they are racing each other.
static GRANTS: LazyLock<Arc<GrantTable>> = LazyLock::new(|| Arc::new(GrantTable::at(grants_path())));

/// The store's path, which is also what gets handed to the hook process so both
/// sides cannot drift onto different files.
///
/// Infallible on purpose. It used to be `Option`, and every caller then had to
/// decide what a missing data dir meant — including the spawn path, where the
/// honest-looking answer (skip) would have left the chat with no gate while this
/// process happily went on using a fallback store of its own.
pub fn grants_path() -> PathBuf {
    crate::app_paths::data_dir()
        // No data dir is a broken install; keep screen control working against
        // a temp store rather than failing the whole app to death over it.
        .unwrap_or_else(std::env::temp_dir)
        .join(oximux_computer_use::grants::GRANTS_FILE_NAME)
}

/// Where the user's driver approval is recorded, on the one platform that has
/// to ask for one.
///
/// Lives here rather than in the settings pane that writes it, for the same
/// reason [`grants_path`] does: the pane records the approval and this module
/// reads it at spawn, and if those two ever resolved different files the pane
/// would report a driver as approved while every chat kept refusing it — with
/// nothing in either place looking wrong.
///
/// `app_paths::data_dir` rather than the `dirs::data_dir` above, which is the
/// *roaming* profile on Windows. An approval is a statement about one binary on
/// one machine and must not follow the user to another. The grant store's use of
/// the roaming dir is a pre-existing inconsistency, not a precedent.
#[cfg(windows)]
pub fn trust_store() -> oximux_computer_use::TrustStore {
    oximux_computer_use::TrustStore::for_app_data_dir(
        crate::app_paths::data_dir().unwrap_or_else(std::env::temp_dir),
    )
}

/// Put the installed driver through every gate this platform has.
///
/// The two anchors are not interchangeable and neither is the call: macOS asks
/// the OS about a publisher, Windows asks a store what the user approved, and
/// the Windows signature takes that store as an argument precisely so no caller
/// can forget to name one.
#[cfg(not(windows))]
fn verified_driver() -> Result<oximux_computer_use::VerifiedDriver, oximux_computer_use::Error> {
    oximux_computer_use::prepare()
}

#[cfg(windows)]
fn verified_driver() -> Result<oximux_computer_use::VerifiedDriver, oximux_computer_use::Error> {
    oximux_computer_use::prepare(&trust_store())
}

/// Drop every grant from a previous run. Called once at startup: grants are
/// scoped to a run, and a store surviving a crash would otherwise hand a fresh
/// chat approvals nobody gave it — chat ids restart at 1 every run, so last
/// run's `chat-1` rows are addressed to the id this run's first chat will mint.
///
/// Loud on failure rather than silent. There is nothing sensible to do about it
/// from here — refusing to start over a stale grants file would be a worse
/// trade than starting — but it is the one line in the log that explains why an
/// agent could suddenly drive something nobody approved.
pub fn clear_stale_screen_control_grants() {
    if !GRANTS.clear() {
        tracing::error!(
            path = ?grants_path(),
            "could not clear screen-control grants from the last run; a chat may \
             inherit approvals nobody gave it"
        );
    }
}

/// Hands out chat ids. Monotonic and never reused within a process, so a grant
/// cannot be inherited by a later chat that happened to land on the same slot.
static NEXT_CHAT_ID: AtomicU64 = AtomicU64::new(1);

fn next_chat_id() -> u64 {
    NEXT_CHAT_ID.fetch_add(1, Ordering::Relaxed)
}

/// One chat's screen-control identity and what it may drive.
pub struct ScreenControl {
    /// This chat's driver session. Also labels the on-screen agent cursor, so
    /// it is derived rather than opaque.
    session: SessionId,
    /// What `session` was derived from, kept because the out-of-process gate is
    /// told the chat and derives the session itself. Handing it `session` would
    /// give it `oximux-oximux-chat-1` and a store lookup that matches nothing —
    /// a chat that silently holds no grants rather than an error.
    label: String,
    /// The worktree this chat runs in, plus when it started — together, what
    /// counts as "a binary this chat built for itself".
    provenance: Option<Provenance>,
    /// The table this chat's grants live in. Every chat in the app shares one.
    grants: Arc<GrantTable>,
    /// What this chat has already worked out about the pids it is driving, so
    /// the transcript can name an app instead of a number.
    ///
    /// A memo rather than a lookup because a pid only resolves while that
    /// process is alive: a transcript reloaded tomorrow holds recycled pids, and
    /// resolving one then would name the wrong app with total confidence. Filled
    /// as calls are decided — which is when the answer is still true — and
    /// deliberately not persisted, so a restored transcript says `process 4321`
    /// rather than something it cannot stand behind.
    known_apps: std::collections::HashMap<u32, String>,
}

impl ScreenControl {
    /// Start a chat's screen-control state. `cwd` is the chat's working
    /// directory; a path that cannot be canonicalized yields no provenance, and
    /// then every target is asked about rather than assumed.
    pub fn new(cwd: &Path) -> Self {
        Self::sharing(cwd, GRANTS.clone())
    }

    fn sharing(cwd: &Path, grants: Arc<GrantTable>) -> Self {
        let label = format!("chat-{}", next_chat_id());
        Self {
            session: SessionId::for_agent(&label),
            label,
            provenance: Provenance::new(cwd, SystemTime::now()),
            grants,
            known_apps: std::collections::HashMap::new(),
        }
    }

    /// Note what `pid` is called, for the transcript to use later.
    ///
    /// First writer wins. A pid recycled mid-chat is a different program, and
    /// the actions already recorded against it were aimed at the first one —
    /// relabelling them retroactively would rewrite what the trail says
    /// happened. The policy catches the recycling itself ([`Verdict::Recycled`])
    /// and refuses the new call, so nothing is driven under the stale name.
    pub fn remember_app(&mut self, pid: u32, name: String) {
        self.known_apps.entry(pid).or_insert(name);
    }

    /// What this chat knows `pid` is called, or `None` if it never resolved one.
    pub fn app_named(&self, pid: u32) -> Option<&str> {
        self.known_apps.get(&pid).map(String::as_str)
    }

    #[cfg(test)]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// What should happen to this tool call.
    ///
    /// [`Decision::NotApplicable`] for everything that is not a screen-control
    /// tool, which is the overwhelming majority — callers must treat it as
    /// "leave this alone entirely".
    pub fn decide(&self, tool_name: &str, input: &Value) -> Decision {
        decide(
            tool_name,
            input,
            &PolicyContext {
                session: &self.session,
                grants: &self.grants,
                provenance: self.provenance.as_ref(),
                // In-process, so the running binary *is* the one to protect.
                // The gate is the caller that has to be told; see `HookSpec`.
                host: None,
            },
        )
    }

    /// Let a user's approval stand, recording the grant it implies.
    ///
    /// Runs on the approval path rather than at decision time, so an `Ask` the
    /// user rejects — or never answers — leaves no grant behind.
    ///
    /// `Err(reason)` means the approval must not stand. The policy is re-run
    /// here rather than trusted from when the card appeared, because a card can
    /// sit open for a long time: another chat may have claimed that target in
    /// the meantime, and a cross-drive is refused however the user answers.
    ///
    /// A call that proceeds also marks this turn as one that drives. The hook
    /// marks the calls it allows, but a call the *user* approves never reaches
    /// it a second time — the hook stayed silent so this card could appear — so
    /// without this the first click of every approved run would go unmarked.
    pub fn approve(&self, tool_name: &str, input: &Value) -> Result<(), String> {
        match self.decide(tool_name, input) {
            Decision::NotApplicable => Ok(()),
            Decision::Allow => {
                self.grants.begin_driving_turn(&self.session);
                Ok(())
            }
            Decision::Refuse { reason } => Err(reason),
            Decision::Ask { pid: None } => Ok(()),
            Decision::Ask { pid: Some(pid) } => {
                if self.grants.grant(pid, &self.session) == Verdict::Granted {
                    self.grants.begin_driving_turn(&self.session);
                    Ok(())
                } else {
                    Err(format!("process {pid} is being driven by another chat"))
                }
            }
        }
    }

    /// Drop every grant this chat holds. Called when the chat closes and on
    /// quit — a grant that outlived its chat would let the next occupant of
    /// that target be driven with nobody having approved it.
    pub fn release(&self) {
        self.grants.release_all(&self.session);
    }

    /// Record that this chat has just photographed `pid`, or something the call
    /// did not name.
    ///
    /// Needs no grant and raises no card, which is exactly why it has to be
    /// recorded: a capture is otherwise the one thing an agent can do to the
    /// user's screen that leaves no trace anywhere they would look.
    pub fn note_capture(&self, pid: Option<u32>) {
        self.grants.note_capture(pid, &self.session);
    }

    /// This chat's turn is over, so it is no longer driving anything.
    ///
    /// Separate from [`Self::release`] because the two have different
    /// lifetimes on purpose: the grant is consent and lasts as long as the chat,
    /// while this is activity and lasts one turn. Collapsing them would either
    /// re-ask the user for consent every turn, or leave the machine believing an
    /// idle chat is still driving — which is what this exists to stop.
    pub fn end_turn_activity(&self) {
        self.grants.end_turn_activity(&self.session);
    }

    /// This chat's next turn was started from a paired phone.
    ///
    /// Recorded in the shared store rather than on this struct because the
    /// process that enforces it is the per-tool-call hook, which has no access
    /// to anything in memory here.
    pub fn begin_remote_turn(&self) {
        self.grants.begin_remote_turn(&self.session);
    }

    /// The turn ended; judge the next one on its own origin.
    pub fn end_remote_turn(&self) {
        self.grants.end_remote_turn(&self.session);
    }

    #[cfg(test)]
    pub fn granted_pids(&self) -> Vec<u32> {
        self.grants.granted_to(&self.session)
    }
}

/// The gate binary, which ships beside the app.
///
/// A sibling of the running executable rather than a `PATH` search: the gate
/// enforces this build's policy and must be this build's gate. A `PATH` hit
/// could be an older copy, or something else entirely with the same name.
///
/// Returns `None` when it is not there — which happens in a development build
/// that was compiled without it (`cargo build -p oximux-app` alone does not
/// produce it). The caller must then decline to declare screen control at all,
/// because a hook command pointing at a missing file is a hook that never
/// refuses anything.
fn gate_binary() -> Option<PathBuf> {
    let gate = std::env::current_exe()
        .ok()?
        .parent()?
        .join(oximux_computer_use::gate_binary_file_name());
    gate.is_file().then_some(gate)
}

/// Give `spec` this chat's screen-control declaration, if it can have one.
///
/// Two tiers, and the wider one is the part worth stating plainly:
///
/// - **The gate goes on every Claude chat**, opted in or not. What an opt-in
///   controls is whether the agent gets the driver's *tools*; it does not and
///   cannot control whether the agent can reach the screen, because the macOS
///   Accessibility grant behind that belongs to this process and every child
///   inherits it — an agent's shell included. A chat with no tools at all can
///   still type into the frontmost window via `osascript`, and this hook is what
///   says no. Registering it only where the tools are would put the check
///   exactly where it is least needed.
/// - **The tools go only where the user opted in**, which is both switches in
///   Screen Control settings plus a driver that passes every check.
///
/// Silently does nothing for a non-Claude transport. Hooks are that CLI's
/// mechanism, and the others have no equivalent — so there is nowhere to put the
/// policy, and a capability declared with nothing enforcing it is worse than the
/// capability being absent.
fn declare(spec: &mut ConnectSpec, chat: &ScreenControl, cx: &App) {
    let (Some(gate), Ok(host)) = (gate_binary(), std::env::current_exe()) else {
        tracing::warn!(
            "screen-control gate binary not found beside the app; chats will run without it"
        );
        return;
    };
    let grants = grants_path();

    let Some(declaration) = plan(
        spec.transport,
        chat,
        &gate,
        &host,
        &grants,
        // Verified here rather than read from whenever the settings pane last
        // looked: this is the moment the binary is handed to an agent, and the
        // only moment at which "still the one we checked" is true.
        //
        // Lazy because it spawns `codesign`. A chat that is not opted in — or
        // cannot be gated at all — never pays for it.
        || {
            if !enabled_here(&spec.cwd, cx) {
                return None;
            }
            match verified_driver() {
                Ok(driver) => Some(driver.path),
                Err(err) => {
                    tracing::warn!(%err, "screen control is on for this project but the driver is not usable");
                    None
                }
            }
        },
    ) else {
        return;
    };

    spec.mcp_servers = declaration.server.into_iter().collect();
    spec.disallowed_tools = declaration.disallowed_tools;
    spec.settings_json = Some(declaration.hook_settings);
}

/// Open `spec`'s connection with this chat's screen-control policy attached.
///
/// The only way a chat connects, and [`declare`] is private so it stays that
/// way. A spawn site that forgot to declare would not fail — it would connect a
/// chat whose shell is ungated, and nothing anywhere would say so.
///
/// Every spawn, not just the first: the flags live on the *process*, so a
/// respawn (a model switch, a Stop and resume, a rewind fork) has to declare
/// them again from scratch. The chat's identity does not change across that,
/// which is the point — grants the user approved before the respawn still hold,
/// and switching model mid-task does not make them re-approve every window.
pub fn connect_declaring(
    mut spec: ConnectSpec,
    chat: &ScreenControl,
    cx: &App,
) -> anyhow::Result<(Arc<dyn AgentConnection>, Receiver<ThreadEvent>)> {
    declare(&mut spec, chat, cx);
    oximux_agents::thread::connect(spec)
}

/// What a chat's spawn should carry, decided from resolved inputs.
///
/// Split from [`declare`] so the decision is assertable without a gate binary on
/// disk and without spawning `codesign` — the two things that make the resolver
/// above untestable, and neither of which is where a mistake would hide.
fn plan(
    transport: Transport,
    chat: &ScreenControl,
    gate: &Path,
    host: &Path,
    grants: &Path,
    driver: impl FnOnce() -> Option<PathBuf>,
) -> Option<Declaration> {
    // Nothing at all for a non-Claude chat, and the driver is not even looked
    // for. Hooks are that CLI's mechanism; the other transports have no
    // equivalent, so there is nowhere to put the policy — and a capability
    // declared with nothing enforcing it is worse than the capability being
    // absent.
    if transport != Transport::StreamJson {
        return None;
    }
    Some(oximux_computer_use::mcp::declaration(
        driver().as_deref(),
        &oximux_computer_use::mcp::HookSpec {
            command: gate,
            chat: &chat.label,
            grants,
            host,
            worktree: chat.provenance.as_ref().map(Provenance::root),
            started_at: chat.provenance.as_ref().map(Provenance::since),
        },
    ))
}

/// Has the user turned screen control on for the project a chat in `cwd`
/// belongs to?
///
/// Absent settings read as off, which is also their default.
///
/// Two questions, cheapest first. The settings answer covers an opted-in root
/// and everything beneath it, which is every chat that sits inside the project.
/// It cannot cover a worktree: OxiMux creates those *beside* the project
/// (`suggest_worktree_path` → `<parent>/oximux-wt-…`), so no containment rule
/// reaches one. Resolving the worktree back to the repository that owns it is
/// what makes "verify the build you just made" work in the isolation this
/// feature exists for, rather than that being the one workflow it silently
/// refuses.
///
/// The lookup reads two small files and only runs when the first check already
/// said no, so an ordinary chat pays nothing for it.
fn enabled_here(cwd: &Path, cx: &App) -> bool {
    let Some(settings) = cx.try_global::<ComputerUseSettings>() else {
        return false;
    };
    settings.is_enabled_for(cwd)
        || oximux_git::main_worktree_of(cwd).is_some_and(|main| settings.is_enabled_for(&main))
}

impl Drop for ScreenControl {
    fn drop(&mut self) {
        self.release();
    }
}

/// Tests that drive a real [`AgentChatView`] rather than the policy directly —
/// they are what prove the policy is actually consulted on the event path, which
/// no amount of unit testing can establish. They live here rather than in the
/// view's own test module because they belong with the thing they test, and
/// because a child module can still reach the view's private state.
#[cfg(test)]
/// A process that outlives the test, to stand in for an app being driven.
///
/// Platform-split for the same reason `oximux_computer_use::fixtures` is:
/// none of these tests are about *which* binary is running. They are about
/// grants, pids and cards, and `/bin/sleep` was only ever the cheapest way
/// to get a live pid — a fact that stayed invisible while the whole module
/// was macOS-only.
///
/// `ping -n 120 127.0.0.1` is the Windows equivalent: present on every
/// install, long-lived, and harmless.
/// What the card will call [`spawn_long_lived`]'s process.
///
/// Derived from the fixture rather than written out, so the two cannot drift:
/// the display name drops `.exe` on Windows, which is the kind of detail a
/// hardcoded string gets wrong silently.
#[cfg(test)]
fn long_lived_name() -> String {
    let program = spawn_long_lived();
    Path::new(program.get_program())
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .expect("the fixture names a program")
}

#[cfg(test)]
fn spawn_long_lived() -> std::process::Command {
    #[cfg(windows)]
    let (program, args): (&str, &[&str]) =
        (r"C:\Windows\System32\PING.EXE", &["-n", "120", "127.0.0.1"]);
    #[cfg(not(windows))]
    let (program, args): (&str, &[&str]) = ("/bin/sleep", &["120"]);

    let mut command = std::process::Command::new(program);
    command
        .args(args)
        // Silenced, or `ping`'s per-second chatter is interleaved through the
        // whole test run's output. `sleep` never had anything to say, so this
        // only became necessary once the fixture had to work on Windows.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}


#[cfg(test)]
mod view_tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use serde_json::json;

    use oximux_settings::{Density, Theme, Typography};

    use super::{long_lived_name, spawn_long_lived};
    use super::super::AgentChatView;

    /// What *this* chat has photographed, and what it is driving.
    ///
    /// Scoped to the view's own session rather than reading the store whole. A
    /// real `AgentChatView` shares the process-wide grant table, so the bare
    /// accessors report every chat in the test binary — an assertion built on
    /// them passes or fails on which other tests happen to be running
    /// alongside, which is how the first version of these tests came to be
    /// flaky.
    fn captured_by(view: &AgentChatView) -> Vec<u32> {
        let mine = view.screen_control.session.as_str();
        view.screen_control
            .grants
            .captured()
            .into_iter()
            .filter(|(_, owner)| owner == mine)
            .map(|(pid, _)| pid)
            .collect()
    }

    fn driving_by(view: &AgentChatView) -> Vec<u32> {
        let mine = view.screen_control.session.as_str();
        view.screen_control
            .grants
            .driving()
            .into_iter()
            .filter(|(_, owner)| owner == mine)
            .map(|(pid, _)| pid)
            .collect()
    }

    /// Open a chat over a stub connection, for tests that drive real events.
    async fn stub_chat(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<AgentChatView>, StubConnection) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let probe = stub.clone();
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        (window, probe)
    }

    /// A screen-control call must reach the policy from the ordinary event path
    /// — this is the whole premise of the design, since the driver is a separate
    /// process OxiMux is otherwise not between. A `type_text` with no `pid`
    /// targets whatever window the user is in, so it comes back denied, with the
    /// reason carried to the agent rather than a bare refusal.
    #[gpui::test]
    async fn a_screen_control_call_is_decided_without_asking_the_user(cx: &mut TestAppContext) {
        let (window, probe) = stub_chat(cx).await;

        window
            .update(cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::PermissionRequested {
                        request_id: "r1".into(),
                        tool_use_id: Some("t1".into()),
                        tool_name: "mcp__oximux-computer-use__type_text".into(),
                        input: json!({ "text": "rm -rf /" }),
                        description: String::new(),
                        suggestions: vec![],
                        kind: oximux_agents::thread::PermissionKind::Tool,
                    },
                    cx,
                );
                assert!(
                    view.thread.pending_permission().is_none(),
                    "policy answered it, so nothing is left waiting for the user"
                );
            })
            .expect("window update");

        let sent = probe.sent();
        let denial = sent
            .iter()
            .find(|s| s["response"]["response"]["behavior"] == "deny")
            .unwrap_or_else(|| panic!("expected a deny, got {sent:?}"));
        let message = denial["response"]["response"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains("did not name a target process"),
            "the agent must be told what was wrong: {message}"
        );
    }

    /// A capture coming back must actually reach the store.
    ///
    /// The whole fix rests on this wiring: a capture raises no card and needs no
    /// grant, so if nothing recorded it the user's only signal — the menu bar —
    /// would stay dark while an agent photographed their screen. That is the bug
    /// this closes, and nothing else would notice if the call were dropped.
    #[gpui::test]
    async fn a_capture_coming_back_is_recorded_against_the_chat(cx: &mut TestAppContext) {
        let (window, _probe) = stub_chat(cx).await;

        window
            .update(cx, |view, _window, cx| {
                let mut target = spawn_long_lived()
                    .spawn()
                    .expect("spawn a target");
                let pid = target.id();

                view.on_event(
                    ThreadEvent::ToolCallStarted {
                        id: "t1".into(),
                        name: "mcp__oximux-computer-use__get_window_state".into(),
                        input: json!({ "pid": pid, "window_id": 1 }),
                    },
                    cx,
                );
                assert!(
                    captured_by(view).is_empty(),
                    "calling a read is not yet evidence that a picture was taken"
                );

                view.on_event(
                    ThreadEvent::ToolResultImages {
                        tool_use_id: "t1".into(),
                        images: vec![oximux_agents::thread::ChatImage {
                            media_type: "image/png".into(),
                            data: "AAAA".into(),
                        }],
                    },
                    cx,
                );
                assert_eq!(
                    captured_by(view),
                    vec![pid],
                    "an image came back, so the screen was photographed"
                );

                let _ = target.kill();
                let _ = target.wait();
            })
            .expect("window update");
    }

    /// An ordinary tool returning an image must not read as a screen capture.
    ///
    /// `Read` on a PNG in the repo produces images too. Counting those would put
    /// a permanent claim in the menu bar that an agent is watching the screen,
    /// which is how a safety indicator stops being believed.
    #[gpui::test]
    async fn an_image_from_an_ordinary_tool_is_not_a_screen_capture(cx: &mut TestAppContext) {
        let (window, _probe) = stub_chat(cx).await;

        window
            .update(cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::ToolCallStarted {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: json!({ "file_path": "/tmp/diagram.png" }),
                    },
                    cx,
                );
                view.on_event(
                    ThreadEvent::ToolResultImages {
                        tool_use_id: "t1".into(),
                        images: vec![oximux_agents::thread::ChatImage {
                            media_type: "image/png".into(),
                            data: "AAAA".into(),
                        }],
                    },
                    cx,
                );
                assert!(captured_by(view).is_empty());
            })
            .expect("window update");
    }

    /// The turn ending must actually reach the driving mark.
    ///
    /// Unit-testing `end_driving_turn` proves the store forgets; only a real
    /// `TurnEnded` down the ordinary event path proves anything ever calls it.
    /// Nothing else would notice if that wiring were dropped — the feature keeps
    /// working, and the only symptom is the user's Escape key quietly staying
    /// swallowed after the agent has stopped.
    #[gpui::test]
    async fn a_turn_ending_stops_the_chat_being_reported_as_driving(cx: &mut TestAppContext) {
        let (window, _probe) = stub_chat(cx).await;

        window
            .update(cx, |view, _window, cx| {
                let target = spawn_long_lived()
                    .spawn()
                    .expect("spawn a target");
                let input = json!({ "pid": target.id() });
                view.screen_control
                    .approve("mcp__oximux-computer-use__click", &input)
                    .expect("an ungranted target is approvable");
                assert!(!driving_by(view).is_empty(), "approving starts the driving turn");

                view.on_event(
                    ThreadEvent::TurnEnded {
                        result: None,
                        usage: None,
                        is_error: false,
                        turn_diff: None,
                    },
                    cx,
                );
                assert!(
                    driving_by(view).is_empty(),
                    "the turn ended, so nothing is being driven and Escape is the user's again"
                );

                let mut target = target;
                let _ = target.kill();
                let _ = target.wait();
            })
            .expect("window update");
    }

    /// A target nobody approved leaves the card up, mid-turn, carrying the
    /// app's name.
    ///
    /// The counterpart to the auto-answered refusal above, and the half that is
    /// easy to lose: a policy change that turned `Ask` into `Allow` would still
    /// pass every refusal test while quietly deleting consent.
    ///
    /// `/bin/sleep` stands in for the un-approved app, and its lack of a bundle
    /// id is load-bearing rather than incidental — the allowlist is keyed on
    /// bundle id, so a target without one can never be pre-approved and this
    /// test cannot be made to pass by the user's own settings.
    #[gpui::test]
    async fn an_unapproved_target_leaves_the_card_up(cx: &mut TestAppContext) {
        let (window, probe) = stub_chat(cx).await;
        let mut target = spawn_long_lived()
            .spawn()
            .expect("spawn a target process");

        window
            .update(cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::PermissionRequested {
                        request_id: "r1".into(),
                        tool_use_id: Some("t1".into()),
                        tool_name: "mcp__oximux-computer-use__click".into(),
                        input: json!({ "pid": target.id() }),
                        description: String::new(),
                        suggestions: vec![],
                        kind: oximux_agents::thread::PermissionKind::Screen,
                    },
                    cx,
                );
                assert!(
                    view.thread.pending_permission().is_some(),
                    "an un-approved target must ask the user"
                );
                assert_eq!(
                    view.screen_prompts.get("t1").map(|p| p.app.as_str()),
                    Some(long_lived_name().as_str()),
                    "and the card must know which app it is asking about"
                );
            })
            .expect("window update");

        assert!(
            probe.sent().is_empty(),
            "nothing may be answered on the user's behalf"
        );
        let _ = target.kill();
        let _ = target.wait();
    }

    /// The transcript names the app for every later action against the same
    /// process, including after the card that established the name is gone.
    ///
    /// This is what makes a settled run readable: the consent card names the app
    /// once, and the thirty actions the user then approved would otherwise all
    /// read `process 4321`. The name is resolved when the call is decided —
    /// while the pid still means what it meant — and never re-derived at render
    /// time, because a recycled pid would name the wrong app with no sign that
    /// anything had changed.
    #[gpui::test]
    async fn the_app_named_on_the_card_labels_the_actions_that_follow(cx: &mut TestAppContext) {
        let (window, _probe) = stub_chat(cx).await;
        let mut target = spawn_long_lived()
            .spawn()
            .expect("spawn a target process");
        let pid = target.id();

        window
            .update(cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::PermissionRequested {
                        request_id: "r1".into(),
                        tool_use_id: Some("t1".into()),
                        tool_name: "mcp__oximux-computer-use__click".into(),
                        input: json!({ "pid": pid }),
                        description: String::new(),
                        suggestions: vec![],
                        kind: oximux_agents::thread::PermissionKind::Screen,
                    },
                    cx,
                );
                // A second call to the same process, of the kind that arrives
                // with no permission request at all once the target is granted.
                let later = oximux_agents::thread::ToolCall::new(
                    "t2",
                    "mcp__oximux-computer-use__type_text",
                    json!({ "pid": pid, "text": "hello" }),
                );
                let ctx = view.screen_context(&later);
                assert_eq!(
                    ctx.app.as_deref(),
                    Some(long_lived_name().as_str()),
                    "a later action must inherit the name the card resolved"
                );
                assert!(
                    ctx.prompt.is_none(),
                    "and must not inherit the card itself — nothing is waiting on the user here"
                );
                let header = super::super::screen_card::target(&later, ctx.app.as_deref());
                assert_eq!(
                    header.as_deref(),
                    Some(format!("\"hello\" → {}", long_lived_name()).as_str())
                );
            })
            .expect("window update");

        let _ = target.kill();
        let _ = target.wait();
    }

    /// The other half, and the one that would be expensive to get wrong: every
    /// tool that is not screen control keeps waiting for the user exactly as it
    /// did before any of this existed.
    #[gpui::test]
    async fn an_ordinary_tool_still_waits_for_the_user(cx: &mut TestAppContext) {
        let (window, probe) = stub_chat(cx).await;

        window
            .update(cx, |view, _window, cx| {
                view.on_event(
                    ThreadEvent::PermissionRequested {
                        request_id: "r1".into(),
                        tool_use_id: Some("t1".into()),
                        tool_name: "Bash".into(),
                        input: json!({ "command": "ls" }),
                        description: "ls".into(),
                        suggestions: vec![],
                        kind: oximux_agents::thread::PermissionKind::Tool,
                    },
                    cx,
                );
                assert!(
                    view.thread.pending_permission().is_some(),
                    "a non-screen-control tool must still raise a card"
                );
            })
            .expect("window update");

        assert!(
            probe.sent().is_empty(),
            "nothing may be auto-answered on the agent's behalf"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A live process standing in for "some app the agent wants to drive".
    ///
    /// Not our own pid: OxiMux is refused outright, because an agent that can
    /// drive us can approve its own consent cards. A spawned child is also
    /// closer to what these tests claim to be about.
    struct Target(std::process::Child);

    impl Target {
        fn spawn() -> Self {
            Self(
                spawn_long_lived()
                    .spawn()
                    .expect("spawn a target process"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Target {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A chat with a table of its own.
    ///
    /// Not `ScreenControl::new`: that shares the app-wide table, so tests
    /// running in parallel would contend over it and refuse each other's
    /// grants. That failure would look like a bug in the cross-drive guard
    /// while actually being two tests racing.
    fn chat() -> ScreenControl {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = GrantTable::in_data_dir(dir.path());
        // The tempdir is deliberately leaked for the test's lifetime: the store
        // has to outlive the chat that reads it, and threading a guard through
        // every call site would obscure what each test is actually asserting.
        std::mem::forget(dir);
        // A real directory, because provenance is only built when the worktree
        // canonicalizes — a path that does not exist yields `None` and the chat
        // silently loses `--worktree`/`--since`. `/tmp` made that invisible on
        // macOS by happening to exist.
        ScreenControl::sharing(&std::env::temp_dir(), Arc::new(table))
    }

    /// Two chats that can see each other's grants — the arrangement the
    /// cross-drive guard exists for.
    fn two_chats() -> (ScreenControl, ScreenControl) {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = Arc::new(GrantTable::in_data_dir(dir.path()));
        std::mem::forget(dir);
        (
            ScreenControl::sharing(Path::new("/tmp"), shared.clone()),
            ScreenControl::sharing(Path::new("/tmp"), shared),
        )
    }

    fn ns(tool: &str) -> String {
        format!("mcp__oximux-computer-use__{tool}")
    }

    fn gate() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate")
    }

    fn host() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    fn planned(transport: Transport, driver: Option<&str>) -> Option<Declaration> {
        let driver = driver.map(PathBuf::from);
        plan(
            transport,
            &chat(),
            gate(),
            host(),
            Path::new("/data/grants.json"),
            || driver,
        )
    }

    /// The invariant the whole out-of-process design rests on: the gate is told
    /// a chat and derives the session id itself, so its answer must be the id
    /// this process granted against.
    ///
    /// Worth its own test because the failure is silent and one-directional —
    /// a gate on the wrong id reads an empty grant set, so every target the user
    /// already approved starts asking again, and nothing anywhere reports why.
    #[test]
    fn the_gate_derives_the_same_session_this_process_granted_against() {
        let chat = chat();
        assert_eq!(&SessionId::for_agent(&chat.label), chat.session());
    }

    #[test]
    fn a_claude_chat_with_no_opt_in_still_carries_the_gate() {
        // The wide half of the rule. The Accessibility grant is process-wide, so
        // a chat with no screen-control tools can still drive the screen through
        // its shell — this hook is the only thing that refuses that, and a chat
        // without it is the one that most needs it.
        let declared = planned(Transport::StreamJson, None).expect("a Claude chat is declared");
        assert!(declared.server.is_none(), "no tools without an opt-in");
        assert!(declared.disallowed_tools.is_empty());

        let v: Value = serde_json::from_str(&declared.hook_settings).expect("valid json");
        let hook = &v["hooks"]["PreToolUse"][0];
        let command = hook["hooks"][0]["command"].as_str().expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        // And the shell is in its matcher, which is the entire point of putting
        // it on a chat that has no screen-control tools to police.
        let matcher = hook["matcher"].as_str().expect("matcher");
        assert!(matcher.contains("Bash"), "{matcher}");
    }

    #[test]
    fn an_opted_in_claude_chat_carries_the_tools_as_well() {
        let declared =
            planned(Transport::StreamJson, Some("/bin/cua-driver")).expect("declared");
        let server = declared.server.expect("the opt-in declares the server");
        assert_eq!(server.command, "/bin/cua-driver");
        assert!(
            declared
                .disallowed_tools
                .iter()
                .any(|t| t.ends_with("replay_trajectory")),
            "{:?}",
            declared.disallowed_tools
        );
        assert!(!declared.hook_settings.is_empty(), "and still the gate");
    }

    #[test]
    fn a_non_claude_chat_is_declared_nothing_and_never_looks_for_a_driver() {
        // Hooks are Claude's mechanism. Handing another transport the server
        // would give it the tools with only the skippable in-process check
        // behind them, which is the arrangement this whole phase exists to
        // avoid — so it gets neither the tools nor a pointless `codesign` spawn.
        for transport in [Transport::AppServer, Transport::Acp, Transport::Rpc] {
            let looked = std::cell::Cell::new(false);
            let declared = plan(
                transport,
                &chat(),
                gate(),
                host(),
                Path::new("/data/grants.json"),
                || {
                    looked.set(true);
                    Some(PathBuf::from("/bin/cua-driver"))
                },
            );
            assert!(declared.is_none(), "{transport:?} must be declared nothing");
            assert!(!looked.get(), "{transport:?} must not resolve a driver");
        }
    }

    #[test]
    fn the_hook_is_told_this_chats_own_worktree_and_start() {
        // Provenance is what lets an agent drive a binary it just built without
        // a card. The gate resolves that itself, in another process, so it has
        // to be handed the same worktree and the same clock this side used — a
        // gate given neither would ask about everything.
        let declared = planned(Transport::StreamJson, None).expect("declared");
        let v: Value = serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains("--worktree"), "{command}");
        assert!(command.contains("--since"), "{command}");
    }

    #[test]
    fn ordinary_tools_are_not_touched() {
        let chat = chat();
        for tool in ["Bash", "Edit", "Read", "mcp__github__create_issue"] {
            assert_eq!(
                chat.decide(tool, &json!({})),
                Decision::NotApplicable,
                "{tool}"
            );
        }
    }

    #[test]
    fn two_chats_never_share_a_session_identity() {
        // The premise of running several agents at once: each gets its own
        // cursor, its own grants, and cannot be confused for the other.
        let (a, b) = (chat(), chat());
        assert_ne!(a.session(), b.session());
    }

    #[test]
    fn approving_a_target_grants_it_and_the_next_call_passes() {
        let chat = chat();
        let target = Target::spawn();
        let pid = target.pid();
        let input = json!({ "pid": pid });

        assert_eq!(
            chat.decide(&ns("click"), &input),
            Decision::Ask { pid: Some(pid) }
        );
        assert!(chat.approve(&ns("click"), &input).is_ok());
        assert_eq!(chat.decide(&ns("click"), &input), Decision::Allow);
    }

    #[test]
    fn a_refused_call_grants_nothing_even_if_approved() {
        // Belt and braces: the approval path re-derives its own decision, so a
        // card that somehow reached Allow for a refused shape still records no
        // grant.
        let chat = chat();
        let target = Target::spawn();
        for input in [
            json!({ "text": "x" }),
            json!({ "pid": target.pid(), "scope": "desktop" }),
        ] {
            assert!(chat.approve(&ns("type_text"), &input).is_err());
        }
        assert!(chat.granted_pids().is_empty());
    }

    #[test]
    fn closing_a_chat_releases_its_grants() {
        let target = Target::spawn();
        let pid = target.pid();
        let input = json!({ "pid": pid });

        let (survivor, closing) = two_chats();
        assert!(closing.approve(&ns("click"), &input).is_ok());
        // While it is open, nobody else may claim its target.
        assert!(matches!(
            survivor.decide(&ns("click"), &input),
            Decision::Refuse { .. }
        ));

        drop(closing);
        // Closed — the target is free again, so it is merely unapproved.
        assert_eq!(
            survivor.decide(&ns("click"), &input),
            Decision::Ask { pid: Some(pid) }
        );
    }

    #[test]
    fn grants_from_a_previous_run_do_not_survive_a_restart() {
        // Grants live in a file so the per-tool-call hook can read them, which
        // means they outlive the process that made them. That is deliberate, and
        // it is exactly what makes the startup clear load-bearing: chat ids
        // restart at 1 every run, so a surviving `chat-1` row is addressed to
        // the id the next run's first chat mints.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(oximux_computer_use::grants::GRANTS_FILE_NAME);
        let target = Target::spawn();
        let input = json!({ "pid": target.pid() });

        let last_run = ScreenControl::sharing(Path::new("/tmp"), Arc::new(GrantTable::at(&path)));
        assert!(last_run.approve(&ns("click"), &input).is_ok());
        // Forgotten, not dropped: `Drop` releases, and a clean quit is not the
        // case this guards. A crash is, and it runs no destructors.
        std::mem::forget(last_run);
        assert!(
            !GrantTable::at(&path).all().is_empty(),
            "the grant must genuinely reach disk, or clearing it proves nothing"
        );

        // What startup does, before any chat exists.
        assert!(GrantTable::at(&path).clear());

        let this_run = ScreenControl::sharing(Path::new("/tmp"), Arc::new(GrantTable::at(&path)));
        assert_eq!(
            this_run.decide(&ns("click"), &input),
            Decision::Ask { pid: Some(target.pid()) },
            "the first chat of a new run must be asked again"
        );
    }

    /// The hazard the test above defends against, stated directly.
    ///
    /// [`next_chat_id`] counts from 1 per *process*, so `chat-1` is not a unique
    /// name — it is the id both the last run's first chat and this run's first
    /// chat get. Nothing else in the store distinguishes them, so a surviving
    /// row is not stale data to be tidied up later; it is a live grant with this
    /// run's chat already named on it.
    #[test]
    fn a_surviving_grant_would_be_inherited_by_the_next_runs_first_chat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = GrantTable::in_data_dir(dir.path());
        let target = Target::spawn();

        // Two runs, each minting the id its first chat gets.
        let last_run = SessionId::for_agent("chat-1");
        let this_run = SessionId::for_agent("chat-1");
        assert_eq!(table.grant(target.pid(), &last_run), Verdict::Granted);
        assert_eq!(
            table.check(target.pid(), &this_run),
            Verdict::Granted,
            "the ids are indistinguishable, which is the whole problem"
        );

        assert!(table.clear());
        assert_eq!(table.check(target.pid(), &this_run), Verdict::Ungranted);
    }

    #[test]
    fn a_phone_started_turn_is_refused_and_the_next_local_one_is_not() {
        // The whole inbound gate as a chat sees it. A phone prompt marks the
        // turn; every screen-control call in it is refused however addressed;
        // the turn ending restores the ordinary consent path.
        let chat = chat();
        let target = Target::spawn();
        let input = json!({ "pid": target.pid() });

        chat.begin_remote_turn();
        let refusal = chat.decide(&ns("click"), &input);
        assert!(
            matches!(&refusal, Decision::Refuse { reason } if reason.contains("paired phone")),
            "{refusal:?}"
        );
        // And a card answered from the phone cannot promote it either — the
        // approval path re-decides rather than trusting the earlier verdict.
        assert!(chat.approve(&ns("click"), &input).is_err());
        assert!(chat.granted_pids().is_empty());

        chat.end_remote_turn();
        assert_eq!(
            chat.decide(&ns("click"), &input),
            Decision::Ask { pid: Some(target.pid()) }
        );
    }

    #[test]
    fn approving_a_target_marks_the_turn_as_one_that_drives() {
        // The card path never reaches the hook a second time, so if the approval
        // did not mark the turn itself, the first click of every approved run
        // would happen with the kill switch disarmed and the menu bar silent.
        let chat = chat();
        let target = Target::spawn();
        let input = json!({ "pid": target.pid() });

        assert!(chat.grants.driving().is_empty());
        assert!(chat.approve(&ns("click"), &input).is_ok());
        assert_eq!(
            chat.grants.driving(),
            vec![(target.pid(), chat.session.as_str().to_string())]
        );
    }

    #[test]
    fn ending_the_turn_stops_the_claim_but_keeps_the_approval() {
        // The lifetimes that must differ: consent is per chat, activity is per
        // turn. The next turn drives the same target without a second card.
        let chat = chat();
        let target = Target::spawn();
        let input = json!({ "pid": target.pid() });
        assert!(chat.approve(&ns("click"), &input).is_ok());

        chat.end_turn_activity();
        assert!(chat.grants.driving().is_empty(), "nothing is being driven between turns");
        assert_eq!(
            chat.decide(&ns("click"), &input),
            Decision::Allow,
            "but the approval stands, so the next turn is not asked again"
        );
    }

    #[test]
    fn a_refused_call_never_marks_the_turn() {
        // Otherwise a chat that only ever got refused would still take the
        // user's Escape key for the rest of the turn.
        let chat = chat();
        let input = json!({ "text": "hi" }); // no pid: refused outright
        assert!(chat.approve(&ns("type_text"), &input).is_err());
        assert!(chat.grants.driving().is_empty());
    }

    #[test]
    fn one_chat_cannot_drive_another_chats_target() {
        // The invariant the whole feature rests on. Two chats, one process:
        // approving in A must not enable B.
        let (a, b) = two_chats();
        let target = Target::spawn();
        let input = json!({ "pid": target.pid() });
        assert!(a.approve(&ns("type_text"), &input).is_ok());

        let refusal = b.decide(&ns("type_text"), &input);
        assert!(
            matches!(&refusal, Decision::Refuse { reason } if reason.contains("another chat")),
            "{refusal:?}"
        );
        // Nor can B promote itself by having its card approved: B's card may
        // have gone up while the target was still free, and the answer arrives
        // after A claimed it. The approval path re-decides for exactly this.
        assert!(b.approve(&ns("type_text"), &input).is_err());
    }
}

/// Which chats the project opt-in actually reaches.
///
/// Against a repository and worktree git created, not a hand-built pair of
/// pointer files: the worktree case exists precisely because the layout is
/// git's rather than ours, and a fixture that encoded our belief about it would
/// agree with the code and disagree with the machine.
#[cfg(test)]
mod enablement_tests {
    use super::*;

    use gpui::TestAppContext;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A committed repo plus a linked worktree beside it — the shape
    /// `suggest_worktree_path` produces, where the worktree is a *sibling* of
    /// the project rather than a child.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("mkdir");
        git(&project, &["init", "-b", "main"]);
        git(&project, &["config", "user.email", "t@example.com"]);
        git(&project, &["config", "user.name", "t"]);
        std::fs::write(project.join("a.txt"), "v1\n").expect("write");
        git(&project, &["add", "a.txt"]);
        git(&project, &["commit", "-m", "init"]);

        let worktree = tmp.path().join("oximux-wt-feat-x");
        git(
            &project,
            &["worktree", "add", worktree.to_str().expect("utf8"), "-b", "feat"],
        );
        (tmp, project, worktree)
    }

    fn with_settings(settings: ComputerUseSettings, cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(settings));
    }

    fn on_for(root: &Path) -> ComputerUseSettings {
        let mut s = ComputerUseSettings {
            enabled: true,
            ..Default::default()
        };
        s.enable_project(root);
        s
    }

    #[gpui::test]
    async fn an_opted_in_project_reaches_its_own_directory(cx: &mut TestAppContext) {
        let (_tmp, project, _wt) = repo_with_worktree();
        with_settings(on_for(&project), cx);
        assert!(cx.update(|cx| enabled_here(&project, cx)));
    }

    #[gpui::test]
    async fn it_reaches_a_chat_opened_on_a_subdirectory(cx: &mut TestAppContext) {
        let (_tmp, project, _wt) = repo_with_worktree();
        with_settings(on_for(&project), cx);
        let sub = project.join("crates").join("thing");
        assert!(cx.update(|cx| enabled_here(&sub, cx)));
    }

    #[gpui::test]
    async fn it_reaches_a_worktree_of_that_project(cx: &mut TestAppContext) {
        // The workflow this feature exists for: the agent builds in its own
        // worktree and then drives the app it just built. The worktree is a
        // sibling of the project, so containment alone answers no and the
        // git-side resolution is what makes the opt-in mean what it says.
        let (_tmp, project, worktree) = repo_with_worktree();
        assert!(!worktree.starts_with(&project), "must be a sibling");
        with_settings(on_for(&project), cx);
        assert!(cx.update(|cx| enabled_here(&worktree, cx)));
    }

    #[gpui::test]
    async fn it_does_not_reach_an_unrelated_repository(cx: &mut TestAppContext) {
        let (_tmp, project, _wt) = repo_with_worktree();
        let (_other_tmp, other, other_wt) = repo_with_worktree();
        with_settings(on_for(&project), cx);
        assert!(!cx.update(|cx| enabled_here(&other, cx)));
        // Including that repository's worktrees, which resolve to *it*.
        assert!(!cx.update(|cx| enabled_here(&other_wt, cx)));
    }

    #[gpui::test]
    async fn the_master_switch_still_governs_every_one_of_those(cx: &mut TestAppContext) {
        // Widening which directories a project covers must not widen what the
        // project opt-in is: it is one of two switches and stays that way.
        let (_tmp, project, worktree) = repo_with_worktree();
        let mut settings = on_for(&project);
        settings.enabled = false;
        with_settings(settings, cx);
        assert!(!cx.update(|cx| enabled_here(&project, cx)));
        assert!(!cx.update(|cx| enabled_here(&worktree, cx)));
    }

    #[gpui::test]
    async fn absent_settings_read_as_off(cx: &mut TestAppContext) {
        let (_tmp, project, _wt) = repo_with_worktree();
        assert!(!cx.update(|cx| enabled_here(&project, cx)));
    }
}
