//! The verify loop's wire types (v27): what `--settle`, `ax -i`, `@eN` refs,
//! `wait` and `assert` send and get back.
//!
//! Re-exported from [`crate::simulator`]; they live here so that file stays
//! small. Every type is new in v27, so its shape is fixed from its first
//! release: postcard encodes structs by field position and enums by ordinal,
//! and these sit inside `Vec`s an older peer may already decode. Grow by
//! minting a new type, never by adding a field or a variant here.

use std::cell::Cell;

use serde::{Deserialize, Deserializer, Serialize};

use crate::simulator::{SimAxNodeWire, SimCmdWire, SimErrorWire, SimPointWire, SimTargetWire};

/// How deep verbs may nest: a caller's envelope around a settle around the
/// verb. The only self-recursive types on the wire, decoded before anyone is
/// authenticated: without a bound, a small frame of nested envelopes would
/// overflow the decoder's stack.
pub const MAX_NESTING: u32 = 2;

thread_local! {
    static NESTING: Cell<u32> = const { Cell::new(0) };
}

/// Decode a nested verb, refusing past [`MAX_NESTING`] before descending (so
/// the stack stays bounded). The bytes are those of a plain `Box`.
pub(crate) fn nested<'de, D: Deserializer<'de>>(d: D) -> Result<Box<SimCmdWire>, D::Error> {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            NESTING.with(|n| n.set(n.get().saturating_sub(1)));
        }
    }
    let depth = NESTING.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let _leave = Leave;
    if depth > MAX_NESTING {
        return Err(serde::de::Error::custom("simulator verbs nest at most twice"));
    }
    Box::<SimCmdWire>::deserialize(d)
}

/// What [`SimCmdWire::Assert`] checks about the element it names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimCheckWire {
    /// It is on screen.
    Exists,
    /// It is not on screen.
    Gone,
    /// Its value (a field's text), or its label when it has no value, is
    /// exactly this.
    Text(String),
    /// It is on screen and enabled.
    Enabled,
}

/// How an action's wait for the screen to go idle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimSettleOutcomeWire {
    /// The screen stopped changing and two accessibility reads agreed (iOS
    /// simulator).
    Settled,
    /// The screen stopped changing; one accessibility read followed (Android
    /// and a real iPhone, where a read takes a second or more).
    SettledFrame,
    /// The cap ran out while the screen kept changing (a spinner, a video).
    FrameBusy,
    /// The cap ran out while the screen was still but the tree kept changing.
    AxUnstable,
}

/// Which way a [`SimDiffLineWire`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimDiffOpWire {
    Added,
    Removed,
}

/// One element that appeared or went away (a changed value is one of each).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimDiffLineWire {
    pub op: SimDiffOpWire,
    /// Its `@eN` ref, for an element that can be acted on.
    pub r: Option<u32>,
    pub node: SimAxNodeWire,
}

/// [`SimReplyWire::Settled`](crate::simulator::SimReplyWire::Settled): what an
/// action did and, when asked to settle, how the screen changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimSettleWire {
    /// `None` when the action was not asked to settle (a bare `tap @e7`).
    pub outcome: Option<SimSettleOutcomeWire>,
    /// From the action's start to the end of the wait.
    pub elapsed_ms: u32,
    pub diff: Vec<SimDiffLineWire>,
    /// Elements present before and after, unchanged.
    pub unchanged: u32,
    /// `moved` (a ref's element had moved; it was tapped where it is now),
    /// and anything else the caller should hear without it failing.
    pub warnings: Vec<String>,
    /// The element a tap aimed at (by label, identifier or ref).
    pub target: Option<SimAxNodeWire>,
    /// Where a tap landed, in points.
    pub at: Option<SimPointWire>,
}

/// One element `ax -i` lists, with the ref that names it from now on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRefNodeWire {
    pub r: u32,
    pub node: SimAxNodeWire,
}

impl SimCmdWire {
    /// Whether this is a v27 verb, or carries one: only those may be answered
    /// with the v27 errors (see [`SimErrorWire::is_verify_error`]).
    pub fn uses_verify(&self) -> bool {
        match self {
            Self::Settled { .. } | Self::AxRefs { .. } | Self::Wait { .. } | Self::Assert { .. } | Self::AsCaller { .. } => true,
            Self::Tap(target) => matches!(target, SimTargetWire::Ref(_)),
            Self::Status
            | Self::Devices
            | Self::Attach { .. }
            | Self::Detach
            | Self::Screenshot { .. }
            | Self::Ax { .. }
            | Self::Swipe { .. }
            | Self::Type { .. }
            | Self::Button(_)
            | Self::Rotate(_)
            | Self::Launch { .. }
            | Self::OpenUrl { .. }
            | Self::Install { .. }
            | Self::Shutdown { .. } => false,
        }
    }
}

impl SimErrorWire {
    /// The errors a v26 decoder does not know: never sent for a pre-v27 verb.
    pub fn is_verify_error(&self) -> bool {
        matches!(self, Self::StaleRef(_) | Self::AssertFailed(_) | Self::TimedOut(_))
    }

    /// This error as `cmd` may be answered: a v27 error for an older verb
    /// becomes [`SimErrorWire::Failed`] with the same words.
    pub fn for_verb(self, cmd: &SimCmdWire) -> Self {
        if self.is_verify_error() && !cmd.uses_verify() { Self::Failed(self.to_string()) } else { self }
    }
}
