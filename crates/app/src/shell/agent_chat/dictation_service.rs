//! Process-wide voice-dictation service.
//!
//! Owns the single [`DictationController`] (one recording session at a time) and
//! the [`ModelManager`], installed as a GPUI [`Global`] at app boot. A composer
//! that starts recording registers itself as the active recorder; a background
//! drain routes each [`DictationEvent`] to that composer. Model download/verify
//! runs on a worker thread; a second drain repaints open windows so the Voice
//! pane shows live progress (it reads status by pull).
//!
//! Nothing blocks the GPUI main thread: the controller's threads are detached
//! and its `Drop` only signals. Model events repaint via `refresh_windows`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gpui::{App, BorrowAppContext, Global, WeakEntity};
use oximux_dictation::{
    DictationController, DictationEvent, ModelManager, ModelPaths, ModelStatus, Readiness,
};

use super::composer::ComposerView;

/// Same per-user data dir as the settings TOMLs; models live under
/// `speech-models/`.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const MODELS_SUBDIR: &str = "speech-models";

fn models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DATA_SUBDIR)
        .join(MODELS_SUBDIR)
}

pub struct DictationService {
    manager: Arc<ModelManager>,
    controller: DictationController,
    /// The composer currently recording (registered on `start`), so the event
    /// drain knows where to deliver the transcript.
    active: Option<WeakEntity<ComposerView>>,
    /// In-flight download cancel flags, keyed by model id.
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Global for DictationService {}

/// Create the controller + manager, install as a global, and start the two
/// drains. Call once from the app's `run` closure.
pub fn install(cx: &mut App) {
    let (manager, mut model_rx) = ModelManager::new(models_dir());
    let manager = Arc::new(manager);
    let (controller, mut evt_rx) = DictationController::new();

    cx.set_global(DictationService {
        manager,
        controller,
        active: None,
        cancels: Mutex::new(HashMap::new()),
    });

    // Route dictation events to the active composer.
    cx.spawn(async move |cx| {
        while let Some(ev) = evt_rx.next().await {
            cx.update(|cx| route_event(cx, ev));
        }
    })
    .detach();

    // Repaint open windows on model status changes so the Voice pane's live
    // progress updates (it reads status by pull, this just triggers the paint).
    cx.spawn(async move |cx| {
        while model_rx.next().await.is_some() {
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

fn route_event(cx: &mut App, ev: DictationEvent) {
    let Some(svc) = cx.try_global::<DictationService>() else {
        return;
    };
    let Some(active) = svc.active.clone() else {
        return;
    };
    if let Some(entity) = active.upgrade() {
        entity.update(cx, |composer, cx| composer.on_dictation_event(ev, cx));
    } else {
        // The composer that started recording is gone (its chat tab was closed
        // mid-session). Cancel the orphaned session so the mic doesn't stay hot
        // until the 2-minute cap with no on-screen way to stop it. Recording
        // emits Level events ~10 Hz, so this fires within ~100 ms of the close.
        cx.update_global::<DictationService, _>(|svc, _| {
            svc.controller.cancel();
            svc.active = None;
        });
    }
}

// NOTE: every accessor tolerates the global being absent (returns a safe
// "missing"/no-op result) rather than `cx.global()` which panics. The service
// is installed at app boot, but the composer/settings render paths run in unit
// tests where it isn't — a render must never depend on `install` having run.

/// Mic-button readiness for `id` (pull; safe to call every render).
pub fn readiness(cx: &App, id: &str) -> Readiness {
    cx.try_global::<DictationService>()
        .map(|s| s.manager.readiness(id))
        .unwrap_or(Readiness::Missing)
}

/// Current download/extract status for `id`.
pub fn status(cx: &App, id: &str) -> ModelStatus {
    cx.try_global::<DictationService>()
        .map(|s| s.manager.status(id))
        .unwrap_or(ModelStatus::NotDownloaded)
}

/// Resolve engine paths for a ready model, injecting the whisper language.
pub fn resolve_paths(cx: &App, id: &str, language: Option<String>) -> Option<ModelPaths> {
    cx.try_global::<DictationService>()
        .and_then(|s| s.manager.resolve_paths(id, language))
}

/// Register `who` as the active recorder and start capture. Returns false if a
/// session is already active (one recorder at a time) or the service is absent.
pub fn start(cx: &mut App, who: WeakEntity<ComposerView>, paths: ModelPaths) -> bool {
    if cx.try_global::<DictationService>().is_none() {
        return false;
    }
    cx.update_global::<DictationService, _>(|svc, _| {
        if svc.controller.is_active() {
            return false;
        }
        svc.active = Some(who);
        svc.controller.start(paths)
    })
}

/// Stop recording → transcribe → deliver the transcript to the active composer.
pub fn stop(cx: &App) {
    if let Some(svc) = cx.try_global::<DictationService>() {
        svc.controller.stop();
    }
}

/// Cancel recording, discarding the audio.
pub fn cancel(cx: &App) {
    if let Some(svc) = cx.try_global::<DictationService>() {
        svc.controller.cancel();
    }
}

/// Start (or resume) a background download of `id`. No-op if already Ready or
/// downloading.
pub fn download(cx: &App, id: &str) {
    let Some(svc) = cx.try_global::<DictationService>() else {
        return;
    };
    if matches!(svc.manager.status(id), ModelStatus::Ready | ModelStatus::Downloading(_)) {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    svc.cancels
        .lock()
        .unwrap()
        .insert(id.to_string(), Arc::clone(&cancel));
    let manager = Arc::clone(&svc.manager);
    let id = id.to_string();
    std::thread::Builder::new()
        .name("oximux-model-download".into())
        .spawn(move || {
            if let Err(e) = manager.download_blocking(&id, &cancel) {
                tracing::warn!(model = %id, %e, "model download failed");
            }
        })
        .ok();
}

/// Abort an in-flight download of `id` (keeps the `.partial` for later resume).
pub fn cancel_download(cx: &App, id: &str) {
    if let Some(svc) = cx.try_global::<DictationService>()
        && let Some(flag) = svc.cancels.lock().unwrap().get(id)
    {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Delete a downloaded model's files.
pub fn delete(cx: &App, id: &str) {
    if let Some(svc) = cx.try_global::<DictationService>()
        && let Err(e) = svc.manager.delete(id)
    {
        tracing::warn!(model = %id, %e, "model delete failed");
    }
}
