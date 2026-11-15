//! The headless retry driver: what it arms, what it sends, and what it
//! refuses to do twice.

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{ChatImage, StubConnection, ThreadEntry, ThreadEvent};
use oximux_storage::SettingsRepo;

use super::*;

/// A limit the provider reports as a window, with its reset.
fn window_limit(resets_at_ms: i64) -> oximux_agent_core::thread::event::RateLimitInfo {
    oximux_agent_core::thread::event::RateLimitInfo {
        status: "rejected".into(),
        limit_type: Some("five_hour".into()),
        resets_at_ms: Some(resets_at_ms),
        utilization: None,
    }
}

/// Everything a driver needs, with the config it will read.
struct Fixture {
    retry: Arc<ServeRetry>,
    registry: Arc<SessionRegistry>,
    settings: SettingsRepo,
    store: RetryStore,
    /// Kept alive so the driver's receiver keeps seeing this config — and
    /// so a test can change a setting while a retry is already waiting.
    config: tokio::sync::watch::Sender<HostConfig>,
}

fn fixture(config: HostConfig) -> Fixture {
    let db = oximux_storage::open_memory().expect("open memory");
    let registry = Arc::new(SessionRegistry::new());
    let settings = SettingsRepo::new(db.clone());
    let store = RetryStore::new(db.conn());
    let (tx, rx) = tokio::sync::watch::channel(config);
    let retry = ServeRetry::new(
        registry.clone(),
        store.clone(),
        settings.clone(),
        rx,
        "host-1".to_string(),
    );
    Fixture { retry, registry, settings, store, config: tx }
}

/// A fold holding one user message whose turn failed on `class`.
fn failed_turn(limit: Option<oximux_agent_core::thread::event::RateLimitInfo>) -> ChatThread {
    let mut fold = ChatThread::default();
    fold.apply(&ThreadEvent::UserMessage { text: "ship it".into(), images: vec![] });
    fold.last_rate_limit = limit;
    fold.apply(&ThreadEvent::TurnEnded {
        result: Some("the window is closed".into()),
        usage: None,
        is_error: true,
        turn_diff: None,
    });
    fold
}

fn track_for(fold: &ChatThread) -> HealthTrack {
    let mut track = HealthTrack::default();
    track.note_sent(oximux_agents::session_health::now_ms(), None);
    track.failed_turn = fold.last_error.clone();
    track
}

fn now_ms() -> i64 {
    oximux_agents::session_health::now_ms()
}

/// Store one session's transcript where a fired retry will read it.
fn store_blob(settings: &SettingsRepo, session_id: &str, entries: Vec<ThreadEntry>) {
    let mut b = blob::ChatBlob::new(session_id.to_string());
    b.entries = entries;
    blob::save(settings, &b);
}

fn user(text: &str) -> ThreadEntry {
    ThreadEntry::User { text: text.into(), images: Vec::new(), checkpoint: None }
}

/// The text of a line the stub recorded. `content` is a plain string for a
/// message with no attachments and a block list when it has them.
fn sent_text(line: &serde_json::Value) -> String {
    let content = &line["message"]["content"];
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .and_then(|parts| parts.iter().find_map(|p| p["text"].as_str()))
        .unwrap_or_default()
        .to_string()
}

fn wait_until(deadline_ms: u64, mut probe: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
    while std::time::Instant::now() < deadline {
        if probe() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// A window the provider will reopen arms a retry, and the health says when.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_window_arms_a_retry_at_its_reset() {
    let f = fixture(HostConfig::default());
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));

    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));

    let armed = f.retry.armed("s-1").expect("a retry is armed");
    assert_eq!(armed.reason, oximux_agents::retry::WINDOW_REASON);
    assert!(armed.wake_at_ms > now_ms() + 59 * 60 * 1000, "it waits for the reset: {armed:?}");
    let live = f.store.live("s-1").expect("read").expect("a durable record");
    assert_eq!(live.attempts, 0, "nothing is spent until it fires");
}

/// A failure waiting cannot fix is not retried, and ends any retry the
/// earlier attempts had running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_waiting_cannot_fix_arms_nothing() {
    let f = fixture(HostConfig::default());
    let mut fold = failed_turn(None);
    // No rate limit and no retryable status: an ordinary error.
    fold.last_turn_failure = Some((Some(400), None));

    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));

    assert!(f.retry.armed("s-1").is_none(), "an ordinary failure is the session's own answer");
    assert!(f.store.live("s-1").expect("read").is_none());
}

/// The setting is the host's, and it is read at the decision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_switched_off_arm_nothing() {
    let f = fixture(HostConfig { retry_enabled: false, ..HostConfig::default() });
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));

    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));

    assert!(f.retry.armed("s-1").is_none());
    assert!(f.store.live("s-1").expect("read").is_none(), "and nothing is recorded either");
}

/// A reset further out than the host may hold a turn is reported, not held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reset_past_the_ceiling_is_not_held() {
    let f = fixture(HostConfig {
        retry_max_wait: oximux_remote_host::host_config::MaxAutomaticWait::SixHours,
        ..HostConfig::default()
    });
    let fold = failed_turn(Some(window_limit(now_ms() + 7 * 60 * 60 * 1000)));

    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));

    assert!(f.retry.armed("s-1").is_none(), "a seven-hour wait is past a six-hour ceiling");
}

/// The turn is sent again, verbatim, because the agent never started on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_sends_the_turn_again() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    // Armed in the past: `restore` is the path a host takes at boot, and it
    // fires a wake time that has just come round.
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", now_ms(), "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();

    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the held turn went out again");
    assert_eq!(sent_text(&conn.sent()[0]), "ship it", "verbatim: the agent never saw it");
    assert_eq!(f.store.attempts("s-1", "key-1").unwrap(), 1, "the attempt is spent at fire time");
    assert!(f.retry.armed("s-1").is_none(), "and the session is no longer holding for a limit");
}

/// A turn the agent had begun is asked to carry on, not started over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_that_had_begun_is_asked_to_continue() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(
        &f.settings,
        "s-1",
        vec![user("ship it"), ThreadEntry::Assistant(Default::default())],
    );
    f.store
        .arm("s-1", "key-1", "overload", "Provider overloaded", now_ms(), "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();

    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the retry sent something");
    assert_eq!(
        sent_text(&conn.sent()[0]),
        oximux_agents::retry::CONTINUE_PROMPT,
        "re-sending the prompt would make the agent do the work twice",
    );
}

/// A retry whose wake time passed while nothing was running is dropped.
///
/// The half that keeps this from being a surprise: a turn the user last saw
/// hours ago must not re-send itself because the host happened to restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_time_long_past_is_dropped_rather_than_fired() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    let long_ago = now_ms() - RESTORE_GRACE_MS - 60_000;
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", long_ago, "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();

    assert!(f.retry.armed("s-1").is_none(), "nothing is armed");
    assert!(f.store.live("s-1").expect("read").is_none(), "and the record is closed");
    assert!(
        !wait_until(300, || !conn.sent().is_empty()),
        "nothing was sent on the user's behalf",
    );
}

/// Switching retries off while the host was down is obeyed at the restore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_is_not_carried_over_once_retries_are_off() {
    let f = fixture(HostConfig { retry_enabled: false, ..HostConfig::default() });
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", now_ms() + 60_000, "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();

    assert!(f.retry.armed("s-1").is_none());
    assert!(f.store.live("s-1").expect("read").is_none());
}

/// A retry that fires against a session it cannot reach stops there.
///
/// "Reopen once, then mark Failed — never loop": with no catalog and no live
/// session there is nothing to send to, and the record is closed rather than
/// left live for a second attempt at the same impossible thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_that_cannot_reach_its_session_stops() {
    let f = fixture(HostConfig::default());
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", now_ms(), "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();

    assert!(
        wait_until(5_000, || f.store.live("s-1").expect("read").is_none()),
        "the retry ended instead of waiting for a session that is not there",
    );
    assert!(f.retry.armed("s-1").is_none());
}

/// A line this driver did not write supersedes the retry; its own does not.
///
/// The one that matters is the second half. A resend is an ordinary send by
/// the time the pump sees it, and a driver that let it supersede its own
/// retry would lose the turn key and the attempt count with it — so the next
/// failure would start again from zero, and the four attempts would never
/// run out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retrys_own_line_does_not_supersede_it() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", now_ms(), "host-0", chrono::Local::now())
        .expect("arm");

    f.retry.restore();
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the retry sent the turn again");

    // The pump's report of that very line, by the id it was written under.
    let line = conn.sent()[0]["uuid"].as_str().and_then(|id| Uuid::parse_str(id).ok());
    let line = line.expect("the retry tags the line it writes");
    assert_eq!(f.retry.on_line_written("s-1", line), None, "nothing of its own to withdraw");
    assert!(
        f.store.live("s-1").expect("read").is_some(),
        "its own line leaves the record live, so the attempt count keeps counting",
    );

    // Anyone else's line ends it. Nothing is handed back to withdraw: the
    // reservation went with the retry's own line when it was reported.
    assert_eq!(f.retry.on_line_written("s-1", Uuid::new_v4()), None);
    assert!(
        f.store.live("s-1").expect("read").is_none(),
        "a person's message takes over from the retry of an earlier turn",
    );
}

/// The attempt count carries across the resend, so four attempts are four.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_attempts_keep_counting_across_a_resend() {
    let f = fixture(HostConfig::default());
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    let track = track_for(&fold);

    f.retry.on_turn_failed("s-1", &fold, &track);
    let first = f.store.live("s-1").expect("read").expect("armed");
    // The turn fires and fails again, with a transcript the resend has grown.
    f.store.fired("s-1", &first.turn_key).expect("fired");
    let mut later = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    later.apply(&ThreadEvent::UserMessage { text: "Please continue.".into(), images: vec![] });
    later.apply(&ThreadEvent::TurnEnded {
        result: Some("the window is closed".into()),
        usage: None,
        is_error: true,
        turn_diff: None,
    });

    f.retry.on_turn_failed("s-1", &later, &track);

    let second = f.store.live("s-1").expect("read").expect("armed again");
    assert_eq!(second.turn_key, first.turn_key, "the same turn, so the same key");
    assert_eq!(second.attempts, 1, "and the attempt the resend spent still counts");
}

/// A turn that ends well ends the retry that got it there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_that_succeeds_ends_its_retry() {
    let f = fixture(HostConfig::default());
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));
    assert!(f.retry.armed("s-1").is_some(), "armed first");

    f.retry.on_turn_succeeded("s-1");

    assert!(f.retry.armed("s-1").is_none());
    assert!(f.store.live("s-1").expect("read").is_none());
}

/// What a retry re-sends, as a rule rather than through a timer.
#[test]
fn what_a_retry_sends_depends_on_whether_the_turn_had_begun() {
    assert_eq!(resend(&[user("ship it")]).map(|p| p.0), Some("ship it".to_string()));
    assert_eq!(
        resend(&[user("ship it"), ThreadEntry::Assistant(Default::default())]).map(|p| p.0),
        Some(oximux_agents::retry::CONTINUE_PROMPT.to_string()),
    );
    assert_eq!(resend(&[]), None, "a transcript with no prompt has nothing to re-send");
    let with_image = ThreadEntry::User {
        text: "look".into(),
        images: vec![ChatImage { media_type: "image/png".into(), data: "x".into() }],
        checkpoint: None,
    };
    assert_eq!(resend(&[with_image]).map(|p| p.1.len()), Some(1), "its attachments go with it");
}

/// A retry switched off while it waits does not send. The wait can be hours
/// long, and the setting means what it says at the moment of the send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_switched_off_while_it_waits_sends_nothing() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));
    let generation = f.retry.armed_generation("s-1").expect("it is waiting out the window");

    f.config.send(HostConfig { retry_enabled: false, ..HostConfig::default() }).expect("send");
    // That timer's own wake, an hour early: the question is what it does when
    // it gets there, not how long it sleeps.
    f.retry.clone().fire("s-1", "key-1", generation).await;

    assert!(conn.sent().is_empty(), "nothing was sent for a retry the user switched off");
    assert!(f.retry.armed("s-1").is_none(), "the wait is over");
    assert!(f.store.live("s-1").expect("read").is_none(), "the record is resolved, not left live");
    assert_eq!(f.store.attempts("s-1", "key-1").unwrap(), 0, "and it cost no attempt");
}

/// A Stop is the clearest takeover there is: the person said no to the very
/// turn the retry is holding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_ends_the_retry_it_was_waiting_on() {
    let f = fixture(HostConfig::default());
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));
    assert!(f.retry.armed("s-1").is_some(), "there is a retry for the stop to end");

    let withdraw = f.retry.on_stopped("s-1");

    assert!(withdraw.is_none(), "nothing of this driver's was queued");
    assert!(f.retry.armed("s-1").is_none(), "the stop ended it");
    assert!(f.store.live("s-1").expect("read").is_none(), "durably, so a restart does not revive it");
}

/// A send that supersedes a retry hands this driver's own resend back to be
/// taken out of the queue: it is behind the message that took over, and
/// written later it would re-send the superseded turn on top of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resend_a_later_send_superseded_is_handed_back() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    f.store
        .arm("s-1", "key-1", "window", "Usage limit reached", now_ms(), "host-0", chrono::Local::now())
        .expect("arm");
    f.retry.restore();
    assert!(wait_until(5_000, || !conn.sent().is_empty()), "the retry sent its line");

    let someone_else = Uuid::new_v4();
    let withdraw = f.retry.on_line_written("s-1", someone_else);

    let withdraw = withdraw.expect("the resend is handed back");
    assert_ne!(withdraw, someone_else, "what comes back is this driver's line, not theirs");
    assert!(f.store.live("s-1").expect("read").is_none(), "and the retry is over");
}

/// A timer a newer arm replaced does nothing when it wakes. It cannot be
/// aborted once it is running, so it has to answer "am I still the live
/// retry?" itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timer_a_newer_arm_replaced_sends_nothing() {
    let f = fixture(HostConfig::default());
    let conn = Arc::new(StubConnection::default());
    f.registry.register("s-1".into(), conn.clone());
    store_blob(&f.settings, "s-1", vec![user("ship it")]);
    let fold = failed_turn(Some(window_limit(now_ms() + 60 * 60 * 1000)));
    f.retry.on_turn_failed("s-1", &fold, &track_for(&fold));
    let armed = f.retry.armed("s-1").expect("a retry is armed");

    // A generation this driver has never handed out: the same position an
    // older timer is in once a newer arm has replaced its entry.
    f.retry.clone().fire("s-1", "key-1", u64::MAX).await;

    assert!(conn.sent().is_empty(), "a superseded timer sends nothing");
    assert_eq!(f.retry.armed("s-1"), Some(armed), "and leaves the live retry alone");
    assert_eq!(f.store.attempts("s-1", "key-1").unwrap(), 0, "spending no attempt of its own");
}
