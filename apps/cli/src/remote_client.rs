//! Dialling a paired host over iroh, and the client half of its auth
//! handshake.
//!
//! The handshake is written here rather than reused from `remote-session`
//! because that crate's [`RemoteSession`](oximux_remote_session::RemoteSession)
//! owns a demux pump: it multiplexes RPCs against a live event stream, which is
//! exactly right for the phone and exactly wrong for a CLI whose every verb
//! already drives one framed transport directly. What IS reused is everything
//! that must not be reimplemented — [`ClientSigner`] for the key, and
//! `registration_proof` for the one canonical proof construction both sides
//! agree on.
//!
//! Nothing here logs. A ticket carries a bearer secret and the reconnect token
//! is a bearer credential; neither may reach a terminal, a log, or `--json`.

use std::sync::Arc;
use std::time::Duration;

use oximux_remote_proto::messages::{AuthProveReq, ConnectReq, RegisterReq};
use oximux_remote_proto::pairing::PairingTicket;
use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::{Transport, registration_proof};
use oximux_remote_session::{ClientSigner, Connector};

use crate::cli::exit;
use crate::output::Failure;

/// The dial deadline when the caller does not shorten it.
///
/// Generous: a first contact resolves the host through pkarr and may fall back
/// to a relay, and a cross-network hole-punch is not instant. `--timeout` can
/// bring it in — a caller that asked to wait two seconds means it, and a fleet
/// view over a dozen hosts is exactly where that matters — but nothing may
/// extend it past this, because a CLI that hangs is worse than one that says
/// "not reachable".
const DIAL_CEILING: Duration = Duration::from_secs(20);

fn unreachable(detail: impl std::fmt::Display) -> Failure {
    Failure::new("unreachable", exit::UNREACHABLE, format!("could not reach the host: {detail}"))
        .with_steps([
            "check the host is running (`oximux serve`, or the desktop app)".into(),
            "both ends need network access; a first connection may take a few seconds".into(),
        ])
}

/// Open a framed transport to `endpoint_id`.
///
/// Binds a throwaway client endpoint per invocation. A CLI is a short-lived
/// process, so there is no reconnect loop to amortise a long-lived endpoint
/// over — and a fresh bind means a moved host is re-resolved every time.
pub async fn dial(
    endpoint_id: [u8; 32],
    deadline: tokio::time::Instant,
) -> Result<Arc<dyn Transport>, Failure> {
    let deadline = deadline.min(tokio::time::Instant::now() + DIAL_CEILING);
    let endpoint = tokio::time::timeout_at(deadline, oximux_remote_iroh::bind_client())
        .await
        .map_err(|_| unreachable("timed out binding a local endpoint"))?
        .map_err(|e| unreachable(e))?;
    let connector = oximux_remote_iroh::IrohConnector::new(endpoint, endpoint_id)
        .map_err(|e| unreachable(e))?;
    tokio::time::timeout_at(deadline, connector.connect())
        .await
        .map_err(|_| unreachable("timed out dialling"))?
        .map_err(|e| unreachable(e))
}

/// The instant a connect attempt must be finished by: dial, version exchange,
/// and authentication together. One budget for the whole thing, so `--timeout`
/// means what it says.
pub fn deadline_in(timeout: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + timeout
}

/// One request → one response on a transport that has no demux behind it.
///
/// Used only during the handshake, before any subscription exists, so no push
/// frame can interleave. The verb-level client owns the general case.
///
/// Bounded by a **shared deadline**, not a per-leg timeout. The handshake is
/// three round trips (Hello, Connect, AuthProve); giving each its own window
/// would let one stalled host cost six times what `--timeout` promised — which
/// matters most in exactly the place the flag matters most, a fleet fan-out
/// over a dozen hosts.
async fn call(
    transport: &dyn Transport,
    req: Request,
    deadline: tokio::time::Instant,
) -> Result<Response, Failure> {
    let bytes = req
        .to_bytes()
        .map_err(|e| Failure::new("encode", exit::ERROR, format!("could not encode: {e}")))?;
    tokio::time::timeout_at(deadline, transport.send(bytes))
        .await
        .map_err(|_| unreachable("timed out sending"))?
        .map_err(|e| unreachable(e))?;
    let frame = tokio::time::timeout_at(deadline, transport.recv())
        .await
        .map_err(|_| unreachable("timed out waiting for a reply"))?
        .map_err(|e| unreachable(e))?
        .ok_or_else(|| unreachable("the host closed the connection"))?;
    Response::from_bytes(&frame)
        .map_err(|e| Failure::new("decode", exit::ERROR, format!("undecodable host reply: {e}")))
}

/// A handshake reply that does not answer what was asked.
///
/// **Never formats the payload.** `Response::Registered` and
/// `Response::Connected` carry the reconnect token as a plain `String`, and
/// this message reaches stderr, the `--json` envelope, and — through
/// `fleet_ls` — a table cell. A `{got:?}` here would put a live bearer
/// credential in all three. The variant *kind* is named instead, which is what
/// a reader actually needs, and is the same discipline
/// `SessionError::Unexpected { expected }` follows on the phone's side.
fn unexpected_handshake_reply(expected: &'static str, got: &Response) -> Failure {
    let kind = match got {
        Response::Error(_) => "an error",
        Response::Registered { .. } | Response::Connected { .. } => "a credential reply",
        Response::Challenge { .. } => "a challenge",
        Response::HelloAck(_) => "a version ack",
        Response::Pong => "a pong",
        _ => "some other reply",
    };
    Failure::new(
        "protocol",
        exit::ERROR,
        format!("the host answered {expected} with {kind}"),
    )
}

/// Exchange protocol versions **before** offering any credential.
///
/// The ordering is the point, and it is the reference implementation's
/// (`RemoteSession::pair` calls `hello()` first, unconditionally): an
/// unusable pairing must fail with "update the older side" rather than with an
/// opaque refusal to a proof the host could not have understood anyway.
pub async fn hello(
    transport: &dyn Transport,
    deadline: tokio::time::Instant,
) -> Result<oximux_remote_proto::messages::HelloAckWire, Failure> {
    use oximux_remote_proto::messages::HelloReq;
    use oximux_remote_proto::proto::{MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, is_compatible};

    let req = Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION });
    let ack = match call(transport, req, deadline).await? {
        Response::HelloAck(ack) => ack,
        Response::Error(e) => return Err(pairing_failure(e)),
        other => return Err(unexpected_handshake_reply("Hello", &other)),
    };
    if !is_compatible(ack.protocol_version) || PROTOCOL_VERSION < ack.min_compatible {
        return Err(Failure::new(
            "incompatible",
            exit::ERROR,
            format!(
                "the host speaks protocol v{} (min v{}); this CLI speaks v{PROTOCOL_VERSION} \
                 (min v{MIN_COMPATIBLE_VERSION})",
                ack.protocol_version, ack.min_compatible
            ),
        )
        .with_steps(["update the older side so both speak a compatible protocol".into()]));
    }
    Ok(ack)
}

/// First-time enrollment: prove possession of the ticket's secret.
///
/// The proof is minted from the secret and never sends it, so a ticket that
/// leaks *after* pairing cannot be replayed against this enrollment — only
/// against a fresh one, which is why the host makes tickets one-time and short-
/// lived.
pub async fn register(
    transport: &dyn Transport,
    signer: &ClientSigner,
    ticket: &PairingTicket,
    device_name: &str,
    deadline: tokio::time::Instant,
) -> Result<String, Failure> {
    let app_pubkey = signer.public_key();
    let now = now_secs();
    let req = Request::Register(RegisterReq {
        app_pubkey,
        device_name: device_name.to_string(),
        proof: registration_proof(&ticket.handshake_secret, &app_pubkey, now),
        timestamp_secs: now,
        session_id: ticket.session_id.clone(),
    });
    match call(transport, req, deadline).await? {
        Response::Registered { session_token } => Ok(session_token),
        Response::Error(e) => Err(pairing_failure(e)),
        other => Err(unexpected_handshake_reply("Register", &other)),
    }
}

/// Reconnect an existing enrollment: the host challenges, we sign the nonce
/// with the key it paired.
///
/// No token fast path. A CLI process holds no token from a previous run (it is
/// a bearer credential and is deliberately not persisted), so the challenge
/// flow is the only path — one extra round trip, in exchange for nothing
/// reusable sitting on disk.
pub async fn authenticate(
    transport: &dyn Transport,
    signer: &ClientSigner,
    deadline: tokio::time::Instant,
) -> Result<String, Failure> {
    let app_pubkey = signer.public_key();
    let challenged =
        call(transport, Request::Connect(ConnectReq { app_pubkey, session_token: None }), deadline)
            .await?;
    let nonce = match challenged {
        Response::Challenge { nonce } => nonce,
        // A host that answered outright (it still held a live token for this
        // key) is already authenticated.
        Response::Connected { session_token } => return Ok(session_token),
        Response::Error(e) => return Err(pairing_failure(e)),
        other => return Err(unexpected_handshake_reply("Connect", &other)),
    };
    let signature = signer.sign(&nonce).to_vec();
    match call(transport, Request::AuthProve(AuthProveReq { signature }), deadline).await? {
        Response::Connected { session_token } => Ok(session_token),
        Response::Error(e) => Err(pairing_failure(e)),
        other => Err(unexpected_handshake_reply("AuthProve", &other)),
    }
}

/// A handshake refusal, with the follow-ups that actually resolve it.
///
/// Distinguished from the generic RPC mapping because the causes are different:
/// at this point `Unauthorized` almost always means the enrollment is gone
/// (revoked host-side, or the local key was regenerated), not that the caller
/// lacks scope for one verb.
fn pairing_failure(err: oximux_remote_proto::proto::RpcError) -> Failure {
    use oximux_remote_proto::proto::RpcError;
    match err {
        RpcError::Unauthorized => Failure::new(
            "denied",
            exit::DENIED,
            "the host refused this enrollment",
        )
        .with_steps([
            "the pairing may have been revoked, or the ticket already redeemed".into(),
            "mint a fresh ticket on the host (`oximux pair-new`) and pair again".into(),
        ]),
        RpcError::IncompatibleVersion { host_version, host_min_compatible } => Failure::new(
            "incompatible",
            exit::ERROR,
            format!(
                "the host speaks protocol v{host_version} (min v{host_min_compatible}); \
                 this CLI speaks v{}",
                oximux_remote_proto::proto::PROTOCOL_VERSION
            ),
        )
        .with_steps(["update whichever side is older".into()]),
        other => Failure::new("rpc", exit::ERROR, format!("host error: {other:?}")),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a ticket from either an `oximux://connect?ticket=…` URL or the bare
/// base64url the QR encodes, so pasting either works.
pub fn parse_ticket(raw: &str) -> Result<PairingTicket, Failure> {
    let raw = raw.trim();
    let parsed = if raw.starts_with(oximux_remote_proto::pairing::CONNECT_URL_PREFIX) {
        PairingTicket::from_url(raw)
    } else {
        PairingTicket::decode(raw)
    };
    // The error text deliberately does not echo the input: it is a bearer
    // credential, and a shell that logs stderr must not capture it.
    parsed.map_err(|e| {
        Failure::new("ticket", exit::USAGE, format!("that is not a valid pairing ticket: {e}"))
            .with_steps([
                "paste the whole `oximux://connect?ticket=…` link, or the ticket alone".into(),
                "tickets are short-lived — mint a fresh one if this has been sitting around".into(),
            ])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> PairingTicket {
        PairingTicket {
            endpoint_id: [7u8; 32],
            handshake_secret: [9u8; 16],
            session_id: None,
        }
    }

    #[test]
    fn a_ticket_parses_from_a_url_or_bare() {
        let t = ticket();
        assert_eq!(parse_ticket(&t.to_url().unwrap()).unwrap().endpoint_id, t.endpoint_id);
        assert_eq!(parse_ticket(&t.encode().unwrap()).unwrap().endpoint_id, t.endpoint_id);
        assert_eq!(
            parse_ticket(&format!("  {}  ", t.encode().unwrap())).unwrap().endpoint_id,
            t.endpoint_id,
            "surrounding whitespace from a paste is trimmed"
        );
    }

    /// The error must not echo the input — a malformed paste is still a
    /// credential-shaped string, and stderr is frequently captured.
    #[test]
    fn a_bad_ticket_is_refused_without_echoing_it() {
        let secretish = "AAAAsupersecretlookingvalueAAAA";
        let err = parse_ticket(secretish).expect_err("not a ticket");
        assert_eq!(err.exit, exit::USAGE);
        assert!(!err.message.contains(secretish), "the input must not be echoed: {}", err.message);
        for step in &err.next_steps {
            assert!(!step.contains(secretish), "nor in the next steps");
        }
    }

    /// A reply carrying a live reconnect token must never be formatted into an
    /// error. `Failure.message` reaches stderr, the `--json` envelope, AND a
    /// cell in the fleet table — a `{:?}` of the `Response` here would put a
    /// bearer credential in all three.
    #[test]
    fn an_off_contract_reply_never_prints_the_token_it_carried() {
        let token = "SECRET-RECONNECT-TOKEN";
        for reply in [
            Response::Connected { session_token: token.into() },
            Response::Registered { session_token: token.into() },
        ] {
            let err = unexpected_handshake_reply("Connect", &reply);
            assert!(!err.message.contains(token), "token leaked: {}", err.message);
            assert!(
                err.message.contains("credential reply"),
                "and it still says what came back: {}",
                err.message
            );
        }
    }

    /// The other arms stay informative — naming the kind is the whole point of
    /// not just dropping the reply on the floor.
    #[test]
    fn other_off_contract_replies_are_still_named() {
        for (reply, expected) in [
            (Response::Pong, "a pong"),
            (Response::Challenge { nonce: [0u8; 32] }, "a challenge"),
            (Response::Ack, "some other reply"),
        ] {
            let err = unexpected_handshake_reply("Register", &reply);
            assert!(err.message.contains(expected), "{}", err.message);
        }
    }
}
