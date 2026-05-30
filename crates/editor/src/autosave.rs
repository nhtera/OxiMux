//! Autosave coordination stubs.
//!
//! The SCM panel needs a way to pause editor autosave around a
//! destructive `git restore --` so that an open buffer doesn't race the
//! filesystem write and immediately re-save the user's old content over
//! the freshly-checked-out version. The editor itself has no autosave
//! pump yet, so these calls are intentionally no-ops — the public
//! signature is stable from the host's perspective, and the body grows
//! when the editor side ships an autosave loop.
//!
//! Both functions are infallible. Passing a path that no editor has
//! open is a no-op (consistent behaviour whether the underlying pump
//! exists or not).

use std::path::Path;

/// Suspend autosave for the buffer (if any) backing `path`. No-op when
/// no editor tab has the path open or when the autosave pump is dormant.
pub fn pause_autosave(path: &Path) {
    tracing::trace!(
        target: "oximux_editor::autosave",
        path = %path.display(),
        "pause_autosave (stub)"
    );
}

/// Resume autosave for `path`. Idempotent — calling it without a
/// matching `pause_autosave` is harmless.
pub fn resume_autosave(path: &Path) {
    tracing::trace!(
        target: "oximux_editor::autosave",
        path = %path.display(),
        "resume_autosave (stub)"
    );
}
