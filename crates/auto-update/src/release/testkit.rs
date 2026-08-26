//! Test-only: mint real minisign keypairs and signatures.
//!
//! The verifier is a third-party crate, so testing it against fixtures it
//! produced itself would prove nothing. This mints signatures the way
//! `minisign -S` does — prehashed Blake2b-512, plus the second signature over
//! the trusted comment — so the gates are exercised against the actual wire
//! format the release workflow will publish.
//!
//! Layout of what this writes, from the minisign spec:
//!
//! ```text
//! public key  = "Ed" ‖ key_id[8] ‖ pubkey[32]                      (base64)
//! signature   = "ED" ‖ key_id[8] ‖ Ed25519(sk, Blake2b512(msg))[64] (base64)
//! global sig  = Ed25519(sk, signature[64] ‖ trusted_comment)[64]    (base64)
//! ```

use base64::Engine as _;
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signer as _, SigningKey};
use rand::RngCore as _;

/// The trusted comment the fixtures carry. Tests rewrite a byte of it to prove
/// the global signature actually covers it.
const TRUSTED_COMMENT: &str = "timestamp:1\tfile:manifest.json";

pub struct MinisignKeypair {
    signing: SigningKey,
    key_id: [u8; 8],
}

impl MinisignKeypair {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let mut key_id = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut key_id);
        Self { signing: SigningKey::from_bytes(&seed), key_id }
    }

    /// The one line `packaging/release-pubkey.txt` holds — the base64 body of
    /// a `.pub` file, without its comment header.
    pub fn public_key_base64(&self) -> String {
        let mut raw = Vec::with_capacity(42);
        raw.extend_from_slice(b"Ed");
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(&self.signing.verifying_key().to_bytes());
        b64(&raw)
    }

    /// A complete `.minisig` file for `message`.
    pub fn sign(&self, message: &[u8]) -> String {
        let prehash = Blake2b512::digest(message);
        let signature = self.signing.sign(prehash.as_slice()).to_bytes();

        let mut sig_line = Vec::with_capacity(74);
        sig_line.extend_from_slice(b"ED");
        sig_line.extend_from_slice(&self.key_id);
        sig_line.extend_from_slice(&signature);

        let mut global_input = Vec::with_capacity(64 + TRUSTED_COMMENT.len());
        global_input.extend_from_slice(&signature);
        global_input.extend_from_slice(TRUSTED_COMMENT.as_bytes());
        let global = self.signing.sign(&global_input).to_bytes();

        format!(
            "untrusted comment: signature from the oximux test key\n{}\ntrusted comment: \
             {TRUSTED_COMMENT}\n{}\n",
            b64(&sig_line),
            b64(&global)
        )
    }
}

fn b64(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture generator itself has to be right, or every gate test below
    /// it is vacuous — a helper that produced garbage would make "refuses bad
    /// signatures" pass for the wrong reason.
    #[test]
    fn the_fixtures_are_real_minisign_and_the_verifier_accepts_them() {
        let keys = MinisignKeypair::generate();
        let message = b"the manifest bytes";
        let key = minisign_verify::PublicKey::from_base64(&keys.public_key_base64())
            .expect("a well-formed public key");
        let signature =
            minisign_verify::Signature::decode(&keys.sign(message)).expect("a well-formed .minisig");
        // `false` = prehashed only, exactly as the verifier calls it.
        key.verify(message, &signature, false).expect("verifies");
    }

    #[test]
    fn two_generated_keypairs_are_distinct() {
        assert_ne!(
            MinisignKeypair::generate().public_key_base64(),
            MinisignKeypair::generate().public_key_base64()
        );
    }
}
