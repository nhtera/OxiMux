//! A device's encoded picture, copied out for a remote viewer (the paired
//! phone) without encoding it a second time.
//!
//! The session already receives H.264 for its own decoder. When somebody
//! watches from afar, each access unit is also handed to an [`EncodedTap`],
//! which passes it to every subscriber's slot ([`TapReceiver`]). Nothing is
//! copied while nobody subscribes ([`EncodedTap::wanted`]).
//!
//! A slot is small and latest-wins: a viewer that falls behind loses frames
//! rather than growing a queue, and **never gets a delta after a gap** — a P
//! frame decoded against the wrong reference shows corruption until the next
//! key frame, so after any drop the slot waits for one ([`TapReceiver::wants_key`]
//! tells the owner to ask). Each slot is also paced ([`Pacer`]): a phone is
//! fed at its own rate whatever the desktop streams at, and when the source
//! runs faster than that, only key frames get through.
//!
//! Pure and runtime-free: the owner wakes its consumer from
//! [`TapReceiver::set_waker`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapKind {
    /// An avcC record: the decoder's configuration. Precedes the key frame
    /// it describes.
    Config,
    Key,
    Delta,
}

/// One encoded frame. The payload is shared by every subscriber.
#[derive(Clone, Debug)]
pub struct TapFrame {
    pub kind: TapKind,
    /// Microseconds on the session's own clock.
    pub pts_us: u64,
    /// An avcC record (config) or an AVCC sample (4-byte lengths).
    pub data: Arc<[u8]>,
}

/// Frames a slot holds for a slow consumer before it drops them: about a
/// second at 30 fps. The consumer writes at the feed's rate, so a key frame
/// of tens of kB takes a good part of a second to go out, and the deltas
/// that arrive meanwhile must wait here rather than break the stream.
pub const SLOT_FRAMES: usize = 32;

/// A token bucket over bytes: the rate a remote viewer is fed at.
///
/// A delta passes only when the bucket holds it whole. A key frame passes
/// whenever the bucket is not in debt, and may take it into debt: a key
/// frame is what recovers a broken stream, and refusing the large ones would
/// leave a viewer on a link slower than one key frame per burst with nothing
/// at all. Over time the rate holds either way.
#[derive(Clone, Debug)]
pub struct Pacer {
    /// Bytes per second.
    rate: f64,
    /// The most the bucket holds.
    burst: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl Pacer {
    /// A pacer at `bits_per_sec`, with a second of burst. Less would not
    /// hold a key frame and the deltas right after it (a key frame is tens of
    /// kB, a quarter second at 0.8 Mbit/s is 25): every key frame would leave
    /// the bucket in debt, the next delta would be a gap, and the gap would
    /// ask for another key frame, leaving the viewer on key frames alone.
    pub fn new(bits_per_sec: u64) -> Self {
        let rate = bits_per_sec as f64 / 8.0;
        Self { rate, burst: rate, tokens: rate, last: None }
    }

    /// Whether a frame of `bytes` may go now, taking its bytes if so.
    pub fn admit(&mut self, now: Instant, kind: TapKind, bytes: usize) -> bool {
        if let Some(last) = self.last {
            self.tokens = (self.tokens + now.saturating_duration_since(last).as_secs_f64() * self.rate).min(self.burst);
        }
        self.last = Some(now);
        let bytes = bytes as f64;
        let ok = match kind {
            TapKind::Config => true,
            TapKind::Key => self.tokens >= 0.0,
            TapKind::Delta => self.tokens >= bytes,
        };
        if ok && kind != TapKind::Config {
            self.tokens -= bytes;
        }
        ok
    }
}

/// One subscriber's slot.
struct Slot {
    state: Mutex<SlotState>,
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

struct SlotState {
    queue: VecDeque<TapFrame>,
    /// A frame was lost since the last key frame (or none came yet):
    /// deltas are useless.
    need_key: bool,
    pacer: Pacer,
    /// When this viewer last asked for a key frame.
    last_ask: Option<Instant>,
}

impl Slot {
    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take `frame` if it is any use; `true` when it was queued.
    fn offer(&self, frame: &TapFrame, now: Instant) -> bool {
        let mut st = self.lock();
        match frame.kind {
            TapKind::Config => {
                // Tiny, and a new one means a new decoder: always kept, and
                // what follows must start from its key frame.
                st.queue.retain(|f| f.kind != TapKind::Config);
                st.queue.push_back(frame.clone());
                st.need_key = true;
            }
            TapKind::Key => {
                if !st.pacer.admit(now, TapKind::Key, frame.data.len()) {
                    st.need_key = true;
                    return false;
                }
                // Latest wins: a key frame makes everything queued before it
                // (but the config) pointless to send.
                st.queue.retain(|f| f.kind == TapKind::Config);
                st.queue.push_back(frame.clone());
                st.need_key = false;
            }
            TapKind::Delta => {
                if st.need_key {
                    return false;
                }
                if st.queue.len() >= SLOT_FRAMES || !st.pacer.admit(now, TapKind::Delta, frame.data.len()) {
                    // A gap: drop what is queued, and wait for a key frame.
                    st.queue.retain(|f| f.kind == TapKind::Config);
                    st.need_key = true;
                    return false;
                }
                st.queue.push_back(frame.clone());
            }
        }
        true
    }

    fn wake(&self) {
        if let Some(waker) = self.waker.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            waker();
        }
    }
}

/// A session's tap: hand it every config, and every other frame while
/// [`Self::wanted`]. Dropping it (the session ended) ends every receiver.
pub struct EncodedTap {
    shared: Arc<Shared>,
}

/// What the tap and its receivers share. A receiver never holds the session
/// itself: a phone still watching must not keep a detached device's helper
/// alive.
struct Shared {
    inner: Mutex<TapInner>,
    live: AtomicUsize,
    /// Zero on the session's frame clock ([`EncodedTap::pts_now`]).
    epoch: Instant,
    /// Asks the session's encoder for a key frame (installed by the session).
    key_request: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct TapInner {
    /// The last config seen, for a subscriber that joins mid-stream.
    config: Option<Arc<[u8]>>,
    slots: Vec<Weak<Slot>>,
    /// The session is gone: no frame will come.
    closed: bool,
}

impl Default for EncodedTap {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::default(),
                live: AtomicUsize::new(0),
                epoch: Instant::now(),
                key_request: Mutex::new(None),
            }),
        }
    }
}

impl Drop for EncodedTap {
    fn drop(&mut self) {
        let slots: Vec<Arc<Slot>> = {
            let mut inner = self.shared.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.closed = true;
            inner.slots.iter().filter_map(Weak::upgrade).collect()
        };
        for slot in slots {
            slot.wake();
        }
    }
}

impl EncodedTap {
    /// Microseconds since the tap began: a pts for a source that has none.
    pub fn pts_now(&self) -> u64 {
        u64::try_from(self.shared.epoch.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// How a receiver asks the encoder for a key frame. The session installs
    /// it, holding itself weakly.
    pub fn on_key_request(&self, request: impl Fn() + Send + Sync + 'static) {
        *self.shared.key_request.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(request));
    }

    /// Whether anyone subscribes: frames are not copied otherwise.
    pub fn wanted(&self) -> bool {
        self.shared.live.load(Ordering::Acquire) > 0
    }

    /// Hand one frame to every subscriber.
    pub fn push(&self, frame: TapFrame) {
        let now = Instant::now();
        let slots: Vec<Arc<Slot>> = {
            let mut inner = self.shared.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if frame.kind == TapKind::Config {
                // Some encoders repeat their config before every key frame;
                // the same one again would only make the viewer start over.
                if inner.config.as_deref() == Some(&*frame.data) {
                    return;
                }
                inner.config = Some(frame.data.clone());
            }
            inner.slots.retain(|s| s.strong_count() > 0);
            inner.slots.iter().filter_map(Weak::upgrade).collect()
        };
        // Offered and woken outside the tap's lock: a waker may take its
        // consumer's locks.
        for slot in slots {
            if slot.offer(&frame, now) {
                slot.wake();
            }
        }
    }

    /// The config to start a decoder with, if one has been seen.
    pub fn config(&self) -> Option<Arc<[u8]>> {
        self.shared.inner.lock().unwrap_or_else(PoisonError::into_inner).config.clone()
    }

    /// Subscribe, paced at `bits_per_sec`. The slot starts with the last
    /// config seen and wants a key frame (a viewer joins mid-stream).
    pub fn subscribe(&self, bits_per_sec: u64) -> TapReceiver {
        let mut inner = self.shared.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut queue = VecDeque::new();
        if let Some(config) = &inner.config {
            queue.push_back(TapFrame { kind: TapKind::Config, pts_us: 0, data: config.clone() });
        }
        let slot = Arc::new(Slot {
            state: Mutex::new(SlotState {
                queue,
                need_key: true,
                pacer: Pacer::new(bits_per_sec),
                last_ask: None,
            }),
            waker: Mutex::new(None),
        });
        inner.slots.push(Arc::downgrade(&slot));
        self.shared.live.fetch_add(1, Ordering::AcqRel);
        TapReceiver { slot, shared: self.shared.clone() }
    }
}

/// One subscriber's end. Dropping it unsubscribes.
pub struct TapReceiver {
    slot: Arc<Slot>,
    shared: Arc<Shared>,
}

impl TapReceiver {
    /// The next frame, if one is queued.
    pub fn try_take(&self) -> Option<TapFrame> {
        self.slot.lock().queue.pop_front()
    }

    /// The session ended: nothing more will be queued.
    pub fn is_closed(&self) -> bool {
        self.shared.inner.lock().unwrap_or_else(PoisonError::into_inner).closed
    }

    /// Called (from the session's threads) whenever a frame is queued, and
    /// once when the session ends.
    pub fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.slot.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(waker));
    }

    /// Whether a key frame should be asked for now: only while this viewer
    /// cannot decode (it just joined, or lost a frame), and at most once per
    /// `every`, so one that waits on a still screen asks again. A viewer
    /// that decodes asks for nothing: on some platforms a key frame restarts
    /// the encoder for every viewer, the desktop's own panel included.
    pub fn wants_key(&self, now: Instant, every: Duration) -> bool {
        let mut st = self.slot.lock();
        let due = st.need_key && st.last_ask.is_none_or(|at| now.saturating_duration_since(at) >= every);
        if due {
            st.last_ask = Some(now);
        }
        due
    }

    /// Ask the session's encoder for a key frame (through its limiter).
    pub fn request_key(&self) {
        let request = self.shared.key_request.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(request) = request {
            request();
        }
    }
}

impl Drop for TapReceiver {
    fn drop(&mut self) {
        self.shared.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: TapKind, len: usize) -> TapFrame {
        TapFrame { kind, pts_us: 0, data: vec![0u8; len].into() }
    }

    fn kinds(rx: &TapReceiver) -> Vec<TapKind> {
        std::iter::from_fn(|| rx.try_take()).map(|f| f.kind).collect()
    }

    /// Fast enough never to pace.
    const FAST: u64 = 1_000_000_000;

    #[test]
    fn nothing_is_copied_until_someone_subscribes() {
        let tap = EncodedTap::default();
        assert!(!tap.wanted());
        let rx = tap.subscribe(FAST);
        assert!(tap.wanted());
        drop(rx);
        assert!(!tap.wanted(), "dropping the receiver unsubscribes");
    }

    #[test]
    fn a_late_subscriber_starts_from_the_config_and_a_key_frame() {
        let tap = EncodedTap::default();
        tap.push(frame(TapKind::Config, 20));
        tap.push(frame(TapKind::Key, 100));
        let rx = tap.subscribe(FAST);
        tap.push(frame(TapKind::Delta, 10));
        tap.push(frame(TapKind::Key, 100));
        tap.push(frame(TapKind::Delta, 10));
        assert_eq!(kinds(&rx), [TapKind::Config, TapKind::Key, TapKind::Delta], "no delta before the first key frame");
    }

    #[test]
    fn no_delta_follows_a_gap() {
        let tap = EncodedTap::default();
        let rx = tap.subscribe(FAST);
        let now = Instant::now();
        assert!(rx.wants_key(now, Duration::from_secs(2)), "a new subscriber asks for a key frame");
        assert!(!rx.wants_key(now, Duration::from_secs(2)), "once per period");
        tap.push(frame(TapKind::Key, 100));
        let later = now + Duration::from_secs(5);
        assert!(!rx.wants_key(later, Duration::from_secs(2)), "a viewer that decodes asks for nothing");
        // The consumer falls behind: the slot overflows.
        for _ in 0..SLOT_FRAMES + 3 {
            tap.push(frame(TapKind::Delta, 10));
        }
        assert!(rx.wants_key(later, Duration::from_secs(2)), "the gap asks for a key frame");
        tap.push(frame(TapKind::Delta, 10));
        let got = kinds(&rx);
        assert!(got.is_empty(), "everything queued before the gap and after it is dropped: {got:?}");
        tap.push(frame(TapKind::Key, 100));
        tap.push(frame(TapKind::Delta, 10));
        assert_eq!(kinds(&rx), [TapKind::Key, TapKind::Delta]);
    }

    #[test]
    fn a_saturated_viewer_gets_key_frames_only() {
        let tap = EncodedTap::default();
        // 80 kbit/s: 10 kB/s, 10 kB of burst.
        let rx = tap.subscribe(80_000);
        tap.push(frame(TapKind::Key, 2_000));
        assert_eq!(kinds(&rx), [TapKind::Key]);
        // A source far above the rate: deltas of 1 kB, back to back. The
        // burst carries the first eight; the ninth overdraws the bucket: a
        // gap, so what is queued goes, and every later delta waits for a key
        // frame; the next key frame passes.
        for _ in 0..30 {
            tap.push(frame(TapKind::Delta, 1_000));
        }
        assert_eq!(kinds(&rx), []);
        tap.push(frame(TapKind::Key, 2_000));
        assert_eq!(kinds(&rx), [TapKind::Key]);
        // A newer key frame replaces one not yet taken (latest wins).
        tap.push(frame(TapKind::Key, 1));
        tap.push(frame(TapKind::Key, 1));
        assert_eq!(kinds(&rx).len(), 0, "the bucket is in debt now");
    }

    #[test]
    fn the_pacer_holds_its_rate() {
        // 800 kbit/s = 100 kB/s; 15 fps of 20 kB deltas is 300 kB/s.
        let mut pacer = Pacer::new(800_000);
        let start = Instant::now();
        let mut sent = 0usize;
        for i in 0..150u32 {
            let now = start + Duration::from_millis(u64::from(i) * 66);
            if pacer.admit(now, TapKind::Delta, 20_000) {
                sent += 20_000;
            }
        }
        let secs = 150.0 * 0.066;
        let rate = sent as f64 / secs;
        assert!(rate <= 100_000.0 * 1.05 + 100_000.0 / secs, "{rate} B/s");
        assert!(rate >= 100_000.0 * 0.8, "{rate} B/s: the bucket must not starve either");
    }

    #[test]
    fn a_large_key_frame_still_passes_and_is_paid_for() {
        let mut pacer = Pacer::new(80_000);
        let now = Instant::now();
        assert!(pacer.admit(now, TapKind::Key, 50_000), "bigger than the burst, but the bucket is not in debt");
        assert!(!pacer.admit(now, TapKind::Key, 50_000), "now it is");
        assert!(!pacer.admit(now + Duration::from_secs(3), TapKind::Key, 1), "still paying it off");
        assert!(pacer.admit(now + Duration::from_secs(5), TapKind::Key, 1));
    }

    #[test]
    fn a_key_frame_and_the_deltas_after_it_fit_the_phone_rate() {
        // The phone's 0.8 Mbit/s, a 44 kB key frame (an iOS one at half
        // scale), then 30 fps of 2 kB deltas: nothing is dropped, so no gap
        // asks for another key frame.
        let mut pacer = Pacer::new(800_000);
        let start = Instant::now();
        assert!(pacer.admit(start, TapKind::Key, 44_000));
        for i in 1..=90u64 {
            assert!(pacer.admit(start + Duration::from_millis(i * 33), TapKind::Delta, 2_000), "delta {i}");
        }
    }

    #[test]
    fn a_viewer_waiting_on_a_still_screen_asks_again_each_period() {
        let tap = EncodedTap::default();
        let rx = tap.subscribe(FAST);
        let t0 = Instant::now();
        assert!(rx.wants_key(t0, Duration::from_secs(2)));
        assert!(!rx.wants_key(t0 + Duration::from_millis(500), Duration::from_secs(2)));
        assert!(rx.wants_key(t0 + Duration::from_secs(3), Duration::from_secs(2)), "no key frame came: ask again");
        assert!(!rx.wants_key(t0 + Duration::from_millis(3500), Duration::from_secs(2)), "counted from the ask");
        tap.push(frame(TapKind::Key, 10));
        assert!(!rx.wants_key(t0 + Duration::from_secs(60), Duration::from_secs(2)), "decoding: never again");
    }

    #[test]
    fn a_repeated_config_is_not_passed_on() {
        let tap = EncodedTap::default();
        let rx = tap.subscribe(FAST);
        tap.push(frame(TapKind::Config, 20));
        tap.push(frame(TapKind::Key, 100));
        tap.push(frame(TapKind::Config, 20));
        tap.push(frame(TapKind::Delta, 10));
        assert_eq!(kinds(&rx), [TapKind::Config, TapKind::Key, TapKind::Delta]);
        tap.push(frame(TapKind::Config, 21));
        assert_eq!(kinds(&rx), [TapKind::Config], "a new one is");
    }

    #[test]
    fn a_receiver_learns_the_session_ended_and_can_ask_for_a_key_frame() {
        let tap = EncodedTap::default();
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        tap.on_key_request(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        let rx = tap.subscribe(FAST);
        let woken = Arc::new(AtomicUsize::new(0));
        let count = woken.clone();
        rx.set_waker(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        rx.request_key();
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert!(!rx.is_closed());
        drop(tap);
        assert!(rx.is_closed(), "the session's tap is gone");
        assert_eq!(woken.load(Ordering::SeqCst), 1, "and the consumer is told");
        rx.request_key(); // still callable: the hook outlives the tap harmlessly
    }

    #[test]
    fn the_waker_fires_only_for_a_kept_frame() {
        let tap = EncodedTap::default();
        let rx = tap.subscribe(FAST);
        let woken = Arc::new(AtomicUsize::new(0));
        let count = woken.clone();
        rx.set_waker(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        tap.push(frame(TapKind::Delta, 10)); // no key frame yet: dropped
        tap.push(frame(TapKind::Key, 10));
        assert_eq!(woken.load(Ordering::SeqCst), 1);
    }
}
