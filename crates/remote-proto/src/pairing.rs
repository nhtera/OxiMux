//! The pairing ticket a QR encodes, and the `oximux://connect?ticket=` URL
//! helpers around it.
//!
//! A ticket carries only what routes and authenticates a first connection: the
//! host's iroh node id (so pkarr/relays can find it — **no IP is ever in the
//! QR**) and a one-time `handshake_secret` the client proves possession of via
//! HMAC. The secret IS the bearer credential, so it is never logged (see the
//! host's redaction discipline).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The scheme + host a pairing URL always starts with.
pub const CONNECT_URL_PREFIX: &str = "oximux://connect";

/// The canonical registration proof: `HMAC-SHA256(secret, app_pubkey || ts_le)`.
///
/// A wire invariant, and this is its one shared definition. The client
/// (`remote-session`) calls this to build `RegisterReq.proof`; the host verifies
/// the *same construction* over the same inputs, but does so with a constant-time
/// `Mac::verify_slice` (not by calling this fn and comparing — that would be a
/// timing-leaky equality). So the byte formula lives here for both to agree on,
/// while each side uses the primitive appropriate to its role. The
/// `handshake_secret` never crosses the wire; only this proof does, inside the
/// host's ±skew window.
pub fn registration_proof(
    secret: &[u8; 16],
    app_pubkey: &[u8; 32],
    timestamp_secs: u64,
) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(app_pubkey);
    mac.update(&timestamp_secs.to_le_bytes());
    mac.finalize().into_bytes().into()
}

/// Everything a phone needs to open its first connection to a host.
///
/// `Debug` is hand-written to **redact `handshake_secret`** — the ticket is the
/// bearer credential the plan requires never be logged, and a derived `Debug`
/// would print the secret bytes through any `{:?}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingTicket {
    /// The host's iroh endpoint id — a 32-byte Ed25519 public key. Kept as raw
    /// bytes so this crate stays iroh-free; the transport converts to
    /// `iroh::EndpointId`. Routing is via pkarr/relays from this id alone, so no
    /// address is exposed.
    pub endpoint_id: [u8; 32],
    /// One-time HMAC secret, pinned at **16 bytes (128-bit)** — adequate for a
    /// short-lived pairing proof (ignore any 256-bit references in upstream docs;
    /// the wire type is authoritative).
    pub handshake_secret: [u8; 16],
    /// A one-time ticket may bind pairing to a single session; `None` for a
    /// static/global ticket that authorizes the whole host.
    pub session_id: Option<String>,
}

impl std::fmt::Debug for PairingTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingTicket")
            .field("endpoint_id", &self.endpoint_id)
            .field("handshake_secret", &"<redacted>")
            .field("session_id", &self.session_id)
            .finish()
    }
}

/// A pairing encode/decode failure.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("not an oximux pairing url")]
    NotAPairingUrl,
    #[error("pairing url is missing its ticket parameter")]
    MissingTicket,
    #[error("ticket base64 is invalid: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("ticket postcard is invalid: {0}")]
    Postcard(#[from] postcard::Error),
}

impl PairingTicket {
    /// The ticket as its `base64url` (no-pad) postcard form — the exact string a
    /// QR encodes and the `ticket=` value in the URL.
    pub fn encode(&self) -> Result<String, PairingError> {
        use base64::Engine;
        let bytes = postcard::to_stdvec(self)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Decode a ticket from its `base64url` postcard form.
    pub fn decode(ticket_b64: &str) -> Result<Self, PairingError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(ticket_b64)?;
        Ok(postcard::from_bytes(&bytes)?)
    }

    /// The full `oximux://connect?ticket=…` deep link a QR renders.
    pub fn to_url(&self) -> Result<String, PairingError> {
        Ok(format!("{CONNECT_URL_PREFIX}?ticket={}", self.encode()?))
    }

    /// Parse a ticket out of an `oximux://connect?ticket=…` URL. The `ticket`
    /// value is `base64url` (no `&`/`=`/reserved chars), so a lightweight split
    /// avoids pulling a URL-parsing dependency into this portable crate.
    pub fn from_url(url: &str) -> Result<Self, PairingError> {
        let query = url
            .strip_prefix(CONNECT_URL_PREFIX)
            .and_then(|rest| rest.strip_prefix('?'))
            .ok_or(PairingError::NotAPairingUrl)?;
        let ticket = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("ticket="))
            .ok_or(PairingError::MissingTicket)?;
        Self::decode(ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(session: Option<&str>) -> PairingTicket {
        PairingTicket {
            endpoint_id: [0xAB; 32],
            handshake_secret: [0x11; 16],
            session_id: session.map(Into::into),
        }
    }

    #[test]
    fn ticket_round_trips_through_base64() {
        let t = sample(Some("sess-42"));
        let encoded = t.encode().expect("encode");
        assert_eq!(PairingTicket::decode(&encoded).expect("decode"), t);
    }

    #[test]
    fn ticket_round_trips_through_url() {
        let t = sample(None);
        let url = t.to_url().expect("url");
        assert!(url.starts_with("oximux://connect?ticket="));
        assert_eq!(PairingTicket::from_url(&url).expect("from_url"), t);
    }

    #[test]
    fn base64url_is_pad_and_reserved_char_free() {
        // A QR / URL must not carry '+', '/', or '=' — those need escaping and
        // break the naive `from_url` split. URL_SAFE_NO_PAD guarantees none.
        let encoded = sample(Some("s")).encode().expect("encode");
        assert!(!encoded.contains(['+', '/', '=']));
    }

    #[test]
    fn wrong_scheme_is_rejected() {
        assert!(matches!(
            PairingTicket::from_url("https://evil.example/?ticket=abc"),
            Err(PairingError::NotAPairingUrl)
        ));
    }

    #[test]
    fn missing_ticket_param_is_rejected() {
        assert!(matches!(
            PairingTicket::from_url("oximux://connect?foo=bar"),
            Err(PairingError::MissingTicket)
        ));
    }

    #[test]
    fn garbage_ticket_is_a_decode_error_not_a_panic() {
        assert!(PairingTicket::decode("!!!not base64!!!").is_err());
    }

    #[test]
    fn debug_redacts_the_handshake_secret() {
        let t = PairingTicket {
            endpoint_id: [0; 32],
            handshake_secret: [0xDE; 16],
            session_id: None,
        };
        let rendered = format!("{t:?}");
        assert!(rendered.contains("<redacted>"), "secret must be redacted");
        assert!(!rendered.contains("222"), "raw secret byte (0xDE=222) must not appear");
    }
}
