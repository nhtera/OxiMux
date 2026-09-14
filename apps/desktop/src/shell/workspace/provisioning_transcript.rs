//! The provisioning transcript: where one create's event stream is written,
//! and the writer that drains it.
//!
//! Lifted out of `workspace_ops.rs`, which sits at the 3000-LOC hard cap
//! `xtask file-size-lint` enforces in CI. The formatter the writer uses lives
//! with the live card (`provision_card::provision_line`) so the file and the
//! card can never disagree about what happened.

use std::path::{Path, PathBuf};

use oximux_worktree_ops::ProvisionEvent;

/// Provisioning transcripts retained per workspace slug.
const KEEP_TRANSCRIPTS: usize = 3;

/// Where one worktree's provisioning transcript lives:
/// `<data_dir>/projects/<project_id>/provisioning/<slug>-<millis>.log`.
///
/// Outside the worktree deliberately. The transcript matters most when setup
/// failed, and that is exactly when the worktree has been rolled back — a file
/// written inside it would be deleted by the rollback that made it interesting.
///
/// A fresh path per attempt, not one file per slug. Overwriting in place looks
/// tidier and is wrong: the editor activates an already-open tab for a path
/// without re-reading it, so a second failed create would show the user the
/// *first* failure's output while the file on disk said otherwise. Older
/// transcripts for the same slug are pruned so this stays a log and not an
/// archive.
pub(crate) fn provisioning_transcript_path(project_id: &str, slug: &str) -> PathBuf {
    let dir = crate::app_paths::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("projects")
        .join(project_id)
        .join("provisioning");
    prune_transcripts(&dir, slug, KEEP_TRANSCRIPTS);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{slug}-{stamp}.log"))
}

/// Keep the `keep` newest transcripts for `slug`, delete the rest.
///
/// By name, not by mtime: the name carries the timestamp, and lexicographic
/// order over a fixed-width millisecond stamp is chronological order. Every
/// failure here is ignored — pruning a log must never be able to affect a
/// worktree create.
fn prune_transcripts(dir: &Path, slug: &str, keep: usize) {
    let prefix = format!("{slug}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // first run for this project: nothing to prune
    };
    let mut mine: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "log")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    if mine.len() < keep {
        return;
    }
    mine.sort();
    // `keep - 1`: one slot is about to be taken by the transcript this call is
    // making a path for.
    for stale in mine.iter().take(mine.len().saturating_sub(keep - 1)) {
        let _ = std::fs::remove_file(stale);
    }
}

/// Drain [`ProvisionEvent`]s into the transcript file until the sender drops.
///
/// Flushed per event rather than at the end so `tail -f` shows a long install
/// progressing, and so a hard kill mid-setup still leaves the lines that
/// explain where it got to. Every IO error here is swallowed: failing to write
/// a log must not be able to fail a worktree creation that otherwise worked.
///
/// `tee`, when given, receives a clone of every event AFTER it has been
/// written, so the live provisioning card (see `provision_card`) is fed from
/// the same stream as the file and can never disagree with it. The tee is
/// dropped when the stream ends, which is how the card's drain learns that
/// provisioning is over.
pub(crate) async fn stream_provisioning(
    path: PathBuf,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProvisionEvent>,
    tee: Option<tokio::sync::mpsc::UnboundedSender<ProvisionEvent>>,
) {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(?err, path = %path.display(), "provisioning transcript unavailable");
            // Still drain — the card can still show what happened — or the
            // unbounded channel grows for the whole run.
            while let Some(event) = rx.recv().await {
                if let Some(tee) = &tee {
                    let _ = tee.send(event);
                }
            }
            return;
        }
    };
    while let Some(event) = rx.recv().await {
        let line = super::provision_card::provision_line(&event);
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
        if let Some(tee) = &tee {
            // A dropped receiver (card dismissed, window closed) is not an
            // error for the file.
            let _ = tee.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writer forwards every event to the tee, in order, after writing
    /// it — and closes the tee when the stream ends.
    #[tokio::test]
    async fn the_tee_sees_every_event_in_order_and_then_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tee_tx, mut tee_rx) = tokio::sync::mpsc::unbounded_channel();
        let writer = tokio::spawn(stream_provisioning(path.clone(), rx, Some(tee_tx)));
        tx.send(ProvisionEvent::SetupStarted("make".into())).unwrap();
        tx.send(ProvisionEvent::SetupLine("building".into())).unwrap();
        tx.send(ProvisionEvent::SetupFinished(oximux_worktree_ops::SetupOutcome::Ok)).unwrap();
        drop(tx);
        writer.await.unwrap();

        let mut seen = Vec::new();
        while let Some(ev) = tee_rx.recv().await {
            seen.push(ev);
        }
        assert_eq!(seen.len(), 3);
        assert!(matches!(seen[0], ProvisionEvent::SetupStarted(_)));
        assert!(matches!(seen[2], ProvisionEvent::SetupFinished(_)));
        let file = std::fs::read_to_string(&path).unwrap();
        assert_eq!(file, "$ make\nbuilding\n== setup succeeded\n");
    }

    /// A dismissed card (dropped tee receiver) never disturbs the file.
    #[tokio::test]
    async fn a_dropped_tee_receiver_does_not_stop_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tee_tx, tee_rx) = tokio::sync::mpsc::unbounded_channel::<ProvisionEvent>();
        drop(tee_rx);
        let writer = tokio::spawn(stream_provisioning(path.clone(), rx, Some(tee_tx)));
        tx.send(ProvisionEvent::SetupLine("still written".into())).unwrap();
        drop(tx);
        writer.await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "still written\n");
    }
}
