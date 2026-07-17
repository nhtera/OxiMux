//! Audible start/stop cues for a dictation press.
//!
//! A tactile confirmation that capture went live (and ended) without looking at
//! the mic pill. Deliberately **dependency-free**: rather than pull in an audio
//! playback crate plus bundled sound assets just to play two short blips, this
//! hands a stock macOS system sound to `afplay`. That keeps the crate's
//! dependency surface unchanged and means there are no binary assets to ship or
//! license.
//!
//! Non-macOS targets compile to a no-op — OxiMux is macOS-only today, and a
//! missing cue must never be a build error.
//!
//! Playback is fire-and-forget: [`play`] returns immediately and must never
//! block the caller, because it runs on the GPUI main thread at the moment a
//! recording starts.

/// Which end of a recording a cue marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// Capture just went live.
    Start,
    /// Capture just ended (transcription may still be running).
    Stop,
}

impl Cue {
    /// The stock macOS sound backing this cue. `Tink` is a short rising tick
    /// that reads as "armed"; `Pop` is a duller close that reads as "done".
    /// Both are ~200 ms, so the two never overlap in normal use.
    #[cfg(target_os = "macos")]
    fn sound_path(self) -> &'static str {
        match self {
            Cue::Start => "/System/Library/Sounds/Tink.aiff",
            Cue::Stop => "/System/Library/Sounds/Pop.aiff",
        }
    }
}

/// Play `cue` without blocking the caller.
///
/// The child is waited on inside a short-lived detached thread rather than
/// abandoned: dropping a `Child` without reaping it would leave a zombie
/// process for the lifetime of the app, and dictation can be pressed many times
/// per session. Any failure (sound file missing, `afplay` unavailable) is
/// silently ignored — a cue is a nicety, never worth surfacing an error for.
#[cfg(target_os = "macos")]
pub fn play(cue: Cue) {
    let path = cue.sound_path();
    let spawned = std::thread::Builder::new()
        .name("oximux-dictation-cue".into())
        .spawn(move || {
            let _ = std::process::Command::new("/usr/bin/afplay")
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "could not spawn dictation cue thread");
    }
}

/// No-op on non-macOS targets (no stock sound set to lean on).
#[cfg(not(target_os = "macos"))]
pub fn play(_cue: Cue) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn cue_sounds_are_distinct_and_exist_on_disk() {
        // The cues must be tellable apart, and both must be real files — a typo'd
        // path would silently play nothing.
        assert_ne!(Cue::Start.sound_path(), Cue::Stop.sound_path());
        for cue in [Cue::Start, Cue::Stop] {
            assert!(
                std::path::Path::new(cue.sound_path()).is_file(),
                "stock sound missing: {}",
                cue.sound_path()
            );
        }
    }

    #[test]
    fn play_returns_immediately_and_never_panics() {
        // Guards the fire-and-forget contract: this runs on the GPUI main thread.
        let t0 = std::time::Instant::now();
        play(Cue::Start);
        play(Cue::Stop);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(150),
            "play must not block the caller"
        );
    }
}
