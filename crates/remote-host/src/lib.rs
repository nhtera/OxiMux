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
pub mod dispatcher;
pub mod identity;

pub use auth::{
    AppPubkey, AuthStore, DeviceInfo, DeviceStore, PairingSlot, StorageDeviceStore, StoredDevice,
    mint_pairing_secret, registration_proof,
};
pub use dispatcher::Dispatcher;
pub use identity::HostIdentity;
