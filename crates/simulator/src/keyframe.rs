//! A session's frame signal: when the screen last *changed*, measured where
//! pictures arrive (before decoding), and the one gate every forced key frame
//! goes through.
//!
//! "A frame arrived" is not evidence of change: a helper may stream a static
//! screen at a fixed rate, and a requested key frame is a large picture of the
//! same screen. So [`FrameClock::last_motion_at`] moves only on a picture that
//! carries change — a delta above [`STATIC_BPPF`] bits per pixel, a key frame
//! nobody asked for, a picture dropped before the decoder (its content is
//! unknown), a JPEG whose size moved by more than [`JPEG_DELTA`]. The verify
//! loop's settle reads it to know when the screen has gone quiet.
//!
//! [`KeyframeLimiter`] holds every forced key frame to at most one per
//! [`MIN_FORCED_GAP`], shared by whoever asks (the decoder's resync today, more
//! viewers later), and tags a key frame arriving within [`REQUESTED_WINDOW`] of
//! a request as requested, so it never reads as motion.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// At most one forced key frame per this long, whoever asks.
pub const MIN_FORCED_GAP: Duration = Duration::from_secs(1);
/// A key frame this soon after a request is the requested one.
pub const REQUESTED_WINDOW: Duration = Duration::from_millis(500);
/// A delta frame above this many bits per pixel is a change on screen.
///
/// Provisional until measured (the verify loop's spike, phase 1 step 1): an
/// idle H.264 delta is a few hundred bytes on a ~3 Mpx stream (≈ 0.0005), a
/// scroll tens of kilobytes (≈ 0.05+). The `trace` log in
/// [`FrameSignal::frame`] prints each delta's figure for that measurement.
pub const STATIC_BPPF: f64 = 0.004;
/// A JPEG frame whose size differs from the last by more than this fraction
/// is a change on screen (a static screen re-encodes to within a few bytes).
pub const JPEG_DELTA: f64 = 0.02;

/// When pictures last arrived, and when one last carried change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameClock {
    /// Pictures that arrived (key, delta or JPEG).
    pub seq: u64,
    pub last_frame_at: Option<Instant>,
    pub last_motion_at: Option<Instant>,
    /// Bumped when the stream pauses or resumes: across a gap in the
    /// pictures, "no motion seen" proves nothing about the screen.
    pub epoch: u64,
}

impl FrameClock {
    /// Whether nothing has changed on screen for `quiet`, counting no earlier
    /// than `since` (the action's start: motion before it is not its own).
    pub fn quiet_for(&self, since: Instant, now: Instant, quiet: Duration) -> bool {
        let from = self.last_motion_at.map_or(since, |motion| motion.max(since));
        now.saturating_duration_since(from) >= quiet
    }
}

/// One picture as it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ingress {
    /// An H.264 IDR of this many bytes.
    Key { bytes: usize },
    /// An H.264 delta of this many bytes, on a stream of `pixels` per picture
    /// (`None` before the size is known).
    Delta { bytes: usize, pixels: Option<u64> },
    /// A JPEG of this many bytes.
    Jpeg { bytes: usize },
}

/// Forced key frames: rate-limited and tagged (see the module docs).
#[derive(Debug, Default)]
pub struct KeyframeLimiter {
    last_forced: Option<Instant>,
    requested_at: Option<Instant>,
    /// A request came too soon; [`Self::poll`] sends it once allowed.
    pending: bool,
}

impl KeyframeLimiter {
    /// Ask for a key frame: `true` means send the request now; otherwise it
    /// is held and [`Self::poll`] releases it.
    pub fn request(&mut self, now: Instant) -> bool {
        if self.allowed(now) {
            self.forced(now);
            true
        } else {
            self.pending = true;
            false
        }
    }

    /// A held request whose turn has come: `true` means send it now.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.pending && self.allowed(now) {
            self.pending = false;
            self.forced(now);
            true
        } else {
            false
        }
    }

    /// A key frame forced regardless of the limit (a resumed stream cannot
    /// show anything without one): it still counts, and is still tagged.
    pub fn forced(&mut self, now: Instant) {
        self.last_forced = Some(now);
        self.requested_at = Some(now);
    }

    /// Whether a key frame arriving `now` is a requested one.
    pub fn is_requested(&self, now: Instant) -> bool {
        self.requested_at.is_some_and(|at| now.saturating_duration_since(at) <= REQUESTED_WINDOW)
    }

    fn allowed(&self, now: Instant) -> bool {
        self.last_forced.is_none_or(|at| now.saturating_duration_since(at) >= MIN_FORCED_GAP)
    }
}

#[derive(Debug, Default)]
struct State {
    clock: FrameClock,
    keyframes: KeyframeLimiter,
    last_jpeg: Option<usize>,
}

/// The frame clock and key-frame gate of one session, shared by its threads.
#[derive(Debug, Default)]
pub struct FrameSignal(Mutex<State>);

impl FrameSignal {
    /// A picture arrived.
    pub fn frame(&self, now: Instant, ingress: Ingress) {
        let mut state = self.lock();
        state.clock.seq += 1;
        state.clock.last_frame_at = Some(now);
        let motion = match ingress {
            Ingress::Key { .. } => !state.keyframes.is_requested(now),
            Ingress::Delta { bytes, pixels } => match pixels.filter(|&p| p > 0) {
                Some(pixels) => {
                    let bppf = bytes as f64 * 8.0 / pixels as f64;
                    tracing::trace!(bytes, pixels, bppf, "video delta");
                    bppf > STATIC_BPPF
                }
                None => true,
            },
            Ingress::Jpeg { bytes } => {
                let moved = state.last_jpeg.is_none_or(|last| (bytes as f64 - last as f64).abs() > last as f64 * JPEG_DELTA);
                state.last_jpeg = Some(bytes);
                moved
            }
        };
        if motion {
            state.clock.last_motion_at = Some(now);
        }
    }

    /// The stream paused or resumed (see [`FrameClock::epoch`]).
    pub fn interrupted(&self) {
        self.lock().clock.epoch += 1;
    }

    /// A picture was dropped before the decoder: what it showed is unknown,
    /// so it counts as change.
    pub fn dropped(&self, now: Instant) {
        self.lock().clock.last_motion_at = Some(now);
    }

    pub fn clock(&self) -> FrameClock {
        self.lock().clock
    }

    /// See [`KeyframeLimiter::request`].
    pub fn request_key(&self, now: Instant) -> bool {
        self.lock().keyframes.request(now)
    }

    /// See [`KeyframeLimiter::poll`].
    pub fn poll_key(&self, now: Instant) -> bool {
        self.lock().keyframes.poll(now)
    }

    /// See [`KeyframeLimiter::forced`].
    pub fn forced_key(&self, now: Instant) {
        self.lock().keyframes.forced(now);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Also called from VideoToolbox's callback thread: never panic.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    /// A 1206×2622 stream (an iPhone 17 Pro at scale 1).
    const PIXELS: Option<u64> = Some(1206 * 2622);

    #[test]
    fn at_most_one_forced_key_frame_per_second() {
        let t0 = Instant::now();
        let mut limiter = KeyframeLimiter::default();
        assert!(limiter.request(t0));
        assert!(!limiter.request(t0 + 300 * MS), "too soon: held");
        assert!(!limiter.poll(t0 + 900 * MS));
        assert!(limiter.poll(t0 + 1000 * MS), "its turn: released once");
        assert!(!limiter.poll(t0 + 3000 * MS), "and only once");
        // A forced one counts against the limit too.
        limiter.forced(t0 + 3000 * MS);
        assert!(!limiter.request(t0 + 3500 * MS));
    }

    #[test]
    fn requested_key_frames_are_not_motion() {
        let t0 = Instant::now();
        let signal = FrameSignal::default();
        // Five requested IDRs, each arriving 100 ms after its request.
        for i in 0..5 {
            let asked = t0 + i * 1000 * MS;
            assert!(signal.request_key(asked));
            signal.frame(asked + 100 * MS, Ingress::Key { bytes: 400_000 });
        }
        let clock = signal.clock();
        assert_eq!((clock.seq, clock.last_motion_at), (5, None));
        // One nobody asked for is a change.
        signal.frame(t0 + 6000 * MS, Ingress::Key { bytes: 400_000 });
        assert_eq!(signal.clock().last_motion_at, Some(t0 + 6000 * MS));
    }

    #[test]
    fn small_deltas_are_quiet_and_large_ones_are_motion() {
        let t0 = Instant::now();
        let signal = FrameSignal::default();
        signal.frame(t0, Ingress::Delta { bytes: 300, pixels: PIXELS });
        assert_eq!(signal.clock().last_motion_at, None, "an idle delta");
        signal.frame(t0 + 16 * MS, Ingress::Delta { bytes: 40_000, pixels: PIXELS });
        assert_eq!(signal.clock().last_motion_at, Some(t0 + 16 * MS), "a scroll");
        signal.frame(t0 + 32 * MS, Ingress::Delta { bytes: 10, pixels: None });
        assert_eq!(signal.clock().last_motion_at, Some(t0 + 32 * MS), "no size yet: assume change");
        signal.dropped(t0 + 48 * MS);
        assert_eq!(signal.clock().last_motion_at, Some(t0 + 48 * MS));
    }

    #[test]
    fn a_jpeg_moves_when_its_size_does() {
        let t0 = Instant::now();
        let signal = FrameSignal::default();
        signal.frame(t0, Ingress::Jpeg { bytes: 100_000 });
        signal.frame(t0 + 50 * MS, Ingress::Jpeg { bytes: 100_900 });
        assert_eq!(signal.clock().last_motion_at, Some(t0), "the first, then within 2%");
        signal.frame(t0 + 100 * MS, Ingress::Jpeg { bytes: 110_000 });
        assert_eq!(signal.clock().last_motion_at, Some(t0 + 100 * MS));
    }

    #[test]
    fn quiet_counts_from_the_action_or_the_last_motion() {
        let t0 = Instant::now();
        let clock = FrameClock { seq: 3, last_frame_at: Some(t0), last_motion_at: Some(t0), epoch: 0 };
        assert!(!clock.quiet_for(t0 + 100 * MS, t0 + 300 * MS, 250 * MS), "counted from the action");
        assert!(clock.quiet_for(t0 + 100 * MS, t0 + 350 * MS, 250 * MS));
        let later = FrameClock { last_motion_at: Some(t0 + 200 * MS), ..clock };
        assert!(!later.quiet_for(t0, t0 + 400 * MS, 250 * MS));
        assert!(FrameClock::default().quiet_for(t0, t0 + 250 * MS, 250 * MS), "no pictures at all");
    }

    #[test]
    fn a_pause_or_resume_starts_a_new_epoch() {
        let signal = FrameSignal::default();
        signal.interrupted();
        signal.interrupted();
        assert_eq!(signal.clock().epoch, 2);
    }
}
