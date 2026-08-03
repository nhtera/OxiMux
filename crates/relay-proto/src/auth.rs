//! The proofs both ends of the relay connection exchange instead of the token.
//!
//! A unix-domain socket sits in a directory only this account can enter, so
//! whoever answers it is this account. `\\.\pipe\` is one flat namespace and
//! pipe *creation* needs no privilege, so the name the client dials can be
//! claimed by a process that got there first — and the relay's own
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` does not help the client here: it makes the
//! *real* daemon fail to start, which leaves the squatter holding the name the
//! GUI is about to dial.
//!
//! A squatter that receives the token gets to impersonate the relay for the
//! rest of the session — every byte the user sees in a terminal would be
//! attacker-authored. So the token never goes on the wire. Each side proves it
//! knows the token by MACing a pair of nonces, one chosen by each end:
//!
//! - both nonces are covered, so neither proof can be replayed into a later
//!   connection (a recorded proof is bound to nonces that will not recur);
//! - the two directions use different domain prefixes, so a proof harvested
//!   from one side cannot be echoed back as the other's;
//! - the server proves itself *first*. It cannot produce that proof without the
//!   token, so a squatter is caught before the client has revealed anything at
//!   all — the client simply hangs up.
//!
//! Neither the token nor either proof identifies a *process*, so this says
//! nothing about which program is on the far end beyond "it can read a file
//! only this account can read". That is the same statement the unix socket
//! makes, which is the bar being restored here rather than raised.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Nonce length. 32 bytes is well past what collision resistance needs here;
/// the cost of being generous is 32 bytes on a local socket, once per connect.
pub const NONCE_LEN: usize = 32;

/// A proof is the full SHA-256 HMAC tag.
pub const PROOF_LEN: usize = 32;

pub type Nonce = [u8; NONCE_LEN];
pub type Proof = [u8; PROOF_LEN];

// Distinct prefixes are what stop a proof going in the direction it was not
// made for. Without them both sides would MAC identical input, so a squatter
// could take the client's proof and replay it as its own server proof on the
// next connection.
const CLIENT_DOMAIN: &[u8] = b"oximux-relay/v8/client";
const SERVER_DOMAIN: &[u8] = b"oximux-relay/v8/server";

/// Proof that the *client* holds the token.
pub fn client_proof(token: &str, server_nonce: &Nonce, client_nonce: &Nonce) -> Proof {
    proof(CLIENT_DOMAIN, token, server_nonce, client_nonce)
}

/// Proof that the *server* holds the token.
pub fn server_proof(token: &str, server_nonce: &Nonce, client_nonce: &Nonce) -> Proof {
    proof(SERVER_DOMAIN, token, server_nonce, client_nonce)
}

fn proof(domain: &[u8], token: &str, server_nonce: &Nonce, client_nonce: &Nonce) -> Proof {
    // `new_from_slice` on Hmac only fails for key lengths it cannot handle, and
    // HMAC accepts any length — it hashes keys longer than the block size and
    // zero-pads shorter ones. An empty token is refused where it is read, not
    // here.
    let mut mac = <Hmac<Sha256>>::new_from_slice(token.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(domain);
    mac.update(server_nonce);
    mac.update(client_nonce);
    mac.finalize().into_bytes().into()
}

/// Compare two proofs without leaking where they diverge.
///
/// A byte-by-byte `==` returns sooner the earlier the mismatch, which over many
/// attempts recovers the expected tag one byte at a time. The window for that
/// is small here — a wrong proof drops the connection — but the constant-time
/// comparison is free, and reaching for `==` is the kind of thing that reads as
/// correct forever after.
pub fn proofs_match(a: &Proof, b: &Proof) -> bool {
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce(seed: u8) -> Nonce {
        [seed; NONCE_LEN]
    }

    #[test]
    fn the_same_inputs_give_the_same_proof() {
        // Both ends compute independently; if this ever stopped holding, every
        // connection would fail closed rather than open, but it would fail.
        let a = client_proof("tok", &nonce(1), &nonce(2));
        let b = client_proof("tok", &nonce(1), &nonce(2));
        assert!(proofs_match(&a, &b));
    }

    #[test]
    fn the_two_directions_do_not_produce_the_same_proof() {
        // The property the domain prefixes exist for: a client proof must not
        // be replayable as the server's.
        let c = client_proof("tok", &nonce(1), &nonce(2));
        let s = server_proof("tok", &nonce(1), &nonce(2));
        assert!(!proofs_match(&c, &s));
    }

    #[test]
    fn a_different_token_gives_a_different_proof() {
        let a = client_proof("tok", &nonce(1), &nonce(2));
        let b = client_proof("tuk", &nonce(1), &nonce(2));
        assert!(!proofs_match(&a, &b));
    }

    #[test]
    fn either_nonce_changing_changes_the_proof() {
        // Both must be covered: if only one were, the side that picks the other
        // could force a proof it had already seen.
        let base = client_proof("tok", &nonce(1), &nonce(2));
        assert!(!proofs_match(&base, &client_proof("tok", &nonce(9), &nonce(2))));
        assert!(!proofs_match(&base, &client_proof("tok", &nonce(1), &nonce(9))));
    }

    #[test]
    fn nonce_order_is_not_interchangeable() {
        // Guards against a refactor that passes the two nonces the other way
        // round on one side only — which would still "work" if the MAC treated
        // them as an unordered pair.
        let a = client_proof("tok", &nonce(1), &nonce(2));
        let b = client_proof("tok", &nonce(2), &nonce(1));
        assert!(!proofs_match(&a, &b));
    }

    #[test]
    fn proofs_match_rejects_a_single_byte_difference() {
        let a = client_proof("tok", &nonce(1), &nonce(2));
        let mut b = a;
        b[PROOF_LEN - 1] ^= 1;
        assert!(!proofs_match(&a, &b));
        // ...including in the first byte, where a short-circuiting compare
        // would bail earliest.
        let mut c = a;
        c[0] ^= 1;
        assert!(!proofs_match(&a, &c));
    }
}
