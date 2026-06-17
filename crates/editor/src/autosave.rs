//! Autosave coordination.
//!
//! Two concerns live here:
//!
//! 1. **Pause registry** — the SCM panel suspends autosave for a path around a
//!    destructive `git restore --` so an open buffer's debounced write can't
//!    race the filesystem and clobber the freshly-checked-out version. The
//!    calls are path-keyed free functions (the SCM side has no editor handle),
//!    so the paused set is a process-wide refcounted registry. Refcounting
//!    keeps nested / overlapping pauses on the same path from resuming early.
//!    The editor's per-view pump consults [`is_autosave_paused`] before it
//!    writes.
//!
//! 2. The debounced write pump itself lives on [`crate::editor_view::EditorView`]
//!    (it needs the buffer + GPUI context); the cadence comes from
//!    `oximux_settings::AutosaveSettings`.
//!
//! All functions here are infallible. Resuming a path that was never paused is
//! a harmless no-op.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Process-wide set of paths whose autosave is suspended, with a refcount per
/// path so overlapping pause/resume pairs don't resume prematurely.
static PAUSED: OnceLock<Mutex<HashMap<PathBuf, u32>>> = OnceLock::new();

fn paused() -> &'static Mutex<HashMap<PathBuf, u32>> {
    PAUSED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Suspend autosave for the buffer (if any) backing `path`. Refcounted: pair
/// each call with exactly one [`resume_autosave`].
pub fn pause_autosave(path: &Path) {
    tracing::trace!(
        target: "oximux_editor::autosave",
        path = %path.display(),
        "pause_autosave"
    );
    let mut map = paused().lock().expect("autosave pause registry poisoned");
    *map.entry(path.to_path_buf()).or_insert(0) += 1;
}

/// Resume autosave for `path`. Idempotent — resuming without a matching
/// [`pause_autosave`] is harmless (the refcount floors at zero).
pub fn resume_autosave(path: &Path) {
    tracing::trace!(
        target: "oximux_editor::autosave",
        path = %path.display(),
        "resume_autosave"
    );
    let mut map = paused().lock().expect("autosave pause registry poisoned");
    if let Some(count) = map.get_mut(path) {
        *count -= 1;
        if *count == 0 {
            map.remove(path);
        }
    }
}

/// `true` while autosave is suspended for `path`. The per-view pump checks
/// this immediately before writing so a destructive SCM op in flight is never
/// overwritten by a stale buffer.
pub fn is_autosave_paused(path: &Path) -> bool {
    paused()
        .lock()
        .expect("autosave pause registry poisoned")
        .contains_key(path)
}

#[cfg(test)]
mod tests {
    // NOTE: `PAUSED` is process-global, so these tests must use distinct
    // paths to avoid cross-test pollution when run on the same process.
    use super::*;

    #[test]
    fn pause_resume_round_trip() {
        let p = PathBuf::from("/tmp/oximux-autosave-test-a.rs");
        assert!(!is_autosave_paused(&p));
        pause_autosave(&p);
        assert!(is_autosave_paused(&p));
        resume_autosave(&p);
        assert!(!is_autosave_paused(&p));
    }

    #[test]
    fn refcount_survives_nested_pause() {
        let p = PathBuf::from("/tmp/oximux-autosave-test-b.rs");
        pause_autosave(&p);
        pause_autosave(&p);
        resume_autosave(&p);
        // One resume left outstanding — still paused.
        assert!(is_autosave_paused(&p));
        resume_autosave(&p);
        assert!(!is_autosave_paused(&p));
    }

    #[test]
    fn resume_without_pause_is_noop() {
        let p = PathBuf::from("/tmp/oximux-autosave-test-c.rs");
        resume_autosave(&p); // must not panic / underflow
        assert!(!is_autosave_paused(&p));
    }
}
