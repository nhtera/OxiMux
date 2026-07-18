//! The client's app-signing identity: an Ed25519 key **decoupled from the
//! transport key**. The host authorizes (and can revoke) this public key; the
//! private key never leaves the device. `mobile-core` persists the 32-byte seed
//! in the OS keystore and rebuilds the signer via [`ClientSigner::from_seed`].

use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;
use rand::rngs::OsRng;

/// The app-signing keypair. Signs the reconnect challenge nonce; its public key
/// is what the host records as the paired device.
pub struct ClientSigner {
    signing: SigningKey,
}

impl ClientSigner {
    /// A fresh random identity. Seeds from the OS RNG directly — ed25519-dalek
    /// 2.x gates `SigningKey::generate` behind a `rand_core` feature we don't pull
    /// in, and `fill_bytes` + `from_bytes` is the same 32 bytes of entropy.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        Self::from_seed(&seed)
    }

    /// Rebuild from a persisted 32-byte seed (the keystore round-trip).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(seed) }
    }

    /// The 32-byte seed to persist. A secret — never logged, never sent.
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// The app-signing public key the host authorizes by.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Sign a challenge nonce; returns the 64-byte Ed25519 signature.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }
}
