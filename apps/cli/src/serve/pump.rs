//! The per-session event pump — serve's stand-in for the desktop's chat view.
//!
//! On the desktop, a view drains the agent's event receiver, feeds the
//! registry (so remote subscribers see the stream), folds the transcript, and
//! persists it. Headless, this task does exactly those four things and nothing
//! else: drain → ingest → fold → persist. One pump per live session; it ends
//! when the agent's event channel closes or serve drains for shutdown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oximux_agent_core::thread::ThreadEvent;
use oximux_agents::session_registry::{RemotePrompt, SessionHandle, SessionMeta, SessionRegistry};
use oximux_agents::thread::ChatThread;
use oximux_storage::SettingsRepo;

use super::blob::{self, ChatBlob};
use super::catalog::SessionIndex;

/// How often a dirty fold is written even mid-turn, so a long turn's transcript
/// survives a crash reasonably intact.
const SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Every live pump's drain-facing state, so shutdown can ask "is any turn
/// still in flight?" and tell every pump to finalize.
#[derive(Default)]
pub struct PumpSet {
    inner: Mutex<HashMap<String, PumpState>>,
    /// Flipped once at drain; pumps finalize (mark interrupted turns, save)
    /// when they observe it.
    finalize: tokio::sync::watch::Sender<bool>,
}

struct PumpState {
    turn_active: tokio::sync::watch::Receiver<bool>,
}

impl PumpSet {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sessions with a turn currently in flight.
    pub fn active_turns(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|p| *p.turn_active.borrow())
            .count()
    }

    /// Tell every pump to finalize (drain path). Idempotent.
    pub fn finalize_all(&self) {
        let _ = self.finalize.send(true);
    }

    /// Whether any pump is still registered (finalization still running).
    pub fn live_pumps(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    fn register(&self, session_id: &str, turn_active: tokio::sync::watch::Receiver<bool>) {
        self.inner
            .lock()
            .unwrap()
            .insert(session_id.to_string(), PumpState { turn_active });
    }

    fn remove(&self, session_id: &str) {
        self.inner.lock().unwrap().remove(session_id);
    }
}

/// Everything a pump needs at start.
pub struct PumpSpec {
    pub session_id: String,
    pub handle: Arc<SessionHandle>,
    /// The agent's event stream (std receiver — the transports hand one out).
    pub events: std::sync::mpsc::Receiver<ThreadEvent>,
    /// Events already consumed before the pump started (a fresh launch reads
    /// up to `SessionInit` to learn the id). Ingested and folded first, in
    /// order, so nothing is lost to the handoff.
    pub buffered: Vec<ThreadEvent>,
    /// The persisted state to rehydrate from (a resume), else a fresh blob.
    pub seed: ChatBlob,
    pub settings: SettingsRepo,
    /// To unregister the session when its agent dies, so it returns to the
    /// dormant set (listable, resumable) instead of being stranded as
    /// live-looking-but-dead until the whole host restarts.
    pub registry: Arc<SessionRegistry>,
    /// The shared list-row index, refreshed on every persist so a session the
    /// launcher just minted (or whose title/model evolved) lists correctly
    /// the moment its agent dies.
    pub index: Arc<SessionIndex>,
    /// Run once when the pump ends, however it ends (agent exit or drain).
    /// The launcher uses it to revoke the agent's local-control credential:
    /// a secret outliving the process it was minted for is a secret nothing
    /// is watching.
    pub on_end: Option<Box<dyn FnOnce() + Send>>,
}

/// Start one session's pump. Returns immediately; the pump runs until the
/// agent's stream closes or serve finalizes.
pub fn start(spec: PumpSpec, pumps: Arc<PumpSet>) {
    let (turn_tx, turn_rx) = tokio::sync::watch::channel(false);
    pumps.register(&spec.session_id, turn_rx);
    let mut finalize = pumps.finalize.subscribe();
    let pumps_for_end = pumps.clone();

    let PumpSpec {
        session_id,
        handle,
        events,
        buffered,
        seed,
        settings,
        registry,
        index,
        on_end,
    } = spec;

    // Bridge the transports' std receiver onto the async world. The blocking
    // thread dies with the channel; nothing joins it.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name(format!("pump-{session_id}"))
        .spawn(move || {
            while let Ok(event) = events.recv() {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        })
        .expect("spawn pump bridge thread");

    // Prompts injected over the protocol reach the fold through the handle's
    // relay sink — no backend echoes the user's half.
    let (prompt_tx, mut prompt_rx) = futures::channel::mpsc::unbounded::<RemotePrompt>();
    handle.set_remote_prompt_sink(prompt_tx);

    tokio::spawn(async move {
        let mut fold = ChatThread::rehydrated(
            Some(session_id.clone()),
            seed.model.clone(),
            seed.entries.clone(),
            seed.slash_commands.clone(),
        );
        fold.session_meta = seed.session_meta.clone();
        let mut blob = seed;
        let mut persist = Persist::new(settings, index, &fold);
        // A resumed session publishes its history immediately — `--resume`
        // replays nothing, so without this the transcript RPCs would answer
        // empty until the first new turn.
        persist.save_now(&handle, &mut blob, &fold);

        for event in buffered {
            apply(&handle, &mut fold, &event, &turn_tx, /* ingest */ true);
        }
        persist.maybe_save(&handle, &mut blob, &fold);

        let mut save_tick = tokio::time::interval(SAVE_INTERVAL);
        save_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        // The agent's stream closed: the child exited. An
                        // in-flight turn can never finish now — mark it so
                        // rather than leaving "running" on disk forever.
                        if fold.turn_active {
                            fold.interrupt();
                        }
                        let _ = turn_tx.send(false);
                        persist.save_now(&handle, &mut blob, &fold);
                        // Back to dormant: with the registry entry gone the
                        // catalog lists it again and `open()` resumes it
                        // fresh, instead of "already live" answering for a
                        // corpse until the host restarts.
                        registry.unregister(&session_id);
                        break;
                    };
                    let settled = matches!(
                        event,
                        ThreadEvent::TurnEnded { .. }
                            | ThreadEvent::PermissionRequested { .. }
                            | ThreadEvent::QuestionAsked { .. }
                    );
                    apply(&handle, &mut fold, &event, &turn_tx, true);
                    if settled {
                        persist.save_now(&handle, &mut blob, &fold);
                    }
                }
                prompt = futures::StreamExt::next(&mut prompt_rx) => {
                    if let Some(prompt) = prompt {
                        fold.push_user_message_with_images(prompt.text, prompt.images);
                        let _ = turn_tx.send(true);
                    }
                }
                _ = save_tick.tick() => {
                    persist.maybe_save(&handle, &mut blob, &fold);
                }
                _ = finalize.changed() => {
                    if !*finalize.borrow() {
                        continue;
                    }
                    // Drain: an unfinished turn is reported as interrupted,
                    // never silently truncated.
                    if fold.turn_active {
                        fold.interrupt();
                    }
                    let _ = turn_tx.send(false);
                    persist.save_now(&handle, &mut blob, &fold);
                    break;
                }
            }
        }
        pumps_for_end.remove(&session_id);
        if let Some(on_end) = on_end {
            on_end();
        }
    });
}

/// Fold + registry bookkeeping for one event.
fn apply(
    handle: &SessionHandle,
    fold: &mut ChatThread,
    event: &ThreadEvent,
    turn_tx: &tokio::sync::watch::Sender<bool>,
    ingest: bool,
) {
    if ingest {
        handle.ingest(event.clone());
    }
    fold.apply(event);
    let _ = turn_tx.send(fold.turn_active);
    // Keep the registry's list-row metadata current on the events that change it.
    if matches!(
        event,
        ThreadEvent::SessionInit { .. }
            | ThreadEvent::TitleUpdated { .. }
            | ThreadEvent::ModeChanged { .. }
    ) {
        handle.set_meta(SessionMeta {
            title: fold.title.clone(),
            model: fold.model.clone(),
            permission_mode: fold.permission_mode.clone(),
            cwd: fold.session_meta.cwd.clone().map(std::path::PathBuf::from),
        });
    }
}

/// Revision-gated persistence: publish the fold to the registry (for the
/// transcript RPCs) and write the blob (for restarts), only when the fold
/// actually changed since the last write.
struct Persist {
    settings: SettingsRepo,
    index: Arc<SessionIndex>,
    last_saved_revision: u64,
}

impl Persist {
    fn new(settings: SettingsRepo, index: Arc<SessionIndex>, fold: &ChatThread) -> Self {
        // Start one behind so the initial save always runs.
        Self { settings, index, last_saved_revision: fold.revision().wrapping_sub(1) }
    }

    fn maybe_save(&mut self, handle: &SessionHandle, blob: &mut ChatBlob, fold: &ChatThread) {
        if fold.revision() != self.last_saved_revision {
            self.save_now(handle, blob, fold);
        }
    }

    fn save_now(&mut self, handle: &SessionHandle, blob: &mut ChatBlob, fold: &ChatThread) {
        self.last_saved_revision = fold.revision();
        blob.model = fold.model.clone();
        blob.entries = fold.entries.clone();
        blob.slash_commands = fold.slash_commands.clone();
        blob.session_meta = fold.session_meta.clone();
        match serde_json::to_string(&blob.entries) {
            Ok(entries_json) => handle.publish_transcript(entries_json, fold.model.clone()),
            Err(err) => tracing::warn!(%err, "fold serialize failed; transcript not published"),
        }
        // The SQLite write happens inline: the blob is bounded by one
        // conversation and this task owns no latency contract — there is no
        // UI thread here to protect.
        blob::save(&self.settings, blob);
        // Keep the shared list-row index as fresh as the blob, so the session
        // lists correctly the moment it goes dormant.
        self.index.note(
            &blob.session_id,
            fold.title.clone().or_else(|| blob.derived_title()),
            fold.model.clone(),
            blob.session_meta.cwd.clone().map(std::path::PathBuf::from),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::StubConnection;

    fn wait_until(deadline_ms: u64, mut probe: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        while std::time::Instant::now() < deadline {
            if probe() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// The bug a review caught before it shipped: an agent whose stream closes
    /// must return its session to the dormant set — final state persisted,
    /// registry entry gone (so `open()` can resume it fresh), and the shared
    /// index still listing it. Without the unregister, one agent crash
    /// stranded the session as live-looking-but-dead until a host restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_agent_returns_its_session_to_dormant() {
        let registry = Arc::new(SessionRegistry::new());
        let handle = registry.register("s-1".into(), Arc::new(StubConnection::default()));
        let db = oximux_storage::open_memory().unwrap();
        let settings = SettingsRepo::new(db);
        let index = Arc::new(SessionIndex::default());
        let pumps = PumpSet::new();
        let (tx, rx) = std::sync::mpsc::channel();
        // The credential-revoking hook the launcher installs, stubbed: what
        // matters here is that a dead agent runs it at all, since a credential
        // that outlives its process is the leak this hook exists to close.
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ended_flag = ended.clone();

        start(
            PumpSpec {
                session_id: "s-1".into(),
                handle: handle.clone(),
                events: rx,
                buffered: vec![ThreadEvent::UserMessage { text: "hi".into(), images: vec![] }],
                seed: ChatBlob::new("s-1".into()),
                settings: settings.clone(),
                registry: registry.clone(),
                index: index.clone(),
                on_end: Some(Box::new(move || {
                    ended_flag.store(true, std::sync::atomic::Ordering::SeqCst)
                })),
            },
            pumps.clone(),
        );

        // A turn starts, then the agent dies mid-turn.
        tx.send(ThreadEvent::AssistantText("working…".into())).unwrap();
        assert!(
            wait_until(5_000, || pumps.active_turns() == 1),
            "the pump tracks the in-flight turn"
        );
        drop(tx);

        assert!(
            wait_until(5_000, || registry.get("s-1").is_none()),
            "a dead agent's session leaves the registry"
        );
        assert!(wait_until(5_000, || pumps.live_pumps() == 0), "the pump ends");
        assert!(
            wait_until(5_000, || ended.load(std::sync::atomic::Ordering::SeqCst)),
            "the end hook runs, so the agent's credential is revoked with it"
        );
        // The final state reached disk, interruption marked (turn no longer
        // active in the persisted fold), and the index still knows the row.
        let blob = blob::load(&settings, "s-1").expect("final state persisted");
        assert!(!blob.entries.is_empty(), "the fold reached disk");
        assert_eq!(
            index.title_of("s-1"),
            Some(Some("hi".into())),
            "the index lists the now-dormant session"
        );
    }
}
