//! The in-app remote-control host.
//!
//! Three concerns, all transport-agnostic (driven over the `remote-proto`
//! `Transport` seam — iroh is one driver, the in-memory loopback drives tests):
//! - [`identity`] — the host's persistent Ed25519 node identity (0600, SHA-256
//!   keyed per workspace).
//! - [`auth`] — the QR pairing proof, the two-key challenge/token handshake, and
//!   the two-level ACL, with authorization re-checkable on every RPC.
//! - [`dispatcher`] — the per-connection RPC loop that serves the Phase-2
//!   [`SessionRegistry`](oximux_agents::session_registry::SessionRegistry).
//!
//! The live iroh `Endpoint` binding + QR display, the V019 device-persistence
//! migration, and the enablement UI are later slices; this crate is the network-
//! free core they build on.

pub mod auth;
pub mod catalog;
pub mod dispatcher;
pub mod identity;
pub mod launcher;
pub mod projects;
pub mod rewind;
pub mod schedule_runner;
pub mod terminals;
pub mod transcribe;
pub mod transcript_budget;
pub mod worktrees;

pub use auth::{
    AppPubkey, AuthStore, DeviceInfo, DeviceStore, LocalScope, PairedDevice, PairingEvent,
    PairingSlot, StorageDeviceStore, StoredDevice, mint_pairing_secret, registration_proof,
};
pub use dispatcher::Dispatcher;
pub use identity::HostIdentity;
pub use launcher::{LaunchError, SessionLauncher};
pub use projects::ProjectProvider;
pub use rewind::{RewindError, RewindService};
pub use schedule_runner::{RunNowError, ScheduleRunner, TickerRunner, schedule_run_to_wire};
pub use terminals::{AttachmentId, TerminalAttach, TerminalError, TerminalFrame, TerminalSource};
pub use transcribe::{AudioTranscriber, TranscribeError};
pub use worktrees::{WorktreeError, WorktreeService};
