//! Sessions that exist on disk but have no live view behind them.
//!
//! The registry only holds sessions the desktop has actually opened, because a
//! session's entry *is* its connection to a running agent. The desktop builds a
//! project's views the first time that project is shown, so a remote client sees
//! only the sessions belonging to projects the desktop happens to have visited —
//! and after a restart that is one project, or none. From the phone this reads as
//! the sessions having disappeared.
//!
//! A catalog closes the gap from the other end: it enumerates what is on disk so
//! the list is complete, and materializes one on demand so opening it works. The
//! desktop keeps ownership of both — this crate has no idea what a project is,
//! and building views is something only the UI layer can do.
//!
//! Deliberately *not* solved by building every project's views at startup: that
//! spawns an agent process per session per launch, and the overwhelming majority
//! are never opened.

use std::path::PathBuf;

use async_trait::async_trait;

/// A session on disk with nothing running behind it.
///
/// Carries only what a list row needs. There is no status: a dormant session has
/// no event stream, so it has no cursor and nothing outstanding — which is
/// exactly what a client should see until it is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DormantSession {
    pub session_id: String,
    /// The session's display title, if one was ever generated.
    pub title: Option<String>,
    /// The model it was last running, so a list row is not blank until it opens.
    pub model: Option<String>,
    /// Its working directory, which supplies the project label on a list row.
    pub cwd: Option<PathBuf>,
}

/// A dormant session's history, read straight from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DormantTranscript {
    /// The folded `Vec<ThreadEntry>` as JSON — the same shape a live session's
    /// snapshot carries, so one reply path serves both.
    pub entries_json: String,
    /// The model it was last running, so a client rehydrating from this seeds its
    /// fold with the same model a live session would have reported.
    pub model: Option<String>,
}

/// One selectable option — a model, or a permission mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DormantChoice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// What a dormant session's pickers offer, as its backend last reported them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DormantChoices {
    pub models: Vec<DormantChoice>,
    pub modes: Vec<DormantChoice>,
    /// The model in force, resolved — including the backend's default when the
    /// user never picked one, or a picker renders everything unselected.
    pub current_model: Option<String>,
    pub current_mode: Option<String>,
}

/// The desktop's index of persisted sessions, and the means to bring one to life.
#[async_trait]
pub trait SessionCatalog: Send + Sync {
    /// Every session on disk with no live view, cheapest-possible.
    ///
    /// Called on each session-list snapshot, so it must not touch the network or
    /// spawn anything — reading persisted layout is the intended cost. Sessions
    /// that *are* live must be omitted; the registry is authoritative for those,
    /// and a duplicate row would show a session twice with conflicting status.
    fn dormant(&self) -> Vec<DormantSession>;

    /// A dormant session's persisted transcript, read without building anything.
    ///
    /// This is what makes *reading* a session free. A client opening a
    /// conversation to see what the agent did is the common case — far more
    /// common than continuing one — and none of it needs a running agent, since
    /// the history is already on disk. Only a prompt has to reach a live backend,
    /// and that pays for [`SessionCatalog::open`] when it arrives.
    ///
    /// `None` means this desktop has no session under that id, which also answers
    /// "does it exist?" for a subscribe that must not open a stream for an id
    /// nobody has. A session that never saved a transcript still answers `Some`
    /// with an empty entry list: it exists, it simply has nothing to show yet.
    fn transcript(&self, session_id: &str) -> Option<DormantTranscript>;

    /// A dormant session's pickers, as its backend last reported them.
    ///
    /// A backend answers this over its live connection, so without a persisted
    /// copy a client opening a dormant session would make the desktop spawn an
    /// agent to fill two dropdowns — undoing everything
    /// [`SessionCatalog::transcript`] saves, since a client asks for both the
    /// moment it opens a conversation.
    ///
    /// Empty lists are a legitimate answer, and the same one a backend that
    /// offers no choices gives: the pickers are simply not shown. A session saved
    /// before this was recorded reads as empty until the desktop next saves it.
    fn choices(&self, session_id: &str) -> Option<DormantChoices>;

    /// Build the views behind `session_id` so it enters the registry.
    ///
    /// Resolves once the session is registered and commandable, or with a reason
    /// it cannot be. Slow by nature — it builds a project's panes and spawns an
    /// agent — so callers should treat it as a one-off on first access rather
    /// than something to poll.
    ///
    /// **Must be idempotent and safe to call concurrently.** Asking for a session
    /// that is already live succeeds without doing anything, and two callers
    /// racing for the same dormant one must produce a single session between
    /// them, not two agent processes for one conversation. The dispatcher
    /// serializes requests within a connection but not across them, so a second
    /// device is enough to make that race real; see [`OpenGate`] for a ready-made
    /// implementation of the sharing half.
    async fn open(&self, session_id: &str) -> Result<(), String>;
}

/// The result of a build, once it has one. `None` while still running.
type OpenOutcome = Option<Result<(), String>>;

/// Shares one in-flight `open` between every caller asking for the same session.
///
/// Materializing spawns an agent process, so two callers racing for one dormant
/// session must not each start one: the conversation would end up with two
/// backends writing to it, and the second registration would swap the first out
/// from under any subscriber. Requests are serialized within a connection but not
/// across them, so a second paired device is all it takes.
///
/// Sharing rather than locking, so a slow build does not serialize unrelated
/// sessions behind it — only callers wanting the *same* one wait, and they wait
/// on its result rather than repeating it.
#[derive(Default)]
pub struct OpenGate {
    /// Sessions currently being built, each with a subscribe-only channel that
    /// publishes the outcome. `Mutex` rather than a concurrent map because the
    /// critical section is a hash lookup and the contention is a handful of
    /// devices, not a thread pool.
    inflight: std::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Receiver<OpenOutcome>>>,
}

/// Which half of the gate a caller landed on.
enum Slot {
    /// This caller claimed the session and runs the build.
    Build(tokio::sync::watch::Sender<OpenOutcome>),
    /// Someone else is already building it; wait on their result.
    Wait(tokio::sync::watch::Receiver<OpenOutcome>),
}

impl OpenGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `build` for `session_id`, or wait on the build already running for it.
    ///
    /// The winner runs `build` and publishes its result; everyone else receives
    /// that same result. A caller that goes away mid-build does not cancel it —
    /// the work is already worth finishing for whoever else is waiting, and for
    /// the next request.
    pub async fn open<F, Fut>(&self, session_id: &str, build: F) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        // Decide inside the lock, act outside it: the guard is not `Send`, and
        // holding it across the build would serialize every session behind one.
        let slot = {
            let mut inflight = self.inflight.lock().unwrap();
            match inflight.get(session_id) {
                Some(rx) => Slot::Wait(rx.clone()),
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(None);
                    inflight.insert(session_id.to_string(), rx);
                    Slot::Build(tx)
                }
            }
        };

        match slot {
            Slot::Build(tx) => self.run_build(session_id, tx, build).await,
            // Wait for the winner to publish. A closed channel means it was
            // dropped without a result — a failure, not something to wait on.
            Slot::Wait(mut rx) => loop {
                let published = rx.borrow_and_update().clone();
                if let Some(result) = published {
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err("the session could not be opened".into());
                }
            },
        }
    }

    /// The winner's half: build, publish, and release the slot whatever happens.
    async fn run_build<F, Fut>(
        &self,
        session_id: &str,
        tx: tokio::sync::watch::Sender<OpenOutcome>,
        build: F,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        let result = build().await;
        // Publish before releasing, so a caller arriving between the two takes
        // the fresh receiver and reads the result rather than starting again.
        let _ = tx.send(Some(result.clone()));
        self.inflight.lock().unwrap().remove(session_id);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The bug this guards: two devices reaching for the same dormant session
    /// would each build it, leaving one conversation with two agent processes.
    #[tokio::test]
    async fn concurrent_opens_of_one_session_build_it_once() {
        let gate = Arc::new(OpenGate::new());
        let builds = Arc::new(AtomicUsize::new(0));
        let (release, _) = tokio::sync::broadcast::channel::<()>(1);

        let callers: Vec<_> = (0..8)
            .map(|_| {
                let gate = gate.clone();
                let builds = builds.clone();
                let mut wait = release.subscribe();
                tokio::spawn(async move {
                    gate.open("cold-1", || async move {
                        builds.fetch_add(1, Ordering::SeqCst);
                        // Hold the build open so every caller piles up behind it.
                        let _ = wait.recv().await;
                        Ok(())
                    })
                    .await
                })
            })
            .collect();

        // Let them all arrive, then finish the one build.
        tokio::task::yield_now().await;
        let _ = release.send(());
        for caller in callers {
            assert_eq!(caller.await.unwrap(), Ok(()), "every caller gets the result");
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1, "one build served them all");
    }

    /// A failure reaches every waiter, rather than one of them hanging.
    #[tokio::test]
    async fn a_failed_build_is_reported_to_everyone_waiting() {
        let gate = Arc::new(OpenGate::new());
        let first = gate.open("cold-1", || async { Err("no such project".to_string()) }).await;
        assert_eq!(first, Err("no such project".into()));
    }

    /// The slot is released, so a later attempt can retry rather than being
    /// permanently stuck on a stale failure.
    #[tokio::test]
    async fn a_session_can_be_retried_after_a_failure() {
        let gate = OpenGate::new();
        assert!(gate.open("cold-1", || async { Err("transient".to_string()) }).await.is_err());
        assert!(gate.open("cold-1", || async { Ok(()) }).await.is_ok(), "retry is allowed");
    }

    /// Different sessions do not queue behind each other.
    #[tokio::test]
    async fn separate_sessions_build_independently() {
        let gate = Arc::new(OpenGate::new());
        let builds = Arc::new(AtomicUsize::new(0));
        let a = {
            let (gate, builds) = (gate.clone(), builds.clone());
            tokio::spawn(async move {
                gate.open("a", || async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
            })
        };
        let b = {
            let (gate, builds) = (gate.clone(), builds.clone());
            tokio::spawn(async move {
                gate.open("b", || async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
            })
        };
        assert!(a.await.unwrap().is_ok());
        assert!(b.await.unwrap().is_ok());
        assert_eq!(builds.load(Ordering::SeqCst), 2, "each session built on its own");
    }
}
