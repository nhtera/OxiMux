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
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;

use oximux_computer_use::grants::{GrantTable, Provenance, Verdict};
use oximux_computer_use::policy::{PolicyContext, decide};
use oximux_computer_use::session::SessionId;
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
static GRANTS: LazyLock<Arc<GrantTable>> = LazyLock::new(|| {
    Arc::new(match grants_path() {
        Some(path) => GrantTable::at(path),
        // No data dir is a broken install; keep screen control working against
        // a temp store rather than failing the whole app to death over it.
        None => GrantTable::at(std::env::temp_dir().join(oximux_computer_use::grants::GRANTS_FILE_NAME)),
    })
});

/// The store's path, which is also what gets handed to the hook process so both
/// sides cannot drift onto different files.
pub fn grants_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| {
        dir.join("dev.nhtera.oximux")
            .join(oximux_computer_use::grants::GRANTS_FILE_NAME)
    })
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
    /// The worktree this chat runs in, plus when it started — together, what
    /// counts as "a binary this chat built for itself".
    provenance: Option<Provenance>,
    /// The table this chat's grants live in. Every chat in the app shares one.
    grants: Arc<GrantTable>,
}

impl ScreenControl {
    /// Start a chat's screen-control state. `cwd` is the chat's working
    /// directory; a path that cannot be canonicalized yields no provenance, and
    /// then every target is asked about rather than assumed.
    pub fn new(cwd: &Path) -> Self {
        Self::sharing(cwd, GRANTS.clone())
    }

    fn sharing(cwd: &Path, grants: Arc<GrantTable>) -> Self {
        Self {
            session: SessionId::for_agent(&format!("chat-{}", next_chat_id())),
            provenance: Provenance::new(cwd, SystemTime::now()),
            grants,
        }
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
    pub fn approve(&self, tool_name: &str, input: &Value) -> Result<(), String> {
        match self.decide(tool_name, input) {
            Decision::NotApplicable | Decision::Allow => Ok(()),
            Decision::Refuse { reason } => Err(reason),
            Decision::Ask { pid: None } => Ok(()),
            Decision::Ask { pid: Some(pid) } => {
                if self.grants.grant(pid, &self.session) == Verdict::Granted {
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
mod view_tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use serde_json::json;

    use oximux_settings::{Density, Theme, Typography};

    use super::super::AgentChatView;

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
        let mut target = std::process::Command::new("/bin/sleep")
            .arg("120")
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
                    Some("sleep"),
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
                std::process::Command::new("/bin/sleep")
                    .arg("120")
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
        ScreenControl::sharing(Path::new("/tmp"), Arc::new(table))
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
