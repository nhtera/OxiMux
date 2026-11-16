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
use oximux_agents::health::Process;
use oximux_agents::health_track::{HealthExtras, HealthTrack, inputs_from};
use oximux_agents::send_gate::{Holds, SendGate, SendSource};
use oximux_agents::thread::{ChatThread, TaskStatus};
use oximux_storage::SettingsRepo;

use super::blob::{self, ChatBlob};
use super::catalog::SessionIndex;
use super::retry::ServeRetry;

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
    /// The host's retry driver. The pump sees the failures and the lines; the
    /// driver owns the timer, because a turn that failed is often the one
    /// that killed the agent, and a timer held here would die with it.
    pub retry: Arc<ServeRetry>,
}

/// Start one session's pump. Returns immediately; the pump runs until the
/// agent's stream closes or serve finalizes.
/// Start a session's pump.
///
/// The returned receiver fires once the pump has published the session's
/// state for the first time. A caller that looks before that reads a default
/// — `recovery_note_owed: false` among them, which is exactly the answer that
/// makes a child's resume decide no recovery turn is owed and give up on it.
/// Whoever's next step depends on what this session *is* waits for the ack
/// rather than for a duration.
#[must_use = "a caller that reads this session's state before the first publish reads defaults"]
pub fn start(spec: PumpSpec, pumps: Arc<PumpSet>) -> tokio::sync::oneshot::Receiver<()> {
    let (published_tx, published_rx) = tokio::sync::oneshot::channel();
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
        retry,
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
    // Host-synthesized events (an operator-edited approval) reach the fold the
    // same way: the registry's ingest covers remote subscribers, this covers
    // the transcript this pump owns. Folded through `apply` with ingest=false —
    // re-ingesting would give the event a second seq.
    let (event_sink_tx, mut synth_rx) = futures::channel::mpsc::unbounded::<ThreadEvent>();
    handle.set_remote_event_sink(event_sink_tx);

    tokio::spawn(async move {
        let mut fold = ChatThread::rehydrated(
            Some(session_id.clone()),
            seed.model.clone(),
            seed.entries.clone(),
            seed.slash_commands.clone(),
        );
        fold.session_meta = seed.session_meta.clone();
        // A session with history behind it was reopened, not started: its
        // backend replays nothing, so until a frame arrives nothing has
        // confirmed that the conversation on disk is the one this process is
        // now holding. That is `RestoredUnconfirmed`, and it is a truer first
        // answer than `Idle`. The first frame clears it (`note_event`).
        //
        // `prior_host_gone` stays false: it is a claim that the previous
        // host's processes are provably gone, which only the won host lock and
        // its record can say, and the pump has neither. Claiming it would turn
        // a reopened session that had been mid-turn into `Interrupted` on no
        // evidence. The proof half is F14's.
        let reopened = !seed.entries.is_empty();
        // The blob's own answer is preferred over the fold's: `mid_turn` was
        // written by the host that was actually interrupted, while the fold
        // only shows a turn left marked running on disk. They agree in the
        // ordinary case; where they differ, the writer knew.
        let was_mid_turn = seed.mid_turn || mid_turn(&fold);
        let mut blob = seed;
        // What the fold does not keep: the last sign of life, an unanswered
        // send, how the process ended. The same tracker the desktop folds its
        // events into (`oximux_agents::health_track`).
        let mut track = if reopened {
            HealthTrack::restored(was_mid_turn, /* prior_host_gone */ false)
        } else {
            HealthTrack::default()
        };
        // A session that died failing comes back failing. The fold keeps no
        // durable trace of it — `last_error` is transient and an errored
        // `TurnEnded` leaves no entry — so the failure is restored from the
        // blob into both the tracker (which decides the health and holds the
        // queue) and the fold (whose text the health reports). Restored into
        // only one of the two, the session would read failed with no reason,
        // or carry a reason nothing reads.
        //
        // The next send clears both: `note_sent` drops the tracker's, and a
        // user message clears the fold's.
        if let Some(failure) = blob.last_failure.clone() {
            fold.last_error = Some(failure.clone());
            track.failed_turn = Some(failure);
        }
        // What a restart owes the agent: a note saying its turn was cut short,
        // once, on the next message. An earlier restart's note that nothing
        // has sent yet is kept rather than replaced — the agent needs to hear
        // about the restart that interrupted it, not the latest one.
        let note = Arc::new(Mutex::new(blob.recovery_note.clone().or_else(|| {
            let at = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
            was_mid_turn.then(|| oximux_agents::recovery_note::build(&at, &blob.stopped_tasks))
        })));
        let mut persist = Persist::new(settings, index, &fold, note.clone());

        // The send gate. Attaching it **takes over the send path**: from here
        // `SessionHandle::send_prompt_tagged` queues instead of writing, for
        // every sender — a phone, the CLI, another agent's `send_agent`, and
        // the outbox drainer's child events. So it is attached only together
        // with the loop below that flushes it, and detached when that loop
        // ends; a gate nobody drains is a session that silently stops
        // sending.
        let gate = Arc::new(Mutex::new(SendGate::new()));
        let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        gate.lock().unwrap().set_waker(move || {
            let _ = wake_tx.send(());
        });
        handle.set_gate(Some(gate.clone()));
        // The queue this session had when the last host stopped, back in the
        // gate and **held**: a restart is the one moment nobody has reviewed
        // what was waiting, so these sit visible until a person releases
        // them rather than draining into the agent on the way up. Before the
        // first publish, so the counts a client reads include them.
        for q in std::mem::take(&mut blob.queued) {
            let uuid = q.send_key.as_deref().and_then(|k| uuid::Uuid::parse_str(k).ok());
            gate.lock().unwrap().push_restored(q.text, SendSource::from_wire(&q.source), uuid);
        }
        // Everything the helpers below share, built once: the session they
        // all act on rather than five of its parts.
        let pump = Pump {
            session_id: &session_id,
            handle: &handle,
            gate: &gate,
            turn_tx: &turn_tx,
            retry: &retry,
            note: &note,
        };
        // A resumed session publishes its history immediately — `--resume`
        // replays nothing, so without this the transcript RPCs would answer
        // empty until the first new turn.
        persist.save_now(&handle, &mut blob, &fold, &track);

        for event in buffered {
            apply(&pump, &mut fold, &mut track, &event, /* ingest */ true);
        }
        persist.maybe_save(&handle, &mut blob, &fold, &track);
        // Say something now, even with nothing to replay — especially then.
        // A reopened backend replays nothing and can stay silent until the
        // next prompt, and `health_push` sends a client no frame at all until
        // a session has published once: without this, every device would read
        // a session someone just reopened as a default. The catalog's own
        // comment beside `buffered: Vec::new()` already promised it ("the
        // pump publishes the persisted fold immediately").
        publish_state(&pump, &fold, &mut track, fold.turn_active, Process::Live);
        // This session now answers for itself; whoever started it may look.
        let _ = published_tx.send(());

        let mut retry_changed = retry.subscribe();
        let mut save_tick = tokio::time::interval(SAVE_INTERVAL);
        save_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // A relay arm whose sender is gone returns `Ready(None)` on every poll —
        // the select then spins hot (the 900%-CPU class a `biased` reorder once
        // hit). Serve never rebinds a sink, so these only flip on teardown, but
        // the guard costs nothing and removes the trap.
        let (mut prompt_open, mut synth_open) = (true, true);
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
                    fold_queued_sends(&pump, &mut prompt_rx, &mut fold, &mut track);
                    let Some(event) = event else {
                        // The agent's stream closed: the child exited. An
                        // in-flight turn can never finish now — mark it so
                        // rather than leaving "running" on disk forever.
                        if fold.turn_active {
                            fold.interrupt();
                        }
                        // The code, waited for briefly. The stream ending and
                        // the process being reaped are different moments, so a
                        // read at the first sign of death usually has no code
                        // yet — and reporting `ConnectionClosed` for a provider
                        // that was killed loses the one fact that says *how* it
                        // died. The desktop polls for the same reason
                        // (`health_link::record_exit`); this host reported
                        // `None` unconditionally, which is the kind of
                        // difference this phase calls a bug.
                        let code = exit_code_soon(&handle).await;
                        let proof = track.note_exit(/* stopped */ false, code);
                        // Recorded as this session's last failure, because the
                        // frame below is the *only* live word on the subject:
                        // it reaches whoever is subscribed at this instant, and
                        // then the registry entry goes. A host restart used to
                        // leave no trace at all — the session came back reading
                        // live on its next message, with nothing to say its
                        // provider had died. This is the same channel a failed
                        // turn already persists through, and the next send
                        // clears it the same way.
                        let reason = oximux_agents::health::exited_reason(proof);
                        fold.last_error = Some(reason.clone());
                        track.failed_turn = Some(reason);
                        // Published before the registry entry goes: a watcher
                        // reading an unregistered session reads nothing at all.
                        publish_state(&pump, &fold, &mut track, false, Process::Dormant);
                        persist.save_now(&handle, &mut blob, &fold, &track);
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
                    apply(&pump, &mut fold, &mut track, &event, true);
                    if settled {
                        persist.save_now(&handle, &mut blob, &fold, &track);
                    }
                }
                prompt = futures::StreamExt::next(&mut prompt_rx), if prompt_open => {
                    match prompt {
                        Some(prompt) => {
                            fold.push_user_message_with_images(prompt.text, prompt.images);
                            refresh_meta(&handle, &fold);
                            track.note_sent(oximux_agents::session_health::now_ms(), None);
                            publish_state(&pump, &fold, &mut track, true, Process::Live);
                            // The send is the thing that supersedes a stored
                            // failure, so the blob has to learn about it
                            // here. Left to the next backend frame or the
                            // save tick, a restart in between would restore
                            // a failure the user had already answered — and
                            // hold the queue behind it.
                            persist.maybe_save(&handle, &mut blob, &fold, &track);
                        }
                        None => prompt_open = false,
                    }
                }
                synth = futures::StreamExt::next(&mut synth_rx), if synth_open => {
                    match synth {
                        Some(event) => {
                            // For the reason the backend arm drains: `apply`
                            // publishes the fold's state, and a prompt still
                            // sitting in the channel means that state says
                            // idle while a turn is running.
                            fold_queued_sends(&pump, &mut prompt_rx, &mut fold, &mut track);
                            apply(&pump, &mut fold, &mut track, &event, /* ingest */ false);
                            persist.maybe_save(&handle, &mut blob, &fold, &track);
                        }
                        None => synth_open = false,
                    }
                }
                _ = wake_rx.recv() => {
                    // Something was queued — a child's events, a phone's
                    // message, another agent's send. Nothing to do here: the
                    // wake exists to break this select, and the flush below
                    // runs after every pass.
                }
                _ = retry_changed.changed() => {
                    // A retry was armed or is over, which this pump's holds
                    // and health both report. Republished here because
                    // nothing else would: the gate's waker fires on a push
                    // and only while the gate is open, so a retry firing —
                    // which lifts its own hold and queues a line in the same
                    // breath — pushes into a gate still holding for it and
                    // wakes nobody at all.
                    //
                    // Drained first, for the reason the synth arm drains: a
                    // resend the flush has already written is a prompt still
                    // sitting in the channel, so the fold would report idle
                    // and this publish would clear the gate's hold on the
                    // very turn that resend just opened.
                    fold_queued_sends(&pump, &mut prompt_rx, &mut fold, &mut track);
                    publish_state(&pump, &fold, &mut track, fold.turn_active, Process::Live);
                    // Saved here for the same reason, one step on: a prompt
                    // this arm folded is one the prompt arm will never see,
                    // and the save that clears the stored failure is that
                    // arm's. Left out, a restart would restore a failure the
                    // send had already answered.
                    persist.maybe_save(&handle, &mut blob, &fold, &track);
                }
                _ = save_tick.tick() => {
                    // Republished, not only saved. A message pushed into a
                    // held gate wakes nobody — the gate's waker fires when
                    // something *can* go, which is a contract its own tests
                    // name — so without this the `queued` count a client
                    // reads stays at its last published value for as long as
                    // the hold lasts. This bounds that at one tick.
                    fold_queued_sends(&pump, &mut prompt_rx, &mut fold, &mut track);
                    publish_state(&pump, &fold, &mut track, fold.turn_active, Process::Live);
                    persist.maybe_save(&handle, &mut blob, &fold, &track);
                }
                _ = finalize.changed() => {
                    if !*finalize.borrow() {
                        continue;
                    }
                    // Folded first, as every other arm that reads the fold
                    // does: a line `flush` has just written is a prompt still
                    // in the relay, so the fold would say idle. This arm then
                    // records `mid_turn: false`, skips the interrupt, and
                    // saves a queue that is missing the line — and the next
                    // host owes no note for the turn it cut short.
                    fold_queued_sends(&pump, &mut prompt_rx, &mut fold, &mut track);
                    // Recorded before the interrupt below, which is what makes
                    // the fold stop saying a turn was running: this host is
                    // stopping with one in flight, so the next one owes the
                    // agent a note about it.
                    //
                    // Only on this path. The agent's stream closing is the
                    // *agent* dying, which the session already reports with
                    // its own reason — a note saying OxiMux restarted would be
                    // telling it something that did not happen.
                    persist.note_stopping_mid_turn(mid_turn(&fold));
                    // Drain: an unfinished turn is reported as interrupted,
                    // never silently truncated.
                    if fold.turn_active {
                        fold.interrupt();
                    }
                    // The host is draining, which is not the agent dying: no
                    // exit is claimed, only that nothing is running.
                    publish_state(&pump, &fold, &mut track, false, Process::Dormant);
                    persist.save_now(&handle, &mut blob, &fold, &track);
                    break;
                }
            }
            // After the arm, not inside one: a queued send goes out when the
            // gate opens, and what opens it is usually a turn *ending* — a
            // hold lifting, which the gate's waker never reports. Both halves
            // are needed, and the push half alone is the bug where a child's
            // events sit in the gate until something unrelated arrives.
            flush(&pump);
        }
        // Writes go direct again from here: a gate with no one draining it
        // would swallow every later send in silence.
        handle.set_gate(None);
        pumps_for_end.remove(&session_id);
        if let Some(on_end) = on_end {
            on_end();
        }
    });
    published_rx
}

/// What every one of a pump's helpers needs and none of them changes.
///
/// Grouped rather than threaded: these five travel together through the
/// publish, the flush, the fold and the Stop, and passing them one by one had
/// already put `apply` over the argument limit before the retry driver
/// arrived. A helper that takes this takes the session, not five of its
/// parts.
struct Pump<'a> {
    session_id: &'a str,
    handle: &'a SessionHandle,
    gate: &'a Arc<Mutex<SendGate>>,
    /// The pump's own "is a turn running" channel, which shutdown reads.
    turn_tx: &'a tokio::sync::watch::Sender<bool>,
    retry: &'a Arc<ServeRetry>,
    /// The note a restart left owed, until a message carries it.
    ///
    /// The one piece of this struct that changes, and behind a lock rather
    /// than threaded through every helper: what owes it is the restart, what
    /// spends it is the next write, and what reports it is the publish —
    /// three places in one task. A `RefCell` would read more honestly and
    /// costs the spawned future its `Send`, so a mutex it is; no guard here
    /// outlives the statement that takes it.
    note: &'a Arc<Mutex<Option<String>>>,
}

/// Fold the prompts this host has written that the fold has not seen yet.
///
/// A send and a backend frame arrive on separate channels, so `select!` gives
/// no order between them; `send_prompt` holds the session's prompt order
/// across the backend write and the bridge takes it before handing an event
/// over, so a queued prompt is always the earlier of the two, and draining
/// here is what turns that into fold order.
///
/// Every arm that publishes the fold's state drains first. Published with a
/// prompt still in the channel, the fold reads idle while a turn is running —
/// and the gate, told the turn ended, lets the next queued line out beside it.
fn fold_queued_sends(
    pump: &Pump<'_>,
    prompt_rx: &mut futures::channel::mpsc::UnboundedReceiver<RemotePrompt>,
    fold: &mut ChatThread,
    track: &mut HealthTrack,
) {
    while let Ok(prompt) = prompt_rx.try_recv() {
        fold.push_user_message_with_images(prompt.text, prompt.images);
        refresh_meta(pump.handle, fold);
        // A send, not a frame: the turn is this host's own doing, and the fold
        // does not know it started.
        track.note_sent(oximux_agents::session_health::now_ms(), None);
        publish_state(pump, fold, track, true, Process::Live);
    }
}

/// Write whatever the gate will let go now.
///
/// Called on the gate's waker (something was pushed) **and** after every
/// hold change, because the waker only fires on a push: a turn ending is what
/// opens the gate, and nothing inside it notices that.
///
/// The lock is dropped before the write. It is the lock a `send_prompt` from
/// any thread takes, and the write is a pipe write to a child process; the
/// drainer also takes it on every pass.
fn flush(pump: &Pump<'_>) {
    let (session_id, handle, gate) = (pump.session_id, pump.handle, pump.gate);
    if gate.lock().unwrap().take_stop_request() {
        withdraw(gate, pump.retry.on_stopped(session_id));
        stop_the_turn(pump);
        return;
    }
    loop {
        let Some(item) = gate.lock().unwrap().take_next_if_open() else { return };
        // A recovery turn exists to carry the restart's note. Once the note
        // has gone — with a message written meanwhile — or was never owed,
        // sending it would be a second, unasked turn on the person's bill.
        // Dropped, and reported not-sent so whoever is waiting on its key
        // stops waiting.
        if item.source == SendSource::Recovery && pump.note.lock().unwrap().is_none() {
            tracing::info!(
                session = session_id,
                "a recovery turn this session no longer owes a note for was dropped",
            );
            // `take_next_if_open` opened a turn for this item; nothing was
            // written, so give the gate back the state the item found.
            let mut gate = gate.lock().unwrap();
            gate.report_sent(item.uuid, false);
            gate.on_turn_end();
            continue;
        }
        // The restart's note goes ahead of the first message to leave after
        // it, as one line — never a turn of its own, which would be a turn
        // the person did not ask for and is billed for. Taken before the
        // write and put back if it fails: a note that went nowhere is still
        // owed.
        let note = pump.note.lock().unwrap().take();
        let with_note;
        let text = match &note {
            Some(note) => {
                with_note = oximux_agents::recovery_note::attach(note, &item.text);
                with_note.as_str()
            }
            None => item.text.as_str(),
        };
        // The item's own source, not a guess: it decides whether the text is
        // the host's batch of child events or a paste that merely looks like
        // one. A batch escaped on its way out would reach its parent in the
        // very form that marks a forged event as ordinary text.
        let sent = handle.write_prompt_now(text, &item.images, Some(item.uuid), item.source);
        if let Err(err) = &sent {
            tracing::warn!(%err, session = session_id, "a queued send could not be written");
            // Nothing reached the agent, so the note is still owed.
            *pump.note.lock().unwrap() = note;
        } else if note.is_some() {
            // Said in the transcript, so a person reading it later knows the
            // agent was told about the restart and when.
            handle.post_notice(oximux_agents::recovery_note::SENT_NOTICE);
        }
        // The drainer's hook settles (or returns) the outbox rows this item
        // carried, by its uuid.
        if sent.is_ok() {
            // Whoever sent this has taken over from an armed retry of an
            // earlier turn — unless it *is* that retry's own line, which the
            // driver recognises by this id.
            withdraw(gate, pump.retry.on_line_written(session_id, item.uuid));
        }
        let mut gate = gate.lock().unwrap();
        gate.report_sent(item.uuid, sent.is_ok());
        // The take opened a turn for this item. A write that failed started
        // none, so the gate goes back to what the item found — it was open,
        // or the take would have handed nothing over. Left set, it closes the
        // gate to every later item until the next backend event publishes the
        // holds again, which a quiet backend never does.
        if sent.is_err() {
            gate.on_turn_end();
        }
        // An agent that cannot say what it started on (every backend but
        // Claude) drops the id this line went out under, so no receipt will
        // ever name it. Noted here, the next frame stands in for the one the
        // agent does not send; without that the drainer gives up after its
        // 30 s window and marks a line it did deliver `unconfirmed`.
        if sent.is_ok() && !handle.reports_receipts() {
            gate.note_unreported(item.uuid);
        }
        drop(gate);
        if sent.is_err() {
            return;
        }
    }
}

/// Interrupt the turn, for a Stop that came from somewhere else.
///
/// A Stop from a phone or the CLI reaches `SessionHandle::cancel`, which hands
/// it to the gate rather than the backend whenever a gate is attached and
/// listening: only the gate's driver can take back the lines it wrote into
/// the turn. On the desktop that driver is the chat view; here it is this
/// pump, so the request is taken here and the backend asked directly. Without
/// this the gate held the request and nothing read it: a remote Stop answered
/// `Ok` and the turn ran on.
///
/// What it does **not** do is hold the queue afterwards. The desktop's Stop
/// parks a child's events until the person's next message releases them, and
/// nothing headless can release such a hold yet, so a Stop here interrupts
/// the turn and leaves the queue as it was.
/// Take a line the retry driver has given up on out of the queue.
///
/// A fired retry lifts its own hold and pushes its resend, so anything that
/// was waiting behind the hold is *ahead* of that resend. When one of those
/// goes out first it supersedes the retry — and the resend, still queued,
/// would then re-send the superseded turn on top of the message that took
/// over.
fn withdraw(gate: &Mutex<SendGate>, line: Option<uuid::Uuid>) {
    let Some(line) = line else { return };
    let mut gate = gate.lock().unwrap();
    let Some(id) = gate.queued().find(|q| q.uuid == line).map(|q| q.id) else { return };
    gate.remove(id);
}

fn stop_the_turn(pump: &Pump<'_>) {
    let (session_id, handle, gate) = (pump.session_id, pump.handle, pump.gate);
    // Serve never offers steering (`steer_supported` is false in every
    // publish), so no line is ever waiting to be taken back. One that was
    // would be lost rather than retried, so it is reported unsent — which
    // gives the outbox rows it carried back to the drainer.
    {
        let mut gate = gate.lock().unwrap();
        let unstarted = gate.take_unstarted();
        for (uuid, _) in unstarted {
            tracing::warn!(session = session_id, %uuid, "a line written into the stopped turn was dropped");
            gate.report_sent(uuid, false);
        }
    }
    // The gate's lock is not held across the backend call, as nothing here is.
    if let Err(err) = handle.cancel_backend() {
        tracing::warn!(%err, session = session_id, "a Stop could not interrupt the turn");
    }
}

/// Say what this session is doing: to the pump's own turn channel, and to the
/// health facts every remote client and the host core read — in one call, so
/// the two cannot disagree.
///
/// It used to be the channel and nothing else, so nothing outside this process
/// ever heard. A spawned child was set working once by the spawn and never
/// unset: it held its slot under the fan-out cap for the life of the host, and
/// nothing anywhere said why its parent could no longer spawn.
///
/// **The registry's `working` flag is not written here.** `publish_health`
/// derives it from these facts under the same lock as the health itself, and
/// a second write would be a second opinion — one that is wrong whenever an
/// unanswered send or an open continuation means working while no turn runs.
///
/// `held` and `queued` are read from the gate here, which is why this takes
/// the tracker by `&mut`: they are the gate's own counts, and a copy kept in
/// step from every push and take would be a second answer that can disagree
/// with the one being published.
///
/// What this host cannot answer yet is left at its default rather than
/// guessed: `interrupted`, owed by serve's `Cancel` (F7). `children_active` is
/// 0 because the host's own periodic look at the lineage overwrites it, which
/// is the only place that knows.
fn publish_state(
    pump: &Pump<'_>,
    fold: &ChatThread,
    track: &mut HealthTrack,
    working: bool,
    process: Process,
) {
    let (handle, gate) = (pump.handle, pump.gate);
    let _ = pump.turn_tx.send(working);
    // Read here, not kept in step from every place that pushes or takes: the
    // gate is the only thing that knows, and one read beside the publish
    // cannot drift from what is about to be published.
    (track.held, track.queued) = gate.lock().unwrap().counts();
    // The one read of the retry driver, so the health and the gate's hold
    // cannot disagree about whether this session is waiting out a limit.
    let armed = pump.retry.armed(pump.session_id);
    handle.publish_health(inputs_from(
        fold,
        track,
        HealthExtras {
            process_while_running: process,
            incarnation: handle.incarnation().to_string(),
            limited_until: armed.as_ref().map(|a| a.wake_at_ms),
            limit_reason: armed.as_ref().map(|a| a.reason.clone()),
            recovery_note_owed: pump.note.lock().unwrap().is_some(),
            ..HealthExtras::default()
        },
    ));
    // The gate holds on the same facts. Its waker fires when something is
    // pushed, never when a hold lifts, so whoever updates the holds is also
    // the one that has to look again — see `flush`.
    gate.lock().unwrap().set_holds(Holds {
        turn_active: working,
        // Serve has no Stop of its own yet (F7's `Cancel`), so `interrupted`
        // stays false rather than guessed, as in the health facts above.
        interrupted: false,
        // A turn held for a limit holds the queue with it: a line let out
        // beside it would start a second turn into the same closed window.
        retry_armed: armed.is_some(),
        disconnected: matches!(process, Process::Dormant),
        prompt_pending: fold.pending_permission().is_some() || fold.pending_question().is_some(),
        failed: track.holds_queue(),
        turn_started: false,
        // Queue-only: a steered line needs an agent that reports what it
        // started on, so a Stop can take it back. Spike S5.5 owns that.
        steer_supported: false,
    });
}

/// Whether a restart stopping now would cut something short.
///
/// One expression, used by the save that records it and by the restore that
/// reads it back: a session that saved `mid_turn` one way and judged it
/// another would owe the agent a note on one host and not on the other.
fn mid_turn(fold: &ChatThread) -> bool {
    fold.turn_active
        || fold.continuations_open > 0
        || fold.background_tasks.iter().any(|t| t.status == TaskStatus::Running)
}

/// What the background tasks running now are doing, for a note to name them.
fn stopped_tasks(fold: &ChatThread) -> Vec<String> {
    fold.background_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Running)
        .map(|t| t.description.clone())
        .collect()
}

/// The queue a restart must carry: what this host has accepted and not yet
/// written.
///
/// Two senders are left out, both because they already have a durable path
/// and would otherwise arrive twice — a child's event batch is an outbox
/// delivery, which the outbox hands back to the drainer at boot, and a
/// recovery turn is minted from the lineage by whoever resumes the child.
/// `SendSource::survives_a_restart` is that rule, stated once.
fn queue_to_persist(handle: &SessionHandle) -> Vec<blob::BlobQueuedSend> {
    let Some(gate) = handle.gate() else { return Vec::new() };
    gate.lock()
        .unwrap()
        .queued()
        .filter(|q| q.source.survives_a_restart())
        .map(|q| blob::BlobQueuedSend {
            text: q.text.clone(),
            source: q.source.wire_name().to_string(),
            send_key: Some(q.uuid.to_string()),
        })
        .collect()
}

/// The failure a restart must carry, exactly as the health reports it.
///
/// `inputs_from` is the one reading of a session's health, and it answers
/// `last_turn_failure` from the tracker's failure with the fold's fuller text
/// preferred — the fold folds in the stderr diagnostic that the raw result
/// lacks. Persisting that same string, rather than either half, is what makes
/// a restored session report the words it reported before the restart instead
/// of a shorter version of them.
///
/// `None` while a turn runs, for the same reason the health reads it that
/// way: a turn that started has superseded the last one's failure.
fn failure_to_persist(fold: &ChatThread, track: &HealthTrack) -> Option<String> {
    track
        .failed_turn
        .as_ref()
        .filter(|_| !fold.turn_active)
        .map(|raw| fold.last_error.clone().unwrap_or_else(|| raw.clone()))
}

/// How long to wait for a dead provider's exit code before reporting without
/// one.
///
/// Short, because the wait delays the only frame that says the process died
/// and a reader is entitled to hear about it promptly; long enough that the
/// usual case — a reap a few milliseconds behind the stream's end — is caught.
const EXIT_CODE_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

/// The provider's exit code once it is readable, or `None` within
/// [`EXIT_CODE_WITHIN`].
async fn exit_code_soon(handle: &SessionHandle) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + EXIT_CODE_WITHIN;
    loop {
        if let Some(code) = handle.exit_code() {
            return Some(code);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Fold + registry bookkeeping for one event.
fn apply(
    pump: &Pump<'_>,
    fold: &mut ChatThread,
    track: &mut HealthTrack,
    event: &ThreadEvent,
    ingest: bool,
) {
    let (handle, gate) = (pump.handle, pump.gate);
    if ingest {
        handle.ingest(event.clone());
        // Only the backend's own frames prove it started on the host's line.
        // A host-synthesized event (an operator-edited approval) is this
        // process talking to itself, and proves nothing about the agent.
        stand_in_for_a_missing_receipt(handle, gate, event);
    }
    fold.apply(event);
    track.note_event(event, oximux_agents::session_health::now_ms());
    // After the fold and the tracker, which is where the failure's class
    // comes from, and before the publish, so a retry armed by this very
    // event is already in the health frame it produces.
    if let ThreadEvent::TurnEnded { is_error, .. } = event {
        if *is_error {
            pump.retry.on_turn_failed(pump.session_id, fold, track);
        } else {
            pump.retry.on_turn_succeeded(pump.session_id);
        }
    }
    publish_state(pump, fold, track, fold.turn_active, Process::Live);
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

/// Say what the agent would have said about the host's last line, for an
/// agent that cannot say it.
///
/// Only Claude reports which tagged line it started on. Every other backend
/// drops the id, so a line the host wrote is never named by any receipt — and
/// the outbox drainer, which settles its rows from receipts alone, gives up
/// after 30 s and records a delivered update as `unconfirmed`. The agent's
/// next frame is the proof it started, and the turn ending is the proof it
/// finished; both are stood in for here under the line's own id, exactly as
/// the desktop's driver does, so a parent's updates read as what they were.
fn stand_in_for_a_missing_receipt(handle: &SessionHandle, gate: &Arc<Mutex<SendGate>>, event: &ThreadEvent) {
    if handle.reports_receipts() {
        return;
    }
    // The lock is dropped before the ingest: it reaches the drainer, which
    // takes this same gate on its pass.
    let stand_in = {
        let mut gate = gate.lock().unwrap();
        if matches!(event, ThreadEvent::TurnEnded { .. }) {
            gate.unreported_delivered().map(|u| ThreadEvent::UserMessagesDelivered { uuids: vec![u.to_string()] })
        } else {
            gate.unreported_accepted().map(|u| ThreadEvent::UserMessageAccepted { uuid: u.to_string() })
        }
    };
    if let Some(receipt) = stand_in {
        handle.ingest(receipt);
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
    /// The restart's note, shared with the pump: what is still owed at the
    /// moment of a save is what a restart after it has to owe again.
    note: Arc<Mutex<Option<String>>>,
    last_saved_revision: u64,
    /// The queue as it was last written, so a save can be triggered by the
    /// queue moving and not only by the transcript.
    last_saved_queue: Vec<blob::BlobQueuedSend>,
    /// This host stopped while a turn was running. Sticky: once it is
    /// stopping it stays stopping, and the save that follows the interrupt
    /// has to record what the fold no longer says.
    stopping_mid_turn: bool,
}

impl Persist {
    fn new(
        settings: SettingsRepo,
        index: Arc<SessionIndex>,
        fold: &ChatThread,
        note: Arc<Mutex<Option<String>>>,
    ) -> Self {
        // Start one behind so the initial save always runs.
        Self {
            settings,
            index,
            note,
            last_saved_revision: fold.revision().wrapping_sub(1),
            last_saved_queue: Vec::new(),
            stopping_mid_turn: false,
        }
    }

    /// The host is stopping, and a turn was running when it started to.
    fn note_stopping_mid_turn(&mut self, mid: bool) {
        self.stopping_mid_turn |= mid;
    }

    fn maybe_save(
        &mut self,
        handle: &SessionHandle,
        blob: &mut ChatBlob,
        fold: &ChatThread,
        track: &HealthTrack,
    ) {
        // The queue is checked as well as the revision: queueing a message
        // does not move the fold — the fold learns of a line only when it is
        // written — so a revision check alone would leave a queued send
        // unpersisted until something unrelated happened to the transcript.
        // The desktop has the same hole for its armed retry and plugs it the
        // same way, with its own dirty mark.
        if fold.revision() != self.last_saved_revision
            || queue_to_persist(handle) != self.last_saved_queue
        {
            self.save_now(handle, blob, fold, track);
        }
    }

    fn save_now(
        &mut self,
        handle: &SessionHandle,
        blob: &mut ChatBlob,
        fold: &ChatThread,
        track: &HealthTrack,
    ) {
        // Written on every save, cleared as well as set: a send clears the
        // tracker's failure, and a blob that only ever gained the field
        // would report a failure the session has since moved past.
        blob.last_failure = failure_to_persist(fold, track);
        blob.queued = queue_to_persist(handle);
        // What a restart would cut short if it happened now, and what it
        // would owe the agent. Written on every save, cleared as well as
        // set: a note that has gone out must not come back on the next
        // restart, and a session that is idle now did not end mid-turn.
        blob.mid_turn = self.stopping_mid_turn || mid_turn(fold);
        blob.stopped_tasks = stopped_tasks(fold);
        blob.recovery_note = self.note.lock().unwrap().clone();
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
        //
        // What was saved is remembered only once it *is* saved. Advanced
        // first, a write that failed would leave `maybe_save` believing the
        // queue on disk matches the one in the gate, and it would skip every
        // save until something else moved — so a message accepted just before
        // the failure would be missing after a restart. Left as they are, the
        // next tick tries again.
        if blob::save(&self.settings, blob) {
            self.last_saved_revision = fold.revision();
            self.last_saved_queue = blob.queued.clone();
        }
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
#[path = "pump_tests.rs"]
mod tests;
