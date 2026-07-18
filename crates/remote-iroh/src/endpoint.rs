//! Binding iroh endpoints and accepting inbound connections for the host side.
//!
//! Thin wrappers over the iroh 1.0 endpoint builder that pin our ALPN and hand
//! back a framed [`IrohTransport`] per accepted connection — the dispatcher then
//! `serve`s that transport, blind to iroh.

use iroh::endpoint::presets;
use iroh::Endpoint;

use crate::transport::IrohTransport;
use crate::OXIMUX_ALPN;

/// Bind a **client** endpoint (n0 relays + pkarr discovery, no ALPN needed to
/// dial). Reuse the returned endpoint across reconnects via [`IrohConnector`].
///
/// [`IrohConnector`]: crate::IrohConnector
pub async fn bind_client() -> anyhow::Result<Endpoint> {
    Ok(Endpoint::bind(presets::N0).await?)
}

/// Bind a **host** endpoint advertising our ALPN so it can accept connections.
pub async fn bind_host() -> anyhow::Result<Endpoint> {
    Ok(Endpoint::builder(presets::N0).alpns(vec![OXIMUX_ALPN.to_vec()]).bind().await?)
}

/// Await one inbound connection on `endpoint`, accept its bi-stream, and wrap it
/// as a framed transport. `Ok(None)` once the endpoint stops accepting (closed).
pub async fn accept(endpoint: &Endpoint) -> anyhow::Result<Option<IrohTransport>> {
    let Some(incoming) = endpoint.accept().await else {
        return Ok(None);
    };
    let conn = incoming.await?;
    let (send, recv) = conn.accept_bi().await?;
    Ok(Some(IrohTransport::new(conn, send, recv)))
}
