//! Serve's attention subscriber: the host log.
//!
//! The host core derives attention the same way under `oximux serve` as it
//! does in the app — the evaluator, the edge identities and the retirements
//! are the core's, not a host's. What a headless host lacks is a *reader*.
//!
//! That is not cosmetic. The core's followers run only while something is
//! subscribed to the stream (`feeding_subscribers`), deliberately: they exist
//! to feed it, and with no subscriber they would read the preferences database
//! on every health change for nobody. So without this module serve would not
//! merely log nothing — it would never evaluate attention at all, and the
//! first surface to want it (the push planner, P7) would be the first to find
//! out whether any of it worked.
//!
//! So the log is the subscriber, and an operator watching `oximux serve` sees
//! what a rail would have shown: which session needs a person, why, and when
//! it stopped needing one. A notification is a different thing, with its own
//! preferences (`notify.*`) and its own once-per-edge rule; this is a record
//! of every change, so it suppresses nothing.

use std::sync::Arc;

use oximux_remote_host::HostCore;
use oximux_remote_host::attention::AttentionChanged;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

/// Follow `core`'s attention changes and write each one to the host log.
pub(crate) fn start(core: &Arc<HostCore>) {
    // Subscribing is also what starts the core's followers for sessions that
    // are already live, so this is called while the host is still booting
    // rather than at the first client's request.
    let changes = core.subscribe_attention();
    // Its own target, crate-prefixed like the app's: `RUST_LOG=oximux_cli=debug`
    // still matches it, and an operator who wants only this can ask for
    // `oximux_cli::attention`.
    tokio::spawn(follow(changes, |line| {
        tracing::info!(target: "oximux_cli::attention", "{line}")
    }));
}

/// The loop, with its sink passed in so a test can read what an operator
/// would have seen rather than assert on a global log subscriber.
async fn follow(
    mut changes: broadcast::Receiver<AttentionChanged>,
    mut write: impl FnMut(String) + Send + 'static,
) {
    loop {
        match changes.recv().await {
            Ok(change) => write(line(&change)),
            // A log that fell behind says so. It keeps no snapshot to
            // reconcile against, so the honest record is that some changes
            // went unrecorded — not silence, which would read as a quiet host.
            Err(RecvError::Lagged(missed)) => {
                write(format!("{missed} attention changes were dropped before this log read them"))
            }
            // The core is gone: nothing more will ever be published.
            Err(RecvError::Closed) => return,
        }
    }
}

/// One change as a line.
///
/// Both halves are written, because they answer different questions. What is
/// current says who needs a person now; what was retired says what stopped —
/// and a retirement with nothing current is the only record that a session
/// which had been waiting no longer is.
fn line(change: &AttentionChanged) -> String {
    let now = change.attention.as_ref().map(|a| {
        let edge = change.edge.as_ref().map(|e| e.id.as_str()).unwrap_or("-");
        match a.reason.as_deref() {
            Some(reason) => format!("{} — {reason} [{edge}]", a.kind.verb()),
            None => format!("{} [{edge}]", a.kind.verb()),
        }
    });
    let was = change
        .retired
        .as_ref()
        .map(|e| format!("was {} [{}]", e.kind.verb().to_lowercase(), e.id));
    match (now, was) {
        (Some(now), Some(was)) => format!("{}: {now} ({was})", change.target),
        (Some(now), None) => format!("{}: {now}", change.target),
        (None, Some(was)) => format!("{}: wants nothing now ({was})", change.target),
        // `reevaluate` publishes nothing when there is neither, so this is
        // unreachable rather than a state to describe.
        (None, None) => format!("{}: nothing", change.target),
    }
}

#[cfg(test)]
#[path = "attention_log_tests.rs"]
mod tests;
