//! Serve's attention subscriber, driven through the real host core.
//!
//! These go through `build_host_core` and a registered session rather than
//! calling `line` on a hand-built change, because the thing most likely to
//! break is not the wording: it is whether a headless host evaluates
//! attention at all. The core's followers run only while something
//! subscribes, so a change that drops this subscriber would leave every
//! formatting test passing and the stream silent.

use std::sync::{Arc, Mutex};

use oximux_agents::attention::{AttentionEdge, AttentionKind};
use oximux_agents::health::{HealthInputs, Process, PromptKind};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::attention::AttentionChanged;

use super::{follow, line};

/// What the log wrote, as an operator would have read it.
#[derive(Clone, Default)]
struct Written(Arc<Mutex<Vec<String>>>);

impl Written {
    fn sink(&self) -> impl FnMut(String) + Send + 'static {
        let lines = self.0.clone();
        move |line| lines.lock().unwrap().push(line)
    }

    /// The first line matching `needle`, waited for.
    ///
    /// Waited for rather than read once: every step between the health
    /// publication and the line is another task — the core's follower, its
    /// broadcast, this subscriber — so the line is there a moment later, not
    /// in the same poll.
    async fn waiting_for(&self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(found) =
                self.0.lock().unwrap().iter().find(|l| l.contains(needle)).cloned()
            {
                return found;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no log line containing {needle:?}; saw {:?}",
                self.0.lock().unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

/// A session waiting on a person.
fn needs_permission() -> HealthInputs {
    HealthInputs {
        process: Process::Live,
        prompt: Some(PromptKind::Permission),
        prompt_request: Some("req-1".into()),
        ..HealthInputs::default()
    }
}

/// A headless host evaluates attention and writes it down.
///
/// The whole chain, because every link is somewhere else: serve's core builds
/// the stream, subscribing starts the followers, a session's health wakes one,
/// and the core publishes a change this module turns into a line. Serve had
/// all of that except the subscriber, and without one the followers never ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_needs_a_person_reaches_the_host_log() {
    let db = oximux_storage::open_memory().unwrap();
    let settings = oximux_storage::SettingsRepo::new(db.clone());
    let registry = Arc::new(SessionRegistry::new());
    let core = super::super::host::build_host_core(
        tokio::runtime::Handle::current(),
        &db,
        registry.clone(),
        &settings,
    );
    let written = Written::default();
    tokio::spawn(follow(core.subscribe_attention(), written.sink()));

    let handle = registry.register("s-1".into(), Arc::new(StubConnection::default()));
    handle.publish_health(needs_permission());

    let raised = written.waiting_for("chat:s-1").await;
    assert!(raised.contains("Needs approval"), "saw {raised:?}");
    assert!(raised.contains("req-1"), "the line names the request being waited on: {raised:?}");

    // And the retirement. This is the half P7 sends a dismissal from, so a
    // host that logged only raises would leave a notification outliving the
    // thing it was about.
    handle.publish_health(HealthInputs { prompt: None, ..needs_permission() });
    let retired = written.waiting_for("was needs approval").await;
    assert!(retired.contains("req-1"), "and names the edge that left: {retired:?}");
}

/// A log that fell behind says so rather than going quiet.
///
/// Driven through its own channel: a capacity of one with three sends before
/// the first read is a dropped change by construction, where filling the
/// core's 256-deep ring would be a race dressed up as a test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_that_fell_behind_says_how_much_it_missed() {
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    for id in ["a", "b", "c"] {
        tx.send(AttentionChanged {
            target: format!("chat:{id}"),
            attention: None,
            edge: None,
            retired: Some(AttentionEdge {
                target: format!("chat:{id}"),
                kind: AttentionKind::Failed,
                id: id.into(),
            }),
        })
        .unwrap();
    }
    let written = Written::default();
    tokio::spawn(follow(rx, written.sink()));

    let lagged = written.waiting_for("were dropped").await;
    assert!(lagged.starts_with("2 attention changes"), "saw {lagged:?}");
    // And it carries on from where the ring now starts rather than stopping.
    written.waiting_for("chat:c").await;
}

/// The three shapes a change can take, each answering a different question.
#[test]
fn a_line_says_what_is_current_what_left_or_both() {
    let edge = |id: &str, kind| AttentionEdge { target: "chat:s".into(), kind, id: id.into() };
    let change = |attention, edge, retired| AttentionChanged {
        target: "chat:s".into(),
        attention,
        edge,
        retired,
    };
    let needs_you = oximux_agents::attention::Attention {
        kind: AttentionKind::NeedsPermission,
        since: 0,
        reason: Some("Run cargo test".into()),
        source: oximux_agents::attention::AttentionSource::Health,
    };

    assert_eq!(
        line(&change(
            Some(needs_you.clone()),
            Some(edge("req:1", AttentionKind::NeedsPermission)),
            None
        )),
        "chat:s: Needs approval — Run cargo test [req:1]",
    );
    assert_eq!(
        line(&change(None, None, Some(edge("req:1", AttentionKind::NeedsPermission)))),
        "chat:s: wants nothing now (was needs approval [req:1])",
        "the only record that a session which had been waiting no longer is",
    );
    assert_eq!(
        line(&change(
            Some(needs_you),
            Some(edge("req:2", AttentionKind::NeedsPermission)),
            Some(edge("req:1", AttentionKind::NeedsPermission))
        )),
        "chat:s: Needs approval — Run cargo test [req:2] (was needs approval [req:1])",
        "a replacement names both, so a surface can retire the old alert",
    );
}
