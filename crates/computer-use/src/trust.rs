//! User-anchored trust for a driver binary no publisher will vouch for.
//!
//! # Why this exists alongside [`crate::verify`]
//!
//! [`crate::verify`] gates the driver on Apple code signing: publisher identity,
//! pinned team ID, stapled notarization ticket. That is the strongest control
//! available and it is the one macOS uses.
//!
//! It has no Windows counterpart. Every Windows binary upstream publishes is
//! `NotSigned` — `cua-driver.exe`, `cua-driver-uia.exe`, `cua_driver_sdk.dll` —
//! there is no GitHub build provenance to fall back on (the attestations
//! endpoint 404s), and their own `install.ps1` verifies neither a checksum nor a
//! signature. There is no subject to pin.
//!
//! So Windows cannot ask "did the publisher sign this?" and gets the only other
//! question that has an answer: **"are these the exact bytes the user
//! approved?"**
//!
//! # What this buys, stated precisely
//!
//! It is trust-on-first-use, anchored on a human rather than a certificate
//! authority. Be exact about the two halves, because overselling this is how it
//! becomes worse than nothing:
//!
//! - **It does not establish identity.** Nothing here can tell an authentic
//!   `cua-driver.exe` from a hostile one the user was tricked into installing.
//!   If the first approval is of a bad binary, this pins the bad binary. Any UI
//!   built on it must say "unverified publisher" and must not say "verified".
//! - **It does establish continuity.** Once approved, the bytes cannot change
//!   without the user being asked again. That is the realistic threat against a
//!   long-lived install — something rewriting the binary in place months later,
//!   which is precisely what an unsigned auto-updating tool invites.
//!
//! The trust decision moves to the person who can actually make it: they choose
//! the install route, and OxiMux enforces what they chose.
//!
//! # The pin is on the bytes, not the path
//!
//! Moving an approved driver keeps it approved; rewriting one in place does not.
//! That is the correct axis when there is no publisher — the bytes *are* the
//! identity, so they are what gets pinned.
//!
//! The cost is real and worth stating plainly: **the driver self-updates**
//! (`update --apply` rewrites the binary in place), and upstream ships roughly
//! six releases a week. Every one of those invalidates the pin and asks the user
//! again. That is not a flaw to be smoothed over — with no publisher signature,
//! a new binary genuinely is a new trust decision, and quietly accepting it
//! would discard the only guarantee this module provides. What the UI may do is
//! make re-approval one click; what it may not do is skip it.
//!
//! # Never execute what has not been approved
//!
//! [`TrustStore::evaluate`] reads and hashes the file. It does not run it, and
//! callers must not run it either until the verdict is [`Trust::Approved`].
//! This is why the version floor is checked *after* approval rather than before:
//! reading `--version` means executing the binary, which is the one thing an
//! unapproved binary must not get to do. The approval prompt therefore shows a
//! path, a size, and a hash — never a version, because learning the version
//! costs exactly what the prompt exists to withhold.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;

/// Sits beside the grant store in the app data directory.
pub const TRUST_FILE_NAME: &str = "computer-use-trust.json";

/// What the user approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    /// Lowercase hex SHA-256 of the approved bytes. The pin itself.
    pub sha256: String,
    /// Where the binary was when approved. Recorded for the settings pane and
    /// for bug reports; deliberately **not** part of the comparison, so moving
    /// an approved driver does not revoke it.
    pub path: String,
    /// Seconds since the epoch.
    pub approved_at: u64,
}

impl Pin {
    /// When this was approved, or [`UNIX_EPOCH`] for a clock that predates it.
    pub fn approved_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.approved_at)
    }
}

/// The on-disk store. One driver, one pin.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    pin: Option<Pin>,
}

/// Where a candidate binary stands with the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Never approved. The hash is carried so the prompt can show it.
    Unapproved { sha256: String },
    /// Approved, and the bytes on disk still match.
    Approved { sha256: String, approved_at: SystemTime },
    /// Approved once, and the bytes have changed since. Refuse and re-ask.
    ///
    /// Distinct from [`Trust::Unapproved`] because the two mean opposite things
    /// to a user: one is "you have not set this up yet", the other is "something
    /// rewrote the binary you approved". Collapsing them would hide the only
    /// case this module exists to catch.
    Superseded {
        approved: String,
        found: String,
        approved_at: SystemTime,
    },
}

/// The pin store, addressed by path so tests and callers share one type.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The conventional location, beside the grant store.
    pub fn for_app_data_dir(app_data_dir: impl AsRef<Path>) -> Self {
        Self::at(app_data_dir.as_ref().join(TRUST_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hash `binary` and say where it stands. Does **not** execute it.
    pub fn evaluate(&self, binary: &Path) -> Result<Trust, Error> {
        let found = sha256_of(binary)?;
        let pin = self.pinned()?;
        Ok(match pin {
            None => Trust::Unapproved { sha256: found },
            Some(pin) if pin.sha256 == found => Trust::Approved {
                sha256: found,
                approved_at: pin.approved_at(),
            },
            Some(pin) => Trust::Superseded {
                approved: pin.sha256.clone(),
                found,
                approved_at: pin.approved_at(),
            },
        })
    }

    /// Record the user's approval of the bytes currently at `binary`.
    ///
    /// Re-hashes rather than taking a caller-supplied digest: the value shown in
    /// the prompt and the value pinned must come from the same read of the same
    /// file, or a binary swapped between prompt and click would be pinned under
    /// the hash the user actually saw.
    ///
    /// That still leaves a window — the file can change between this hash and
    /// the next `evaluate`. It cannot be closed from here without holding the
    /// file open across the user's decision, and it does not need to be: the
    /// next evaluate catches the swap and reports [`Trust::Superseded`].
    pub fn approve(&self, binary: &Path) -> Result<Pin, Error> {
        let sha256 = sha256_of(binary)?;
        let pin = Pin {
            sha256,
            path: binary.display().to_string(),
            approved_at: epoch_seconds(SystemTime::now()),
        };
        self.with_locked(|store| store.pin = Some(pin.clone()))?;
        Ok(pin)
    }

    /// Forget the approval. Returns whether anything was pinned.
    pub fn revoke(&self) -> Result<bool, Error> {
        self.with_locked(|store| store.pin.take().is_some())
    }

    /// The current pin, for the settings pane. `None` when nothing is approved.
    pub fn pinned(&self) -> Result<Option<Pin>, Error> {
        self.with_locked(|store| store.pin.clone())
    }

    /// Read-modify-write under a whole-file exclusive lock.
    ///
    /// Mirrors [`crate::grants::GrantTable`]'s store deliberately, including
    /// writing in place rather than temp-then-rename: a rename swaps out the
    /// file the lock is attached to, so concurrent holders would lock different
    /// inodes. Every access is on the handle that owns the lock, which is what
    /// keeps it correct under Windows' mandatory byte-range locks.
    ///
    /// A corrupt store reads as empty, which means "nothing approved" — the
    /// direction that asks the user again rather than the one that admits an
    /// unapproved binary.
    fn with_locked<T>(&self, f: impl FnOnce(&mut Store) -> T) -> Result<T, Error> {
        self.locked_io(f).map_err(|source| Error::TrustStoreUnusable {
            path: self.path.clone(),
            source,
        })
    }

    fn locked_io<T>(&self, f: impl FnOnce(&mut Store) -> T) -> std::io::Result<T> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;

        let mut lock = fd_lock::RwLock::new(file);
        let mut file = lock.write()?;

        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        let mut store: Store = serde_json::from_str(&raw).unwrap_or_default();

        let before = store.pin.clone();
        let outcome = f(&mut store);

        if store.pin != before {
            let payload = serde_json::to_string(&store).map_err(std::io::Error::other)?;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(payload.as_bytes())?;
            file.flush()?;
        }
        Ok(outcome)
    }
}

/// Lowercase hex SHA-256 of a file's contents.
///
/// Shared with [`crate::verify`], which reports it as an audit trail on macOS.
/// Here it is the gate itself.
pub fn sha256_of(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Spawn {
        program: path.display().to_string(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn epoch_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store and a binary in one throwaway directory.
    struct Fixture {
        _dir: tempfile::TempDir,
        store: TrustStore,
        binary: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = TrustStore::for_app_data_dir(dir.path());
            let binary = dir.path().join("cua-driver.exe");
            std::fs::write(&binary, b"MZ original bytes").expect("write");
            Self {
                _dir: dir,
                store,
                binary,
            }
        }

        fn rewrite(&self, bytes: &[u8]) {
            std::fs::write(&self.binary, bytes).expect("rewrite");
        }
    }

    #[test]
    fn an_unknown_binary_is_unapproved_and_carries_its_hash() {
        // The hash has to come back with the verdict: the prompt shows it, and
        // a second read to get it is a second chance to read different bytes.
        let f = Fixture::new();
        let Trust::Unapproved { sha256 } = f.store.evaluate(&f.binary).expect("evaluate") else {
            panic!("a store with no pin must report Unapproved");
        };
        assert_eq!(sha256.len(), 64);
        assert_eq!(sha256, sha256_of(&f.binary).expect("hash"));
    }

    #[test]
    fn approving_pins_the_bytes_and_the_next_look_agrees() {
        let f = Fixture::new();
        let pin = f.store.approve(&f.binary).expect("approve");

        let Trust::Approved { sha256, .. } = f.store.evaluate(&f.binary).expect("evaluate") else {
            panic!("the bytes just approved must evaluate as Approved");
        };
        assert_eq!(sha256, pin.sha256);
    }

    #[test]
    fn rewriting_an_approved_binary_is_superseded_not_unapproved() {
        // The distinction this module exists for. "Not set up yet" and
        // "something replaced the binary you approved" are opposite messages,
        // and only one of them is alarming.
        let f = Fixture::new();
        let pin = f.store.approve(&f.binary).expect("approve");
        f.rewrite(b"MZ something else entirely");

        let Trust::Superseded {
            approved, found, ..
        } = f.store.evaluate(&f.binary).expect("evaluate")
        else {
            panic!("changed bytes under a live pin must report Superseded");
        };
        assert_eq!(approved, pin.sha256);
        assert_ne!(found, pin.sha256);
    }

    #[test]
    fn an_in_place_update_of_identical_bytes_stays_approved() {
        // Rewriting the same content is not a trust event — otherwise a
        // touch-and-rewrite install step would revoke for no reason.
        let f = Fixture::new();
        f.store.approve(&f.binary).expect("approve");
        f.rewrite(b"MZ original bytes");

        assert!(matches!(
            f.store.evaluate(&f.binary).expect("evaluate"),
            Trust::Approved { .. }
        ));
    }

    #[test]
    fn the_pin_follows_the_bytes_rather_than_the_path() {
        // Stated as a test because it is a deliberate design choice, not a
        // side effect: with no publisher, the bytes are the identity.
        let f = Fixture::new();
        f.store.approve(&f.binary).expect("approve");

        let moved = f.binary.with_file_name("cua-driver-moved.exe");
        std::fs::copy(&f.binary, &moved).expect("copy");

        assert!(matches!(
            f.store.evaluate(&moved).expect("evaluate"),
            Trust::Approved { .. }
        ));
    }

    #[test]
    fn revoking_returns_to_unapproved() {
        let f = Fixture::new();
        f.store.approve(&f.binary).expect("approve");
        assert!(f.store.revoke().expect("revoke"), "had a pin to drop");
        assert!(!f.store.revoke().expect("revoke"), "second revoke is a no-op");
        assert!(matches!(
            f.store.evaluate(&f.binary).expect("evaluate"),
            Trust::Unapproved { .. }
        ));
    }

    #[test]
    fn re_approving_replaces_the_pin_rather_than_accumulating() {
        // The re-approval path after an upstream update. One driver, one pin.
        let f = Fixture::new();
        let first = f.store.approve(&f.binary).expect("approve");
        f.rewrite(b"MZ version two");
        let second = f.store.approve(&f.binary).expect("re-approve");

        assert_ne!(first.sha256, second.sha256);
        assert_eq!(
            f.store.pinned().expect("pinned").map(|p| p.sha256),
            Some(second.sha256)
        );
    }

    #[test]
    fn a_corrupt_store_reads_as_nothing_approved() {
        // Fails towards asking the user, never towards admitting a binary.
        let f = Fixture::new();
        f.store.approve(&f.binary).expect("approve");
        std::fs::write(f.store.path(), b"{ this is not json").expect("corrupt");

        assert!(matches!(
            f.store.evaluate(&f.binary).expect("evaluate"),
            Trust::Unapproved { .. }
        ));
    }

    #[test]
    fn a_pin_survives_a_round_trip_through_the_file() {
        // Two independent handles, as OxiMux and a later process would be.
        let f = Fixture::new();
        f.store.approve(&f.binary).expect("approve");

        let reopened = TrustStore::at(f.store.path());
        let pin = reopened.pinned().expect("pinned").expect("some pin");
        assert_eq!(pin.sha256, sha256_of(&f.binary).expect("hash"));
        assert_eq!(pin.path, f.binary.display().to_string());
    }

    #[test]
    fn a_missing_binary_is_an_error_rather_than_a_verdict() {
        // "Cannot be read" must not collapse into "not approved" — the caller
        // needs to tell a missing install from an unapproved one.
        let f = Fixture::new();
        let err = f
            .store
            .evaluate(&f.binary.with_file_name("absent.exe"))
            .expect_err("must fail");
        assert!(matches!(err, Error::Spawn { .. }), "got {err:?}");
    }

    #[test]
    fn evaluating_never_executes_the_candidate() {
        // The load-bearing property of this module: an unapproved binary must
        // not get to run. A file with no execute permission and no valid image
        // would fail to spawn, so a clean verdict proves nothing was spawned.
        let f = Fixture::new();
        f.rewrite(b"not a valid executable image at all");
        assert!(matches!(
            f.store.evaluate(&f.binary).expect("evaluate"),
            Trust::Unapproved { .. }
        ));
    }
}
