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
use gpui::{App, BorrowAppContext, Global, WeakEntity, Window};
use gpui_component::WindowExt as _;
use oximux_dictation::{
    DictationController, DictationEvent, ModelManager, ModelPaths, ModelStatus, Readiness,
};
use oximux_settings::DictationSettings;

use super::composer::ComposerView;
use super::dictation_hud::DictationHud;

/// Where a live dictation session delivers its events + transcript. The composer
/// receives events directly (it renders its own in-line recording bar); every
/// other pane (terminal, code editor) is driven by the global [`DictationHud`],
/// which owns the concrete terminal/editor sink and renders the floating pill.
#[derive(Clone)]
pub enum DictationTarget {
    Composer(WeakEntity<ComposerView>),
    Hud(WeakEntity<DictationHud>),
}

impl DictationTarget {
    /// Whether the receiving entity is still alive (its tab/window wasn't closed
    /// mid-session). The HUD outlives panes, so a HUD target reports alive here
    /// and the HUD itself watches its terminal/editor sink for death.
    fn is_alive(&self) -> bool {
        match self {
            DictationTarget::Composer(w) => w.upgrade().is_some(),
            DictationTarget::Hud(w) => w.upgrade().is_some(),
        }
    }
}

/// The synchronous outcome of the pre-recording checks (settings enabled, model
/// ready, permission). Callers turn `Ready`/`NeedsPermission` into a `start`.
pub enum StartDecision {
    /// All clear — begin immediately with these resolved paths + device.
    Ready {
        paths: ModelPaths,
        device: Option<String>,
    },
    /// Mic permission is undetermined — request it, then begin on grant.
    NeedsPermission {
        paths: ModelPaths,
        device: Option<String>,
    },
    /// Can't start (disabled / no model / denied) — a toast was already shown.
    Blocked,
}

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
    /// Where the current session (if any) delivers its events — a composer, or
    /// the (per-window) HUD driving a terminal/editor pane. The `Hud` variant
    /// carries its own window's HUD handle, so routing never needs a global HUD
    /// pointer (which would misroute across multiple windows).
    target: Option<DictationTarget>,
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
        target: None,
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
    // Post-process a final transcript once at this shared choke point (before
    // history + every target pane) so the cleaned text is what gets recorded and
    // inserted. Non-final events pass through untouched.
    let ev = match ev {
        DictationEvent::Final(text) => DictationEvent::Final(post_process_transcript(cx, text)),
        other => other,
    };
    // Record every completed transcript to the on-disk history BEFORE routing —
    // this is the single choke point all panes share, so history is captured even
    // if the target pane was closed mid-session. A repaint surfaces it live in an
    // open Voice pane. Blank transcripts are dropped by `record`.
    if let DictationEvent::Final(text) = &ev {
        super::dictation_history::record(text);
        cx.refresh_windows();
    }
    let target = {
        let Some(svc) = cx.try_global::<DictationService>() else {
            return;
        };
        svc.target.clone()
    };
    let Some(target) = target else {
        return;
    };
    // The receiving entity is gone (its tab/window closed mid-session). Cancel
    // the orphaned session so the mic doesn't stay hot until the 2-minute cap.
    // Level events fire ~10 Hz, so this fires within ~100 ms of the close.
    if !target.is_alive() {
        cx.update_global::<DictationService, _>(|svc, _| {
            svc.controller.cancel();
            svc.target = None;
        });
        return;
    }
    // Deliver to the target's OWN entity (not a process-global HUD pointer): with
    // multiple workspace windows each owning a `DictationHud`, routing by a shared
    // pointer would deliver the older window's session events to the newer
    // window's HUD, which would self-cancel it. The `Hud(weak)` carried here is
    // the exact HUD that began the session.
    match target {
        DictationTarget::Composer(weak) => {
            if let Some(entity) = weak.upgrade() {
                entity.update(cx, |composer, cx| composer.on_dictation_event(ev, cx));
            }
        }
        DictationTarget::Hud(weak) => {
            if let Some(hud) = weak.upgrade() {
                hud.update(cx, |hud, cx| hud.on_dictation_event(ev, cx));
            }
        }
    }
}

/// Clean a final transcript per the live dictation settings: filler/hallucination
/// filtering first (drops "(sad music)", "um", stutters), then fuzzy custom-word
/// correction toward the user's dictionary. Reads the settings global; absent
/// (unit tests) → identity.
fn post_process_transcript(cx: &App, text: String) -> String {
    let Some(settings) = cx.try_global::<DictationSettings>() else {
        return text;
    };
    // Filter first so fillers/phantoms don't get fuzzy-matched to a custom word,
    // then correct the surviving words toward the dictionary.
    let filtered =
        oximux_dictation::text_filter::filter(&text, &settings.language, settings.filler_filter_enabled);
    oximux_dictation::custom_words::apply(
        &filtered,
        &settings.custom_words,
        settings.word_correction_threshold,
    )
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

/// Register `target` as the active recorder and start capture from `device`
/// (`None` = system default). Returns false if a session is already active (one
/// recorder at a time) or the service is absent.
pub fn start(
    cx: &mut App,
    target: DictationTarget,
    paths: ModelPaths,
    device: Option<String>,
) -> bool {
    if cx.try_global::<DictationService>().is_none() {
        return false;
    }
    // Read the VAD toggle before the mutable global borrow below.
    let vad_enabled = cx
        .try_global::<DictationSettings>()
        .map(|s| s.vad_enabled)
        .unwrap_or(true);
    cx.update_global::<DictationService, _>(|svc, _| {
        if svc.controller.is_active() {
            return false;
        }
        svc.target = Some(target);
        svc.controller.start(paths, device, vad_enabled)
    })
}

/// Whether a recording session is live right now (any target).
pub fn is_active(cx: &App) -> bool {
    cx.try_global::<DictationService>()
        .map(|s| s.controller.is_active())
        .unwrap_or(false)
}

/// Run the synchronous pre-recording checks (enabled → model ready → resolve
/// paths → permission) shared by the composer and the HUD. Emits the right toast
/// and returns [`StartDecision::Blocked`] on any failure; otherwise returns the
/// resolved paths + device so the caller can begin (immediately, or after a
/// permission prompt).
pub fn prepare_start(cx: &mut App, window: &mut Window) -> StartDecision {
    let settings = cx
        .try_global::<DictationSettings>()
        .cloned()
        .unwrap_or_default();
    if !settings.enabled {
        window.push_notification("Dictation is disabled — enable it in Settings › Voice", cx);
        return StartDecision::Blocked;
    }
    let model_id = settings.model_id.clone();
    match readiness(cx, &model_id) {
        Readiness::Ready => {}
        Readiness::Downloading(p) => {
            let pct = (p * 100.0).round() as u32;
            window.push_notification(format!("Downloading dictation model… {pct}%"), cx);
            return StartDecision::Blocked;
        }
        Readiness::Missing => {
            window.push_notification(
                "No dictation model yet — download one in Settings › Voice",
                cx,
            );
            return StartDecision::Blocked;
        }
    }
    let Some(paths) = resolve_paths(cx, &model_id, settings.language_param()) else {
        window.push_notification("Dictation model isn't ready", cx);
        return StartDecision::Blocked;
    };
    let device = settings.device_name();
    match crate::mic_permission::status() {
        crate::mic_permission::MicPermission::Granted => StartDecision::Ready { paths, device },
        crate::mic_permission::MicPermission::Denied
        | crate::mic_permission::MicPermission::Restricted => {
            window.push_notification(
                "Microphone access is off — enable it in System Settings › Privacy › Microphone",
                cx,
            );
            StartDecision::Blocked
        }
        crate::mic_permission::MicPermission::Undetermined => {
            StartDecision::NeedsPermission { paths, device }
        }
    }
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
