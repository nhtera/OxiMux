//! The process-wide session-search service: owns the indexer thread and
//! follows the [`SessionSearchSettings`] global — on starts it, off stops it
//! (keeping the index file), Clear deletes the index and restarts if on.
//!
//! The index lives in its own owner-only folder,
//! `<data_dir>/session-search/session-search.sqlite`, apart from the app
//! database: it is derived data and safe to delete at any time.

use std::path::{Path, PathBuf};

use gpui::{App, BorrowAppContext, Global};
use oximux_agents::session_search::{IndexSources, IndexStatus, IndexerHandle, SessionIndexer};
use oximux_settings::SessionSearchSettings;

pub const DB_DIR_NAME: &str = "session-search";
pub const DB_FILE_NAME: &str = "session-search.sqlite";

pub struct SessionSearchService {
    db_path: Option<PathBuf>,
    sources: IndexSources,
    handle: Option<IndexerHandle>,
    /// A stopped indexer not yet joined. Joined off the UI thread: by the next
    /// indexer's thread, or by a Clear job.
    retired: Option<IndexerHandle>,
    /// A Clear is deleting the index: hold toggles until it has (`resume`).
    clearing: bool,
}

impl Global for SessionSearchService {}

impl SessionSearchService {
    fn new(db_path: Option<PathBuf>, sources: IndexSources) -> Self {
        Self { db_path, sources, handle: None, retired: None, clearing: false }
    }

    /// The index file, when the app has a data dir.
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    /// Whether the indexer thread is running (the feature is on).
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    /// Live counters from the running indexer — a mutex read, no IO. `None`
    /// while the feature is off.
    pub fn live_status(&self) -> Option<IndexStatus> {
        self.handle.as_ref().map(IndexerHandle::status)
    }

    /// Ask the indexer for a pass now (e.g. when search opens).
    pub fn kick(&self) {
        if let Some(h) = &self.handle {
            h.kick();
        }
    }

    /// Start or stop the indexer. Never waits for a thread: a stopped
    /// indexer is only signalled here and joined elsewhere.
    fn set_enabled(&mut self, enabled: bool) {
        if self.clearing {
            return;
        }
        match (enabled, self.handle.is_some()) {
            (true, false) => {
                if let Some(path) = self.db_path.clone() {
                    let previous = self.retired.take();
                    self.handle = Some(SessionIndexer::start(self.sources.clone(), path, previous));
                }
            }
            (false, true) => {
                if let Some(h) = self.handle.take() {
                    h.signal_stop();
                    // Dropping an older retired handle only signals it again.
                    self.retired = Some(h);
                }
            }
            _ => {}
        }
    }

    /// A Clear is running.
    pub fn is_clearing(&self) -> bool {
        self.clearing
    }
    /// Detach what Clear needs so the slow half (joining the thread, deleting
    /// files) can run off the UI thread; finish with [`Self::resume`].
    pub fn begin_clear(&mut self) -> ClearJob {
        self.clearing = true;
        let handles = [self.handle.take(), self.retired.take()].into_iter().flatten().collect();
        ClearJob { handles, db_path: self.db_path.clone() }
    }

    /// Restart (or not) after a clear, per the setting as it is now.
    pub fn resume(&mut self, enabled: bool) {
        self.clearing = false;
        self.set_enabled(enabled);
    }

    /// Delete the index, restarting the indexer afterwards if it was running.
    #[cfg(test)]
    fn clear(&mut self) -> anyhow::Result<()> {
        let was_running = self.is_running();
        self.begin_clear().run()?;
        self.resume(was_running);
        Ok(())
    }
}

/// The detached half of a Clear: stop the indexer, then delete the files.
pub struct ClearJob {
    handles: Vec<IndexerHandle>,
    db_path: Option<PathBuf>,
}

impl ClearJob {
    /// Blocking: run it on a background executor.
    pub fn run(self) -> anyhow::Result<()> {
        for h in self.handles {
            h.stop();
        }
        if let Some(path) = &self.db_path {
            oximux_agents::session_search::delete_index_files(path)?;
        }
        Ok(())
    }
}

/// Install the service and start the indexer if the setting is on. Never
/// blocks boot: the database opens on the indexer's own thread.
pub fn install(cx: &mut App) {
    let db_path = crate::app_paths::data_dir().map(|d| d.join(DB_DIR_NAME).join(DB_FILE_NAME));
    let sources = dirs::home_dir().map(|h| IndexSources::for_home(&h)).unwrap_or_default();
    let mut service = SessionSearchService::new(db_path, sources);
    service.set_enabled(cx.try_global::<SessionSearchSettings>().is_some_and(|s| s.enabled));
    cx.set_global(service);
    cx.observe_global::<SessionSearchSettings>(|cx| {
        let enabled = cx.global::<SessionSearchSettings>().enabled;
        cx.update_global::<SessionSearchService, _>(|s, _| s.set_enabled(enabled));
    })
    .detach();
}

/// Quit: signal the thread without waiting (dropping the handle does that).
/// Its writes are transactional, so an interrupted batch is redone next
/// launch.
pub fn on_quit(cx: &mut App) {
    if cx.has_global::<SessionSearchService>() {
        cx.update_global::<SessionSearchService, _>(|s, _| {
            drop(s.handle.take());
            drop(s.retired.take());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dir: &Path) -> SessionSearchService {
        let sources = IndexSources { claude_dir: Some(dir.join(".claude")), codex_dir: None };
        SessionSearchService::new(Some(dir.join("data").join(DB_FILE_NAME)), sources)
    }

    fn wait_for_file(path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("{} never appeared", path.display());
    }

    #[test]
    fn toggles_start_and_stop_the_indexer_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = service(dir.path());
        assert!(!s.is_running() && s.live_status().is_none());
        s.set_enabled(true);
        assert!(s.is_running());
        wait_for_file(s.db_path().unwrap());
        let started = std::time::Instant::now();
        s.set_enabled(false);
        assert!(!s.is_running() && s.retired.is_some());
        assert!(started.elapsed() < std::time::Duration::from_millis(50), "stop took {:?}", started.elapsed());
        // Off keeps the index; only Clear removes it.
        assert!(s.db_path().unwrap().exists());
        // Back on: the new indexer takes over the retired one.
        s.set_enabled(true);
        assert!(s.is_running() && s.retired.is_none());
        s.set_enabled(false);
    }

    #[test]
    fn toggles_wait_while_a_clear_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = service(dir.path());
        s.set_enabled(true);
        let job = s.begin_clear();
        s.set_enabled(false);
        s.set_enabled(true);
        assert!(!s.is_running(), "no indexer may start under a running Clear");
        job.run().unwrap();
        s.resume(true);
        assert!(s.is_running() && !s.is_clearing());
        s.set_enabled(false);
    }

    #[test]
    fn clear_deletes_the_index_and_restarts_when_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = service(dir.path());
        s.set_enabled(true);
        let path = s.db_path().unwrap().to_path_buf();
        wait_for_file(&path);
        s.clear().unwrap();
        assert!(s.is_running());
        // The restarted indexer recreates a fresh file.
        wait_for_file(&path);
        s.set_enabled(false);
        s.clear().unwrap();
        assert!(!s.is_running());
        assert!(!path.exists());
    }
}
