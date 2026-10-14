//! Who is looking at a device, and which stream quality that asks for.
//!
//! A worktree's device can be on screen in several places at once: the
//! panel of a window whose active tab is that worktree, a tile in another
//! window's grid, and (later) the paired phone. Each is a viewer, keyed by
//! the worktree it shows and a [`ViewerId`]. The [`super::registry`] keeps
//! one map of them: a device with at least one *shown* viewer is never
//! paused, and a device with none pauses and later parks.
//!
//! [`arbitrate`] picks the stream quality: the best any shown viewer asks
//! for. Pure, like the registry.

/// One viewer of a worktree's device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewerId {
    /// A window's panel showing its active worktree (numbered by the app,
    /// one per window: two windows on one worktree are two viewers).
    Panel(u32),
    /// A grid tile (numbered by the app, unique for the run).
    Tile(u32),
    /// A remote view (numbered by its subscription).
    Remote(u64),
}

/// What quality a viewer wants the source to stream at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StreamProfile {
    /// Fewer pixels and frames (an unfocused tile).
    Reduced,
    /// The user's stream settings.
    #[default]
    Full,
}

/// A viewer's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewer {
    /// On screen now (a tile of a hidden panel is not).
    pub shown: bool,
    pub profile: StreamProfile,
}

/// The profile a device should stream at: the best a shown viewer asks
/// for, or `None` when nobody shows it (it is paused then, and the
/// profile it last had stays).
pub fn arbitrate<'a>(viewers: impl IntoIterator<Item = &'a Viewer>) -> Option<StreamProfile> {
    viewers.into_iter().filter(|v| v.shown).map(|v| v.profile).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(shown: bool, profile: StreamProfile) -> Viewer {
        Viewer { shown, profile }
    }

    #[test]
    fn the_best_shown_request_wins() {
        use StreamProfile::*;
        assert_eq!(arbitrate(&[v(true, Reduced), v(true, Full)]), Some(Full));
        assert_eq!(arbitrate(&[v(true, Reduced), v(true, Reduced)]), Some(Reduced));
        assert_eq!(arbitrate(&[v(true, Reduced), v(false, Full)]), Some(Reduced), "a hidden viewer asks for nothing");
        assert_eq!(arbitrate(&[v(false, Full)]), None);
        assert_eq!(arbitrate(&[]), None);
    }
}
