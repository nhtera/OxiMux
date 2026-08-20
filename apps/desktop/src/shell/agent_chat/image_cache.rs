//! A byte-bounded, evicting cache for chat attachment images.
//!
//! Every image a user has ever attached to a prompt in this tab used to stay
//! resident for the view's lifetime: the map was populated with
//! `entry(..).or_insert_with(..)` and had no `remove`, `clear` or `retain`
//! anywhere in the crate. Scrolling back through a long image-heavy chat was a
//! one-way ratchet.
//!
//! What that costs is easy to under-count, because this map holds the *encoded*
//! bytes — the same PNG/JPEG the transcript already carries as base64 — and the
//! expensive copy lives elsewhere. Rendering an `Arc<Image>` makes GPUI decode
//! it into an `Arc<RenderImage>` (raw RGBA, held in the app's asset cache) and
//! upload a sprite-atlas tile on the GPU. A 4K screenshot is ~2MB encoded and
//! ~33MB decoded. Dropping our `Arc` alone would reclaim the small half and
//! leave both large ones behind, which is why eviction here goes through
//! [`release`] rather than simply forgetting the key.
//!
//! Recency is a frame ordinal rather than a clock: the only ordering question
//! is "which of these was read least recently", `Instant::now()` per lookup
//! would be a syscall on the render path, and a counter cannot be perturbed by
//! the machine sleeping.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, Image, Window};

/// How many bytes of *encoded* image the cache may hold before it evicts.
///
/// Generous on purpose. The failure mode of a tight budget is thrashing —
/// scrolling past an image evicts it, scrolling back re-decodes it — and a
/// re-decode costs far more than the bytes it saves, so bounded-and-generous
/// beats tight-and-correct here. It bounds the encoded half directly and the
/// decoded half by proxy, since a decoded image cannot outlive the `Arc<Image>`
/// that keys it.
pub(super) const BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// `(entry index, image index within that entry)` — the stable position a
/// streaming repaint keeps hitting, which is what makes memoizing worthwhile.
type Key = (usize, usize);

struct Slot {
    /// `None` records an attachment that could not be decoded. Kept rather than
    /// omitted so a repaint does not retry a decode that has already failed
    /// once; these carry no bytes and are never evicted, and their number is
    /// bounded by how many undecodable images one conversation contains.
    image: Option<Arc<Image>>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<Key, Slot>,
    /// Sum of `Slot::bytes`, maintained incrementally — recomputing it per
    /// insert would make the render path O(images).
    bytes: usize,
    clock: u64,
}

/// The cache. `RefCell` because the render path holds `&self`.
#[derive(Default)]
pub(super) struct ImageCache {
    inner: RefCell<Inner>,
}

impl ImageCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The memoized decode for one attachment, decoding on first ask.
    ///
    /// Deliberately takes no `App`: this runs while the transcript is building
    /// its children, and eviction needs a `Window` that is not available there.
    /// Insertion may therefore push the cache over budget; [`Self::evict`]
    /// brings it back at the top of the next frame. Overshooting by one frame's
    /// worth of newly-visible images is the cheap direction to be wrong in.
    pub(super) fn get_or_decode(
        &self,
        key: Key,
        decode: impl FnOnce() -> Option<Arc<Image>>,
    ) -> Option<Arc<Image>> {
        let mut inner = self.inner.borrow_mut();
        inner.clock += 1;
        let now = inner.clock;

        if let Some(slot) = inner.slots.get_mut(&key) {
            slot.used = now;
            return slot.image.clone();
        }

        let image = decode();
        let bytes = image.as_ref().map_or(0, |i| i.bytes.len());
        inner.bytes += bytes;
        inner.slots.insert(key, Slot { image: image.clone(), bytes, used: now });
        image
    }

    /// Drop least-recently-used images until the cache is back within budget,
    /// releasing each one's decoded bitmap and atlas tile on the way out.
    ///
    /// Called once per frame rather than per insert, because releasing needs a
    /// `Window` and the insert path has none.
    pub(super) fn evict(&self, window: &mut Window, cx: &mut App) {
        // Victims are chosen and the borrow released *before* anything touches
        // GPUI. `release` reaches into the asset cache and the sprite atlas,
        // and holding a `RefCell` borrow across a call that broad is how a
        // re-entrant panic gets written.
        let victims = {
            let mut inner = self.inner.borrow_mut();
            let mut victims = Vec::new();
            while inner.bytes > BUDGET_BYTES {
                let Some(key) = inner
                    .slots
                    .iter()
                    .filter(|(_, s)| s.image.is_some())
                    .min_by_key(|(_, s)| s.used)
                    .map(|(k, _)| *k)
                else {
                    // Over budget with nothing evictable left. Impossible while
                    // bytes are only ever attributed to slots holding an image,
                    // but looping forever would be the worse way to find that
                    // out.
                    break;
                };
                if let Some(slot) = inner.slots.remove(&key) {
                    inner.bytes = inner.bytes.saturating_sub(slot.bytes);
                    if let Some(image) = slot.image {
                        victims.push(image);
                    }
                }
            }
            victims
        };

        for image in victims {
            release(image, window, cx);
        }
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.inner.borrow().bytes
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.borrow().slots.len()
    }
}

/// Give back everything one image is holding: the GPU atlas tile, then the
/// decoded bitmap in the asset cache, then (as the `Arc` drops) the encoded
/// bytes.
///
/// The order matters. `drop_image` needs the `Arc<RenderImage>`, and the only
/// public way to reach it is to ask the asset cache for it — so it has to
/// happen before `remove_asset` evicts the entry that answer comes from.
///
/// `get_render_image` returning `None` means this image was never actually
/// painted (cached on a frame that built the element but never reached the
/// GPU), so there is no tile to free and skipping is correct.
fn release(image: Arc<Image>, window: &mut Window, cx: &mut App) {
    if let Some(rendered) = image.clone().get_render_image(window, cx) {
        cx.drop_image(rendered, Some(window));
    }
    image.remove_asset(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ImageFormat;

    fn image(bytes: usize) -> Option<Arc<Image>> {
        Some(Arc::new(Image::from_bytes(ImageFormat::Png, vec![0u8; bytes])))
    }

    #[test]
    fn decodes_once_and_memoizes() {
        let cache = ImageCache::new();
        let mut decodes = 0;
        for _ in 0..5 {
            cache.get_or_decode((0, 0), || {
                decodes += 1;
                image(16)
            });
        }
        assert_eq!(decodes, 1, "a repaint must not re-decode");
        assert_eq!(cache.bytes(), 16);
    }

    /// The negative entry is the point: a failed decode must not be retried on
    /// every repaint, which is what an `Option`-valued slot buys over simply
    /// not inserting.
    #[test]
    fn undecodable_is_remembered_and_costs_nothing() {
        let cache = ImageCache::new();
        let mut decodes = 0;
        for _ in 0..3 {
            let got = cache.get_or_decode((0, 0), || {
                decodes += 1;
                None
            });
            assert!(got.is_none());
        }
        assert_eq!(decodes, 1, "a failed decode must not be retried");
        assert_eq!(cache.bytes(), 0, "a negative entry carries no bytes");
    }

    #[test]
    fn accounts_bytes_across_entries() {
        let cache = ImageCache::new();
        cache.get_or_decode((0, 0), || image(100));
        cache.get_or_decode((0, 1), || image(50));
        cache.get_or_decode((7, 0), || image(25));
        assert_eq!(cache.bytes(), 175);
        assert_eq!(cache.len(), 3);
    }

    /// Recency ordering, verified without a `Window` — the eviction *choice* is
    /// pure bookkeeping and testable; only the `release` call needs GPUI, and
    /// this asserts the half that picks victims.
    #[test]
    fn least_recently_used_is_the_first_victim() {
        let cache = ImageCache::new();
        cache.get_or_decode((0, 0), || image(10));
        cache.get_or_decode((0, 1), || image(10));
        cache.get_or_decode((0, 2), || image(10));
        // Re-read the oldest two, leaving (0, 1) as least-recently-used.
        cache.get_or_decode((0, 0), || unreachable!("already cached"));
        cache.get_or_decode((0, 2), || unreachable!("already cached"));

        let mut inner = cache.inner.borrow_mut();
        let victim = inner
            .slots
            .iter()
            .filter(|(_, s)| s.image.is_some())
            .min_by_key(|(_, s)| s.used)
            .map(|(k, _)| *k);
        assert_eq!(victim, Some((0, 1)));

        // And removing it puts the accounting back, which is what keeps the
        // budget loop from spinning.
        let slot = inner.slots.remove(&(0, 1)).expect("victim present");
        inner.bytes -= slot.bytes;
        assert_eq!(inner.bytes, 20);
    }
}
