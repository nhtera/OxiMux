//! Wire types for the host's own settings (v39): the caps, flags and clocks
//! both hosts enforce, read and written over the protocol instead of behind
//! the host's back.
//!
//! **Why this is a verb and not a database write.** A running host reads its
//! [`HostConfig`] from a `watch` channel set when its core was built, so a
//! client that wrote the settings rows directly would change nothing until
//! that host restarted — while reporting success. The host validates,
//! persists and applies, in that order, and answers with what it now
//! enforces.
//!
//! [`HostConfig`]: https://docs.rs/oximux-remote-host

use serde::{Deserialize, Serialize};

/// One setting as the host enforces it now.
///
/// `is_default` is carried from the first version of this type, because
/// without it the answer cannot tell "set to 8" from "8 because nobody set
/// it" — the first question anyone debugging a cap asks. A postcard type is
/// frozen once it ships, so a field a later version wishes it had is a field
/// this one has to have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfigEntryWire {
    /// The settings key, as both hosts' loaders spell it.
    pub key: String,
    /// The value in force, **after** validation and clamping — not what was
    /// asked for. A cap that silently became something else is how a person
    /// ends up believing a limit they do not have.
    pub value: String,
    /// Nothing is stored for this key: the value is the shipped default.
    pub is_default: bool,
}
