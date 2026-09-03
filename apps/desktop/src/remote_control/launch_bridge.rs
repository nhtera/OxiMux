//! The inbound half of remote control: a launch request crossing from the
//! dispatcher's tokio task onto the GPUI thread, and its answer coming back.
//!
//! Every other remote capability reaches something the registry already holds —
//! a live `AgentConnection` — and needs no UI involvement. Starting a session is
//! the exception: it opens a tab, resolves a backend, and spawns a child
//! process, all of which only the view layer can do.
//!
//! So this is the first inbound path in the codebase. The direction is new; the
//! mechanism is not. `RemoteControl::subscribe_pairings` already carries
//! notifications *out* to a `cx.spawn` loop that updates a GPUI entity through a
//! `WeakEntity`; this is the same bridge pointed the other way, with a oneshot
//! riding along so the caller learns the new session's id rather than polling
//! for it.

use oximux_remote_host::{LaunchError, SessionLauncher};
use tokio::sync::{mpsc, oneshot};

/// One request to open a session, plus where to send the answer.
pub struct LaunchRequest {
    pub cwd: String,
    pub agent_id: Option<String>,
    /// The model to open the session on, fixed at spawn. `None` = the agent's
    /// own default.
    pub model: Option<String>,
    /// Sent as the session's first message once it opens. Remote launches pass
    /// `None` (an empty session for the phone to type into); the scheduler
    /// passes its schedule's prompt.
    pub initial_prompt: Option<String>,
    /// The new session's id, or why it could not be opened.
    ///
    /// A oneshot rather than a fire-and-forget send because the RPC contract
    /// returns the id: a phone that had to re-list and guess which row was new
    /// would race any other session opening at the same moment.
    pub reply: oneshot::Sender<Result<String, LaunchError>>,
}

/// How many launch requests may queue before the UI thread has drained them.
///
/// Small on purpose. These arrive at human speed — someone tapping "new
/// session" — so a deep queue would only ever hold requests nobody is waiting on
/// any more. A full queue is reported as [`LaunchError::Unavailable`], which is
/// honest: the desktop is not keeping up.
const QUEUE: usize = 4;

/// The dispatcher's end of the bridge. Cloneable so the scheduler's firer can
/// share the same queue the remote launcher uses — one GPUI drain loop serves
/// both.
#[derive(Clone)]
pub struct BridgeLauncher {
    tx: mpsc::Sender<LaunchRequest>,
}

impl BridgeLauncher {
    /// Open a session, optionally seeding its first message — the scheduler's
    /// entry point. The plain [`SessionLauncher::create`] delegates here with
    /// no prompt.
    pub async fn create_with_prompt(
        &self,
        cwd: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
        initial_prompt: Option<String>,
    ) -> Result<String, LaunchError> {
        let (reply, answer) = oneshot::channel();
        let request = LaunchRequest {
            cwd: cwd.to_string(),
            agent_id: agent_id.map(str::to_string),
            model: model.map(str::to_string),
            initial_prompt,
            reply,
        };
        // `try_send` rather than `send`: a full queue or a dropped receiver
        // should fail now, not park the caller forever. The caller gets an
        // answer either way.
        self.tx.try_send(request).map_err(|_| LaunchError::Unavailable)?;
        // A dropped sender means the UI side gave up without replying — treat it
        // as a failure rather than hanging, since nothing else will ever answer.
        answer.await.map_err(|_| LaunchError::Unavailable)?
    }
}

/// Build both ends. The receiver is drained by the GPUI side; dropping it makes
/// every later launch fail with [`LaunchError::Unavailable`], which is the right
/// answer once the window that would host the tab is gone.
pub fn launch_bridge() -> (BridgeLauncher, mpsc::Receiver<LaunchRequest>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (BridgeLauncher { tx }, rx)
}

#[async_trait::async_trait]
impl SessionLauncher for BridgeLauncher {
    async fn create(
        &self,
        cwd: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, LaunchError> {
        self.create_with_prompt(cwd, agent_id, model, None).await
    }
}

/// Whether `cwd` is a directory this desktop can actually open a session in.
///
/// The trait requires implementations to validate the path themselves: the
/// dispatcher checks *authorization*, not filesystem reality, and the two are
/// genuinely different questions. A device may be fully entitled to start a
/// session and still name a directory that does not exist.
///
/// Checked here rather than deeper in so the failure is a clean
/// [`LaunchError::BadWorkingDirectory`] instead of surfacing as a spawn failure
/// several layers down, where the error text would carry host paths.
pub fn usable_working_directory(cwd: &str) -> Result<std::path::PathBuf, LaunchError> {
    let path = std::path::Path::new(cwd);
    // Canonicalized so a path reaching the tab is absolute and symlink-resolved,
    // matching what the desktop's own session-open path produces. This is not a
    // containment check — arbitrary paths are the agreed scope — it is about the
    // tab showing a real, stable location rather than something relative to
    // whatever directory this process happens to be in.
    let resolved = path.canonicalize().map_err(|_| LaunchError::BadWorkingDirectory)?;
    if !resolved.is_dir() {
        return Err(LaunchError::BadWorkingDirectory);
    }
    Ok(resolved)
}

/// Drain launch requests on the GPUI thread, opening a tab for each.
///
/// Hosted at the top level of `app.run` rather than inside a view: opening a
/// session is not scoped to any existing one, and picking a target window needs
/// the global window set — which only `&App` can enumerate, not a
/// `Context<SomeView>`. It also has to outlive any single window.
///
/// The handoff exists because `AsyncApp` is not `Send` while the dispatcher runs
/// on a multi-threaded tokio runtime, so the RPC task cannot touch GPUI at all.
/// It parks on a oneshot instead and this loop answers it.
pub fn serve_launches(rx: tokio::sync::mpsc::Receiver<LaunchRequest>, cx: &mut gpui::App) {
    let mut rx = rx;
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Some(request) = rx.recv().await {
            // `AsyncApp::update` returns the closure's value directly in this
            // gpui — there is no outer Result to unwrap, so the launch's own
            // error reaches the phone unflattened and it learns whether the
            // directory was bad or the launch simply failed.
            //
            // A send failure means the caller gave up (its RPC timed out, or the
            // connection dropped) — nothing to do about it, and the tab it asked
            // for is already open either way.
            // Split before sending: `send` consumes the reply channel, so the
            // request cannot still be borrowed by the closure at that point.
            let LaunchRequest { cwd, agent_id, model, initial_prompt, reply } = request;
            let opened = cx.update(|cx| {
                open_session(&cwd, agent_id.as_deref(), model, initial_prompt, cx)
            });
            let _ = reply.send(opened);
        }
    })
    .detach();
}

/// Open one session, returning the id the phone should subscribe to.
///
/// `initial_prompt`, when present, is sent as the session's first message. Remote
/// launches pass `None` (they open an empty session for the phone to type into);
/// the scheduler passes the schedule's prompt.
pub(crate) fn open_session(
    cwd: &str,
    agent_id: Option<&str>,
    model: Option<String>,
    initial_prompt: Option<String>,
    cx: &mut gpui::App,
) -> Result<String, LaunchError> {
    let cwd = usable_working_directory(cwd)?;

    // The adapter the request named, or the desktop's own default — the same
    // resolution the left rail performs, so a remote launch and a local one
    // produce the same kind of session rather than two subtly different ones.
    let settings = cx
        .try_global::<oximux_settings::AgentLaunchSettings>()
        .cloned()
        .unwrap_or_default();
    // A request that names no agent (a remote quick-add — the local launcher
    // always names one via its picker) uses the configured default, or falls back
    // to a chat-capable agent so it still starts before the user has picked a
    // default. `None` only when the host has no chat-capable agent at all.
    let adapter_id = match agent_id {
        Some(id) => id.to_string(),
        None => settings.default_chat_agent().ok_or(LaunchError::Failed)?,
    };
    // Refused rather than silently defaulted: starting a *different* agent than
    // the one asked for is a worse outcome than starting none, and the phone
    // cannot tell the difference afterwards.
    if !settings.chat_capable(&adapter_id) {
        return Err(LaunchError::Failed);
    }
    let backend = crate::workspace_root::chat_backend_for(&settings, &adapter_id);
    // Refused rather than dropped. ACP takes no model at spawn — `connect`'s
    // `Transport::Acp` arm never reads `ConnectSpec.model` — so passing one here
    // would open the session on the agent's default while every record said
    // otherwise. This is the reachable case: unlike the headless host, the
    // desktop resolves any chat-capable adapter, ACP presets included.
    if model.is_some() && !backend.transport.takes_model_at_spawn() {
        return Err(LaunchError::ModelUnsupported);
    }

    // The first registered window. A desktop with several open has no signal
    // about which one the phone meant, and asking would defeat the point of
    // starting a session from away — so this picks deterministically rather than
    // guessing at intent.
    let windows = crate::platform::window_registry::all_windows(cx);
    let (_, workspace) = windows.into_iter().next().ok_or(LaunchError::Unavailable)?;

    let panes = workspace.read(cx).active_project_panes().ok_or(LaunchError::Unavailable)?;

    // A window handle is required to open a tab, and only the window that owns
    // this workspace will do.
    let window = cx
        .windows()
        .into_iter()
        .next()
        .ok_or(LaunchError::Unavailable)?;
    window
        .update(cx, |_, window, cx| {
            panes.update(cx, |panes, cx| {
                panes.open_agent_chat_tab_in_active_group(
                    cwd,
                    model,
                    backend,
                    initial_prompt,
                    window,
                    cx,
                )
            })
        })
        .map_err(|_| LaunchError::Unavailable)?
        .ok_or(LaunchError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_launch_request_carries_its_answer_back() {
        let (launcher, mut rx) = launch_bridge();
        let ui = tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            assert_eq!(req.cwd, "/work");
            assert_eq!(req.agent_id.as_deref(), Some("claude"));
            assert_eq!(
                req.model.as_deref(),
                Some("opus-5"),
                "the model rides the request, so the tab opens on it rather than                  being switched afterwards"
            );
            let _ = req.reply.send(Ok("sess-7".into()));
        });
        assert_eq!(
            launcher.create("/work", Some("claude"), Some("opus-5")).await.unwrap(),
            "sess-7"
        );
        ui.await.unwrap();
    }

    /// No UI side at all — the window is gone, or remote was enabled before one
    /// existed. This must fail rather than park the RPC forever.
    #[tokio::test]
    async fn a_dropped_receiver_fails_instead_of_hanging() {
        let (launcher, rx) = launch_bridge();
        drop(rx);
        assert!(matches!(
            launcher.create("/work", None, None).await,
            Err(LaunchError::Unavailable)
        ));
    }

    /// The UI side took the request and then went away without answering. The
    /// caller must not wait on a reply nothing will ever send.
    #[tokio::test]
    async fn a_dropped_reply_channel_fails_instead_of_hanging() {
        let (launcher, mut rx) = launch_bridge();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            drop(req.reply);
        });
        assert!(matches!(
            launcher.create("/work", None, None).await,
            Err(LaunchError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn a_full_queue_is_refused_rather_than_awaited() {
        let (launcher, _rx) = launch_bridge(); // receiver held, never drained
        // Fill the queue with requests nobody reads, then prove the next one is
        // refused promptly instead of blocking the connection's request slot.
        // The futures must be *polled* to enqueue anything — calling `create`
        // and dropping the future would leave the queue empty and the assertion
        // below passing for the wrong reason.
        let mut held = Vec::new();
        for _ in 0..QUEUE {
            held.push(Box::pin(launcher.create("/work", None, None)));
        }
        for f in &mut held {
            // Poll each once so it lands in the queue.
            let _ = futures::poll!(f.as_mut());
        }
        assert!(matches!(
            launcher.create("/work", None, None).await,
            Err(LaunchError::Unavailable)
        ));
    }

    #[test]
    fn a_missing_directory_is_refused() {
        assert!(usable_working_directory("/definitely/not/here").is_err());
    }

    #[test]
    fn a_file_is_not_a_working_directory() {
        let file = std::env::temp_dir().join("oximux-launch-bridge-test-file");
        std::fs::write(&file, b"x").unwrap();
        // `canonicalize` succeeds for a file, so the is_dir check is what
        // actually rejects this — worth pinning, since dropping it would leave a
        // session opening in a "directory" that is a file.
        assert!(usable_working_directory(file.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_real_directory_resolves_to_an_absolute_path() {
        let dir = std::env::temp_dir();
        let resolved = usable_working_directory(dir.to_str().unwrap()).expect("temp dir is usable");
        assert!(resolved.is_absolute());
    }
}
