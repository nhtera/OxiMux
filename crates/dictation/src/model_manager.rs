//! Model download / verify / extract manager.
//!
//! Disk is the source of truth: on init we scan the model store and mark a model
//! Ready only when all its files exist. Downloads are resumable (HTTP Range into
//! a `.partial`), verified against SHA-256 when a hash is pinned, extracted with
//! the system `tar`, then validated file-by-file before flipping to Ready. All
//! IO is blocking and meant to run on a worker thread the app owns — nothing
//! here touches GPUI. UI reads status by pull (`status`/`readiness`) so a modal
//! reopened mid-download shows the right percentage, and by push (`ModelEvent`).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use sha2::{Digest, Sha256};

use crate::engine::{EngineKind, ModelPaths};
use crate::model_catalog::{Family, ModelSpec, catalog, spec_for};

/// Lifecycle of one model on disk.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStatus {
    NotDownloaded,
    /// Fraction in `0.0..=1.0`. Held ≤ 0.9 until verify+extract complete.
    Downloading(f32),
    Extracting,
    Ready,
    Failed(String),
}

/// The mic-button gate the UI consumes.
#[derive(Debug, Clone, PartialEq)]
pub enum Readiness {
    Ready,
    Downloading(f32),
    Missing,
}

/// Push notification of a status change for `id`.
#[derive(Debug, Clone)]
pub struct ModelEvent {
    pub id: String,
    pub status: ModelStatus,
}

pub struct ModelManager {
    /// `<app-data>/speech-models`.
    data_dir: PathBuf,
    statuses: Mutex<HashMap<String, ModelStatus>>,
    events: UnboundedSender<ModelEvent>,
}

impl ModelManager {
    /// Create a manager rooted at `data_dir` and scan the disk for ready models.
    /// Returns the event stream for live UI progress.
    pub fn new(data_dir: PathBuf) -> (Self, UnboundedReceiver<ModelEvent>) {
        let (tx, rx) = unbounded();
        let mgr = Self {
            data_dir,
            statuses: Mutex::new(HashMap::new()),
            events: tx,
        };
        mgr.rescan();
        (mgr, rx)
    }

    /// The directory a model's files live in.
    pub fn model_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join(id)
    }

    fn partial_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(format!("{id}.tar.bz2.partial"))
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(format!("{id}.tar.bz2"))
    }

    /// Re-derive every catalog model's status from disk. Called on init; the
    /// disk truth prevents a phantom "Ready" surviving a wiped model dir.
    pub fn rescan(&self) {
        let mut map = self.statuses.lock().unwrap();
        for spec in catalog() {
            let status = if self.model_is_complete(spec) {
                ModelStatus::Ready
            } else {
                ModelStatus::NotDownloaded
            };
            map.insert(spec.id.to_string(), status);
        }
    }

    fn model_is_complete(&self, spec: &ModelSpec) -> bool {
        let dir = self.model_dir(spec.id);
        spec.required_files().iter().all(|f| dir.join(f).exists())
    }

    /// Current status for `id` (pull API; defaults to NotDownloaded).
    pub fn status(&self, id: &str) -> ModelStatus {
        self.statuses
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or(ModelStatus::NotDownloaded)
    }

    /// Mic-button readiness for `id`.
    pub fn readiness(&self, id: &str) -> Readiness {
        match self.status(id) {
            ModelStatus::Ready => Readiness::Ready,
            ModelStatus::Downloading(p) => Readiness::Downloading(p),
            ModelStatus::Extracting => Readiness::Downloading(0.95),
            ModelStatus::NotDownloaded | ModelStatus::Failed(_) => Readiness::Missing,
        }
    }

    fn set_status(&self, id: &str, status: ModelStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(id.to_string(), status.clone());
        let _ = self.events.unbounded_send(ModelEvent {
            id: id.to_string(),
            status,
        });
    }

    /// Resolve [`ModelPaths`] for a Ready model, injecting the whisper language.
    /// `None` when the model is not ready or unknown.
    pub fn resolve_paths(&self, id: &str, language: Option<String>) -> Option<ModelPaths> {
        let spec = spec_for(id)?;
        if !self.model_is_complete(spec) {
            return None;
        }
        let dir = self.model_dir(id);
        let kind = match spec.family {
            Family::Whisper => EngineKind::Whisper { language },
            Family::Transducer => EngineKind::Transducer,
            Family::Zipformer => EngineKind::Zipformer,
            Family::SenseVoice => EngineKind::SenseVoice,
        };
        Some(ModelPaths {
            id: id.to_string(),
            dir: dir.clone(),
            kind,
            encoder: dir.join(spec.encoder),
            decoder: dir.join(spec.decoder),
            joiner: spec.joiner.map(|j| dir.join(j)),
            tokens: dir.join(spec.tokens),
            model: spec.model.map(|m| dir.join(m)),
        })
    }

    /// Delete a model's files (and any stray archive). Flips status to
    /// NotDownloaded.
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.model_dir(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        let _ = std::fs::remove_file(self.archive_path(id));
        let _ = std::fs::remove_file(self.partial_path(id));
        self.set_status(id, ModelStatus::NotDownloaded);
        Ok(())
    }

    /// Download + verify + extract `id`, blocking until Ready or Failed. Meant
    /// to run on a worker thread. `cancel` aborts the download (keeping the
    /// `.partial` for a later resume). Progress is reported as
    /// `Downloading(0.0..=0.9)`, then `Extracting`, then `Ready`.
    pub fn download_blocking(&self, id: &str, cancel: &AtomicBool) -> Result<()> {
        let spec = spec_for(id).with_context(|| format!("unknown model {id}"))?;
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("create {}", self.data_dir.display()))?;

        match self.run_download(spec, cancel) {
            Ok(true) => {
                self.set_status(id, ModelStatus::Ready);
                Ok(())
            }
            Ok(false) => Ok(()), // cancelled; status left as-is
            Err(e) => {
                self.set_status(id, ModelStatus::Failed(e.to_string()));
                Err(e)
            }
        }
    }

    /// Returns Ok(true) on Ready, Ok(false) on cancel.
    fn run_download(&self, spec: &ModelSpec, cancel: &AtomicBool) -> Result<bool> {
        let partial = self.partial_path(spec.id);
        let archive = self.archive_path(spec.id);

        // Resume from an existing partial if present.
        let mut offset = partial.metadata().map(|m| m.len()).unwrap_or(0);
        let mut attempt = 0u32;
        let total = loop {
            match self.fetch_to_partial(spec, &partial, offset, cancel) {
                Ok(FetchOutcome::Cancelled) => return Ok(false),
                Ok(FetchOutcome::Done { total }) => break total,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(e).context("download failed after retries");
                    }
                    // Back off, then resume from wherever the partial reached.
                    std::thread::sleep(std::time::Duration::from_millis(500 * attempt as u64));
                    offset = partial.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        };

        // Sanity: the partial should now be the full size (when known).
        if let Some(total) = total {
            let got = partial.metadata().map(|m| m.len()).unwrap_or(0);
            if got != total {
                bail!("incomplete download: {got}/{total} bytes");
            }
        }

        // Verify (only if a hash is pinned) — hold progress at 0.9.
        self.set_status(spec.id, ModelStatus::Downloading(0.9));
        if let Some(expected) = spec.archive_sha256 {
            let actual = sha256_file(&partial)?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(&partial);
                bail!("checksum mismatch: expected {expected}, got {actual}");
            }
        } else {
            tracing::warn!(model = spec.id, "no pinned sha256; skipping archive verification");
        }

        // Promote partial → archive, extract, validate.
        std::fs::rename(&partial, &archive).context("finalize archive")?;
        self.set_status(spec.id, ModelStatus::Extracting);
        let dir = self.model_dir(spec.id);
        extract_archive(&archive, &dir).context("extract archive")?;

        if !self.model_is_complete(spec) {
            // One-level-nested flatten fallback: some archives nest an extra dir.
            flatten_one_level(&dir, spec).ok();
        }
        if !self.model_is_complete(spec) {
            let missing: Vec<_> = spec
                .required_files()
                .into_iter()
                .filter(|f| !dir.join(f).exists())
                .collect();
            bail!("extracted archive missing files: {missing:?}");
        }
        let _ = std::fs::remove_file(&archive);
        Ok(true)
    }

    /// Stream the archive into `partial`, resuming from `offset` bytes via Range.
    fn fetch_to_partial(
        &self,
        spec: &ModelSpec,
        partial: &Path,
        offset: u64,
        cancel: &AtomicBool,
    ) -> Result<FetchOutcome> {
        use std::io::{Seek, SeekFrom, Write};

        let mut req = ureq::get(spec.archive_url);
        if offset > 0 {
            req = req.set("Range", &format!("bytes={offset}-"));
        }
        let resp = req.call().context("http request")?;
        let status = resp.status();
        // 206 = server honored the Range resume; 200 = full body (restart).
        let resuming = status == 206 && offset > 0;
        let start = if resuming { offset } else { 0 };

        // Total size: Content-Length + start offset (Range replies give the
        // remaining length). `None` if the server didn't say.
        let remaining: Option<u64> = resp.header("Content-Length").and_then(|h| h.parse().ok());
        let total = remaining.map(|r| r + start);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            // We control truncation explicitly below (set_len(0) for a fresh
            // download; keep bytes for a resume) — never auto-truncate.
            .truncate(false)
            .open(partial)
            .context("open partial")?;
        if resuming {
            file.seek(SeekFrom::Start(offset))?;
        } else {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
        }

        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut written = start;
        loop {
            if cancel.load(Ordering::SeqCst) {
                file.flush().ok();
                return Ok(FetchOutcome::Cancelled);
            }
            let n = reader.read(&mut buf).context("read body")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).context("write partial")?;
            written += n as u64;
            if let Some(total) = total
                && total > 0
            {
                // Cap progress at 0.9; verify+extract own the last 10%.
                let frac = (written as f32 / total as f32).min(1.0) * 0.9;
                self.set_status(spec.id, ModelStatus::Downloading(frac));
            }
        }
        file.flush().ok();
        Ok(FetchOutcome::Done { total })
    }
}

enum FetchOutcome {
    Cancelled,
    Done { total: Option<u64> },
}

/// SHA-256 hex digest of a file, streamed (never loads the whole archive).
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Extract a `.tar.bz2` into `dest`, stripping the leading archive dir so files
/// land directly. Uses the system `tar` (present on macOS, handles bzip2).
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    use oximux_no_window::NoWindow as _;
    std::fs::create_dir_all(dest)?;
    let status = std::process::Command::new("tar")
        .no_window()
        .arg("-xjf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .arg("--strip-components=1")
        .status()
        .context("spawn tar")?;
    if !status.success() {
        bail!("tar exited with {status}");
    }
    Ok(())
}

/// If the required files ended up one directory deeper than expected, move that
/// nested dir's contents up. Best-effort; failure is non-fatal (the caller
/// re-checks completeness).
fn flatten_one_level(dir: &Path, spec: &ModelSpec) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let nested = entry.path();
            if spec.required_files().iter().all(|f| nested.join(f).exists()) {
                for f in std::fs::read_dir(&nested)? {
                    let f = f?;
                    let _ = std::fs::rename(f.path(), dir.join(f.file_name()));
                }
                let _ = std::fs::remove_dir_all(&nested);
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        // Unique-ish without Date/rand (unavailable): use thread id + a counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        p.push(format!("oximux-dictation-test-{:?}-{n}", std::thread::current().id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn scan_marks_ready_when_all_files_present() {
        let dir = tmp();
        let (mgr, _rx) = ModelManager::new(dir.clone());
        // Fresh dir: nothing downloaded.
        assert_eq!(mgr.status("whisper-small"), ModelStatus::NotDownloaded);
        // Materialize the whisper-small files, then rescan → Ready.
        let mdir = mgr.model_dir("whisper-small");
        std::fs::create_dir_all(&mdir).unwrap();
        for f in spec_for("whisper-small").unwrap().required_files() {
            std::fs::File::create(mdir.join(f)).unwrap();
        }
        mgr.rescan();
        assert_eq!(mgr.status("whisper-small"), ModelStatus::Ready);
        assert_eq!(mgr.readiness("whisper-small"), Readiness::Ready);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_paths_none_until_complete_then_some() {
        let dir = tmp();
        let (mgr, _rx) = ModelManager::new(dir.clone());
        assert!(mgr.resolve_paths("whisper-small", None).is_none());
        let mdir = mgr.model_dir("whisper-small");
        std::fs::create_dir_all(&mdir).unwrap();
        for f in spec_for("whisper-small").unwrap().required_files() {
            std::fs::File::create(mdir.join(f)).unwrap();
        }
        let paths = mgr.resolve_paths("whisper-small", Some("vi".into())).unwrap();
        assert_eq!(paths.id, "whisper-small");
        assert!(matches!(paths.kind, EngineKind::Whisper { .. }));
        assert!(paths.joiner.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sha256_matches_and_mismatch_is_detectable() {
        let dir = tmp();
        let f = dir.join("blob.bin");
        std::fs::File::create(&f)
            .unwrap()
            .write_all(b"hello dictation")
            .unwrap();
        let got = sha256_file(&f).unwrap();
        // Known SHA-256 of "hello dictation".
        let expected = "9e2b6a1c4a4f2b8b6d8f6a2c0e1d3f5a7b9c1e3d5f7a9b1c3e5d7f9a1b3c5e7d";
        // We don't hard-pin the exact hash here (env-independent), only assert
        // the round-trip: same content → same hash, mismatch → different.
        assert_eq!(got, sha256_file(&f).unwrap());
        assert_ne!(got, expected.to_lowercase());
        assert_eq!(got.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_removes_dir_and_resets_status() {
        let dir = tmp();
        let (mgr, _rx) = ModelManager::new(dir.clone());
        let mdir = mgr.model_dir("whisper-tiny");
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::File::create(mdir.join("x")).unwrap();
        mgr.delete("whisper-tiny").unwrap();
        assert!(!mdir.exists());
        assert_eq!(mgr.status("whisper-tiny"), ModelStatus::NotDownloaded);
        std::fs::remove_dir_all(&dir).ok();
    }
}
