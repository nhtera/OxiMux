//! The local handshake: prove the token, claim a scope.
//!
//! The relay's v8 discipline, reused via its `relay-proto` primitives: the
//! token never crosses the socket; each side proves possession with
//! `HMAC(token, server_nonce, client_nonce)`, the **host proves first** (so a
//! squatter socket learns nothing and the CLI can refuse it), and comparisons
//! are constant-time. The scope claim rides the client's proof message —
//! carried here, enforced by the dispatcher's ACL.
//!
//! Rides the already-framed [`Transport`], so the handshake and the RPCs that
//! follow share one framing layer instead of a raw-stream phase with its own.

use oximux_relay_proto::auth::{Nonce, client_proof, proofs_match, server_proof};
use oximux_remote_proto::Transport;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

/// What the connection asked to be: the operator, or one agent session's
/// confined caller. The host maps this onto its ACL scope.
///
/// **Not self-declared.** A caller names an [`identity label`](LocalIdentity)
/// in its hello, and the host answers with the scope registered for the secret
/// that label is bound to — so naming a label whose secret you do not hold
/// fails the proof and grants nothing. An earlier shape sent this enum
/// directly and had the host believe it, which made the agent narrowing
/// decorative: one shared token meant any holder could simply say `Operator`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalClaim {
    Operator,
    Session(String),
}

/// Which credential a caller is presenting. The label itself is **not** a
/// secret and carries no authority — it only tells the host which registered
/// secret to run the proof against, so the host can prove itself first.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LocalIdentity {
    /// The operator credential from the runtime dir's token file.
    Operator,
    /// A per-session credential handed to one agent process at spawn.
    Session(String),
}

impl LocalIdentity {
    /// The scope a successfully-proven identity earns.
    pub(crate) fn granted_claim(&self) -> LocalClaim {
        match self {
            LocalIdentity::Operator => LocalClaim::Operator,
            LocalIdentity::Session(id) => LocalClaim::Session(id.clone()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ClientHello {
    client_nonce: Nonce,
    /// Which registered secret this caller claims to hold.
    identity: LocalIdentity,
}

#[derive(Serialize, Deserialize)]
struct ServerChallenge {
    server_nonce: Nonce,
    /// The host's proof it holds the named secret — sent first, so the client
    /// reveals nothing to a socket that cannot authenticate itself. An unknown
    /// label gets a proof computed over a random secret it cannot match,
    /// rather than a distinct error: whether a given session has a credential
    /// registered is not something an unauthenticated caller should be able to
    /// probe.
    server_proof: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct ClientProof {
    client_proof: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct ServerVerdict {
    granted: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HelloError {
    #[error("the socket closed during the handshake")]
    Closed,
    #[error("handshake transport error: {0}")]
    Transport(String),
    #[error("undecodable handshake message")]
    Decode,
    /// The listener could not prove it holds the token — a squatter socket, or
    /// a stale token file. The client must not proceed.
    #[error("the host could not prove it holds the control token")]
    HostNotTrusted,
    /// The caller could not prove it holds the token.
    #[error("the caller could not prove it holds the control token")]
    Denied,
    /// The caller connected but did not finish the handshake in time. Its own
    /// failure, never the listener's: the deadline exists so a silent peer
    /// cannot hold a slot the next caller needs.
    #[error("the caller did not complete the handshake in time")]
    HandshakeTimeout,
}

fn nonce() -> Nonce {
    let mut n = [0u8; 32];
    OsRng.fill_bytes(&mut n);
    n
}

async fn send_msg<T: Serialize>(t: &dyn Transport, msg: &T) -> Result<(), HelloError> {
    let bytes = postcard::to_allocvec(msg).map_err(|_| HelloError::Decode)?;
    t.send(bytes).await.map_err(|e| HelloError::Transport(e.to_string()))
}

async fn recv_msg<T: for<'de> Deserialize<'de>>(t: &dyn Transport) -> Result<T, HelloError> {
    let frame = t
        .recv()
        .await
        .map_err(|e| HelloError::Transport(e.to_string()))?
        .ok_or(HelloError::Closed)?;
    postcard::from_bytes(&frame).map_err(|_| HelloError::Decode)
}

/// Client half: name the credential held, verify the host proves it first,
/// then prove it back.
pub(crate) async fn client_handshake(
    t: &dyn Transport,
    token: &str,
    identity: LocalIdentity,
) -> Result<(), HelloError> {
    let client_nonce = nonce();
    send_msg(t, &ClientHello { client_nonce, identity }).await?;
    let challenge: ServerChallenge = recv_msg(t).await?;
    let expected = server_proof(token, &challenge.server_nonce, &client_nonce);
    if !proofs_match(&challenge.server_proof, &expected) {
        return Err(HelloError::HostNotTrusted);
    }
    let proof = client_proof(token, &challenge.server_nonce, &client_nonce);
    send_msg(t, &ClientProof { client_proof: proof }).await?;
    let verdict: ServerVerdict = recv_msg(t).await?;
    if verdict.granted { Ok(()) } else { Err(HelloError::Denied) }
}

/// Host half: look up the secret for the named identity, prove it first, then
/// verify the caller's proof. Returns the scope **that identity is registered
/// for** — never a scope the caller asked for. On a bad proof the caller is
/// told (`granted: false`) and refused.
///
/// `secret_for` returns `None` for an unregistered label. That case still runs
/// the full exchange against a random secret rather than short-circuiting: an
/// unauthenticated caller must not be able to enumerate which sessions have
/// credentials by timing or by a distinct error.
pub(crate) async fn server_handshake(
    t: &dyn Transport,
    secret_for: impl Fn(&LocalIdentity) -> Option<String>,
) -> Result<LocalClaim, HelloError> {
    let hello: ClientHello = recv_msg(t).await?;
    let known = secret_for(&hello.identity);
    // An unknown label proves with a value the caller cannot possibly hold,
    // so it fails at exactly the same step a wrong secret would.
    let token = known.clone().unwrap_or_else(unguessable_secret);
    let server_nonce = nonce();
    let proof = server_proof(&token, &server_nonce, &hello.client_nonce);
    send_msg(t, &ServerChallenge { server_nonce, server_proof: proof }).await?;
    let client: ClientProof = recv_msg(t).await?;
    let expected = client_proof(&token, &server_nonce, &hello.client_nonce);
    if known.is_none() || !proofs_match(&client.client_proof, &expected) {
        send_msg(t, &ServerVerdict { granted: false }).await?;
        return Err(HelloError::Denied);
    }
    send_msg(t, &ServerVerdict { granted: true }).await?;
    // The scope comes from the identity that just PROVED itself, not from
    // anything the caller said it wanted.
    Ok(hello.identity.granted_claim())
}

/// A throwaway secret for the unknown-label path — never stored, never
/// matched.
///
/// The crate's own token minter rather than a second copy of it: this value has
/// to be as unguessable as a real credential, and two independent definitions of
/// "a fresh secret" would let one be strengthened while the other quietly was
/// not.
fn unguessable_secret() -> String {
    crate::generate_token()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::future::join;
    use oximux_remote_proto::testing::duplex_pair;

    const TOKEN: &str = "0011223344556677";
    const SESSION_SECRET: &str = "8899aabbccddeeff";

    /// A registry of exactly one operator credential.
    fn operator_only(identity: &LocalIdentity) -> Option<String> {
        match identity {
            LocalIdentity::Operator => Some(TOKEN.to_string()),
            LocalIdentity::Session(_) => None,
        }
    }

    /// Operator credential in, operator scope out.
    #[test]
    fn proving_the_operator_credential_grants_operator_scope() {
        let (client, server) = duplex_pair();
        let (client_out, server_out) = block_on(join(
            client_handshake(&client, TOKEN, LocalIdentity::Operator),
            server_handshake(&server, operator_only),
        ));
        client_out.expect("client granted");
        assert_eq!(server_out.unwrap(), LocalClaim::Operator);
    }

    /// A per-session credential grants exactly its own session's scope — the
    /// scope follows the SECRET, never the caller's word.
    #[test]
    fn proving_a_session_credential_grants_that_session() {
        let registry = |identity: &LocalIdentity| match identity {
            LocalIdentity::Session(id) if id == "sess-9" => Some(SESSION_SECRET.to_string()),
            LocalIdentity::Operator => Some(TOKEN.to_string()),
            _ => None,
        };
        let (client, server) = duplex_pair();
        let (client_out, server_out) = block_on(join(
            client_handshake(&client, SESSION_SECRET, LocalIdentity::Session("sess-9".into())),
            server_handshake(&server, registry),
        ));
        client_out.expect("client granted");
        assert_eq!(server_out.unwrap(), LocalClaim::Session("sess-9".into()));
    }

    /// THE containment property: an agent holding only its own session secret
    /// cannot reach operator scope by naming the operator identity, and cannot
    /// reach another session by naming that one. It is refused at the proof,
    /// because the label it names is bound to a secret it does not hold.
    #[test]
    fn a_session_holder_cannot_name_its_way_to_another_scope() {
        let registry = |identity: &LocalIdentity| match identity {
            LocalIdentity::Operator => Some(TOKEN.to_string()),
            LocalIdentity::Session(id) if id == "sess-9" => Some(SESSION_SECRET.to_string()),
            LocalIdentity::Session(id) if id == "sess-other" => Some("othersecret".to_string()),
            _ => None,
        };
        for target in [
            LocalIdentity::Operator,
            LocalIdentity::Session("sess-other".into()),
        ] {
            let (client, server) = duplex_pair();
            // Holds ONLY its own session secret, but names a richer identity.
            let escalate = async move {
                let out = client_handshake(&client, SESSION_SECRET, target.clone()).await;
                drop(client);
                out
            };
            let (client_out, server_out) =
                block_on(join(escalate, server_handshake(&server, registry)));
            // It fails at the host-proof step: the host proved with the secret
            // for the named label, which this caller cannot verify.
            assert!(
                matches!(client_out, Err(HelloError::HostNotTrusted)),
                "escalation must fail before the caller proves anything"
            );
            assert!(server_out.is_err(), "and the host must grant nothing");
        }
    }

    /// An unregistered label is refused, and is not distinguishable from a
    /// wrong secret — no probing which sessions have credentials.
    #[test]
    fn an_unknown_identity_is_refused() {
        let (client, server) = duplex_pair();
        let probe = async move {
            let out = client_handshake(
                &client,
                SESSION_SECRET,
                LocalIdentity::Session("no-such-session".into()),
            )
            .await;
            drop(client);
            out
        };
        let (client_out, server_out) =
            block_on(join(probe, server_handshake(&server, operator_only)));
        // Indistinguishable from a wrong secret: the host proved with a value
        // the caller cannot match, so the caller refuses the host at exactly
        // the step a bad secret would — it never learns the label was unknown.
        assert!(matches!(client_out, Err(HelloError::HostNotTrusted)));
        // The host grants nothing. The variant is whichever refusal comes
        // first: this caller hangs up on the bogus proof, so the host's next
        // read is `Closed` rather than reaching its own `Denied`. Asserting
        // the property (no grant) rather than the race's winner.
        assert!(server_out.is_err(), "an unknown identity must never be granted a scope");
    }

    /// A caller with the wrong token refuses the HOST first: the host's proof
    /// fails verification before the caller reveals anything of its own. The
    /// client future must also DROP its transport on the way out — the host is
    /// still awaiting a proof, and hanging up is what unblocks it (a `join`
    /// on an undropped client would deadlock, which is exactly how a real
    /// caller behaves too: refuse, then hang up).
    #[test]
    fn wrong_token_refuses_the_host_before_proving() {
        let (client, server) = duplex_pair();
        let refuse_then_hang_up = async move {
            let out =
                client_handshake(&client, "not-the-token", LocalIdentity::Operator).await;
            drop(client);
            out
        };
        let (client_out, server_out) =
            block_on(join(refuse_then_hang_up, server_handshake(&server, operator_only)));
        assert!(matches!(client_out, Err(HelloError::HostNotTrusted)));
        assert!(server_out.is_err(), "the host sees the caller vanish, never a grant");
    }

    /// A caller that verifies the host but presents a garbage proof of its own
    /// is denied server-side and told so.
    #[test]
    fn bad_client_proof_is_denied() {
        let (client, server) = duplex_pair();
        let attacker = async {
            let client_nonce = nonce();
            send_msg(
                &client,
                &ClientHello { client_nonce, identity: LocalIdentity::Operator },
            )
            .await
            .unwrap();
            let challenge: ServerChallenge = recv_msg(&client).await.unwrap();
            // The host is genuine (its proof verifies) …
            let expected = server_proof(TOKEN, &challenge.server_nonce, &client_nonce);
            assert!(proofs_match(&challenge.server_proof, &expected));
            // … but we answer with garbage instead of the token proof.
            send_msg(&client, &ClientProof { client_proof: [0u8; 32] }).await.unwrap();
            let verdict: ServerVerdict = recv_msg(&client).await.unwrap();
            assert!(!verdict.granted, "a garbage proof must not be granted");
        };
        let (_, server_out) =
            block_on(join(attacker, server_handshake(&server, operator_only)));
        assert!(matches!(server_out, Err(HelloError::Denied)));
    }
}
