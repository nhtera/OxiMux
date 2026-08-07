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
    //
    // The handoff runs under the session's prompt order, so a reply cannot enter
    // this channel while the prompt that caused it is still being recorded. The
    // wait is bounded by one `send_prompt` — a pipe write plus two records — and
    // nothing is dropped meanwhile: `events` is unbounded, so the reader thread
    // keeps draining the child's stdout regardless of what this thread is doing.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let bridge_handle = handle.clone();
    std::thread::Builder::new()
        .name(format!("pump-{session_id}"))
        .spawn(move || {
            while let Ok(event) = events.recv() {
                // `is_ok()` inside the closure, not a `Result` out of it: the send
                // error carries the whole event back, and a 200-byte `Err` is what
                // `clippy::result_large_err` exists to catch. Only whether it landed
                // matters — a closed channel means the pump is gone.
                let delivered = bridge_handle.with_prompt_order(|| event_tx.send(event).is_ok());
                if !delivered {
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
                    // Fold any prompt that is already queued before this event.
                    //
                    // The two arrive on separate channels, so `select!` alone gives
                    // no order between them, and a reply that overtakes its own
                    // prompt is persisted answer-then-question. `send_prompt` holds
                    // the session's prompt order across the backend write, and the
                    // bridge takes it before handing an event over, so a queued
                    // prompt is always the earlier of the two — draining here is
                    // what turns that guarantee into fold order.
                    //
                    // A drain here, and not a `biased` arm polled ahead of this
                    // one: such an arm goes on returning `Ready(None)` once the
                    // sink is gone, so the loop spins at full tilt and never
                    // reaches the channel-closed branch below — measured at ~900%
                    // CPU on a host with a dozen sessions. Draining cannot do that;
                    // it stops on empty and on terminated alike.
                    while let Ok(prompt) = prompt_rx.try_recv() {
                        fold.push_user_message_with_images(prompt.text, prompt.images);
                        refresh_meta(&handle, &fold);
                        let _ = turn_tx.send(true);
                    }
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
                        refresh_meta(&handle, &fold);
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
    //
    // `UserMessage` is in the list because it is what makes a title derivable:
    // these backends never send `TitleUpdated`, so without it `fold.title` stays
    // `None` for the whole life of a live session and `oximux ls` falls back to
    // printing the session's own UUID as its title — for every row, which is
    // precisely when a list stops being usable. A resumed session emits no
    // `SessionInit` either, so this is also the only trigger it has — but a
    // protocol-injected prompt never comes through here as an event at all (no
    // backend echoes the user's half), so the pump's prompt arms must call
    // `refresh_meta` themselves after pushing the user message into the fold.
    if matches!(
        event,
        ThreadEvent::SessionInit { .. }
            | ThreadEvent::TitleUpdated { .. }
            | ThreadEvent::ModeChanged { .. }
            | ThreadEvent::UserMessage { .. }
    ) {
        refresh_meta(handle, fold);
    }
}

/// Publish the fold's list-row metadata to the registry. One function for every
/// path that can make a title derivable — the event path (`apply`) and the two
/// protocol-prompt arms in the pump loop — because applying the derived-title
/// fallback on only some of them is exactly the bug that made `oximux ls` show
/// a live session as its own UUID whenever the backend announced before the
/// first prompt landed (and always, for a resumed session).
fn refresh_meta(handle: &SessionHandle, fold: &ChatThread) {
    handle.set_meta(SessionMeta {
        // Same fallback the persistence path already applied. Having it on
        // only one of the two is what made a host restart *improve* the
        // listing.
        title: fold.title.clone().or_else(|| super::blob::derived_title(&fold.entries)),
        model: fold.model.clone(),
        permission_mode: fold.permission_mode.clone(),
        cwd: fold.session_meta.cwd.clone().map(std::path::PathBuf::from),
    });
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

    /// A LIVE session lists by its prompt, not by its own UUID.
    ///
    /// The registry row is what `oximux ls` reads while a session is running,
    /// and it used to publish `fold.title` alone. These backends never send
    /// `TitleUpdated`, so that stayed `None` for the session's whole life and
    /// the wire fell back to the session id — every row reading
    /// `<project> · <uuid>`, with the uuid already in the id column. Measured
    /// against a real `claude`: three live sessions were three indistinguishable
    /// rows, and *restarting the host* fixed them, because only the persistence
    /// path applied the derived-title fallback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_session_lists_by_its_prompt_rather_than_its_own_id() {
        let registry = Arc::new(SessionRegistry::new());
        let handle = registry.register("s-2".into(), Arc::new(StubConnection::default()));
        let db = oximux_storage::open_memory().unwrap();
        let settings = SettingsRepo::new(db);
        let index = Arc::new(SessionIndex::default());
        let pumps = PumpSet::new();
        let (tx, rx) = std::sync::mpsc::channel();

        start(
            PumpSpec {
                session_id: "s-2".into(),
                handle: handle.clone(),
                events: rx,
                buffered: vec![ThreadEvent::UserMessage {
                    text: "Summarise what a.txt contains".into(),
                    images: vec![],
                }],
                seed: ChatBlob::new("s-2".into()),
                settings,
                registry: registry.clone(),
                index,
                on_end: None,
            },
            pumps.clone(),
        );

        // Still live — this is the registry row, not the dormant index.
        assert!(
            wait_until(5_000, || registry
                .get("s-2")
                .and_then(|h| h.meta_snapshot().title)
                .is_some_and(|t| t == "Summarise what a.txt contains")),
            "a live session's list row carries the derived title, got {:?}",
            registry.get("s-2").and_then(|h| h.meta_snapshot().title),
        );
        drop(tx);
    }

    /// The order the buffered-event test cannot see: the backend announces
    /// BEFORE the first prompt lands, and the prompt arrives over the protocol
    /// (`send_prompt` → relay sink), never as a `ThreadEvent` — no backend
    /// echoes the user's half. The `SessionInit` trigger then fires on an empty
    /// fold, and if the prompt arms skip the meta refresh the row shows the
    /// session's own UUID for the session's whole life. A resumed session hits
    /// the same hole unconditionally, having no `SessionInit` at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_prompt_injected_over_the_protocol_still_titles_the_live_row() {
        let registry = Arc::new(SessionRegistry::new());
        let handle = registry.register("s-3".into(), Arc::new(StubConnection::default()));
        let db = oximux_storage::open_memory().unwrap();
        let settings = SettingsRepo::new(db);
        let index = Arc::new(SessionIndex::default());
        let pumps = PumpSet::new();
        let (tx, rx) = std::sync::mpsc::channel();

        start(
            PumpSpec {
                session_id: "s-3".into(),
                handle: handle.clone(),
                events: rx,
                // The backend has already announced; the fold holds no user
                // entry when the SessionInit trigger fires.
                buffered: vec![ThreadEvent::SessionInit {
                    session_id: "s-3".into(),
                    model: "fake".into(),
                    permission_mode: "default".into(),
                    slash_commands: vec![],
                    meta: Default::default(),
                }],
                seed: ChatBlob::new("s-3".into()),
                settings,
                registry: registry.clone(),
                index,
                on_end: None,
            },
            pumps.clone(),
        );
        assert!(
            wait_until(5_000, || registry
                .get("s-3")
                .map(|h| h.meta_snapshot().model.as_deref() == Some("fake"))
                .unwrap_or(false)),
            "the pump applied the buffered SessionInit",
        );

        handle.send_prompt("Fix the flaky test", &[]).unwrap();

        assert!(
            wait_until(5_000, || registry
                .get("s-3")
                .and_then(|h| h.meta_snapshot().title)
                .is_some_and(|t| t == "Fix the flaky test")),
            "a protocol-injected prompt titles the live row, got {:?}",
            registry.get("s-3").and_then(|h| h.meta_snapshot().title),
        );
        drop(tx);
    }
}
