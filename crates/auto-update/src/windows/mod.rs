//! The Windows self-update pipeline.
//!
//! Same lifecycle as macOS — background check, verified payload staged on disk,
//! swap at quit — over a different shape of install and a different trust root.
//!
//! | | macOS | Windows |
//! |---|---|---|
//! | Install | one `.app` bundle | a directory of loose files |
//! | Artifact | `.dmg`, mounted | `.zip`, extracted |
//! | Trust root | Developer ID codesign pin | minisign over the release manifest |
//! | Swap | one `renamex_np` of the bundle | move-aside/move-in, file by file |
//! | Backups | deleted in place | deferred to the next boot (mapped images) |
//!
//! The trust root is the interesting difference. The Windows artifacts carry no
//! Authenticode signature — `scripts/bundle-windows.ps1` says so plainly — so
//! there is no publisher identity to pin an update to. What there *is* is the
//! signed release manifest the CLI already updates from: a minisign signature
//! over an inventory of every artifact's sha256, checked against a key compiled
//! into this binary, which a compromised publish token cannot reach. That is
//! the same trust root, applied to a second payload, which is why the machinery
//! for it lives in [`crate::release`] and not here.
//!
//! What is genuinely Windows-shaped, and lives here: unpacking a zip whose
//! contents legitimately vary between releases ([`archive`]), and a swap whose
//! backups cannot be deleted while this process is running out of them
//! ([`staging`]).

pub mod archive;
pub mod install;
pub mod pipeline;
pub mod staging;

pub use install::{InstalledApp, eligibility};
pub use staging::{PendingUpdate, SwapOutcome, boot_sweep};
