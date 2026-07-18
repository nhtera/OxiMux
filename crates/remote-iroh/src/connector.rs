//! The client dial seam: an [`IrohConnector`] that resolves the host by its iroh
//! [`EndpointId`] (from the scanned pairing ticket) and opens a framed bi-stream.
//!
//! It implements [`Connector`], so `remote_session::maintain_connection` drives it
//! for the self-healing dial/reconnect loop. The connector holds only routing
//! data — the host's endpoint id and any direct-address hints — never the pairing
//! secret; that stays with the `RemoteSession` handshake, keeping the secret off
//! this path entirely.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use oximux_remote_proto::transport::Transport;
use oximux_remote_session::{ConnectError, Connector};

use crate::transport::IrohTransport;
use crate::OXIMUX_ALPN;

/// Dials the host over iroh. Reuses one bound [`Endpoint`] across every reconnect
/// attempt (re-binding would churn the relay handshake and pkarr record).
pub struct IrohConnector {
    endpoint: Endpoint,
    host_id: EndpointId,
    /// Optional direct socket hints. Empty in production (pure pkarr/relay
    /// discovery from the id alone); on a LAN or in a same-machine test these let
    /// the dial connect directly without waiting on public discovery.
    direct_addrs: Vec<SocketAddr>,
}

impl IrohConnector {
    /// Build a connector for a host identified by the 32-byte iroh endpoint id the
    /// pairing ticket carries (`PairingTicket::endpoint_id`).
    pub fn new(endpoint: Endpoint, host_id: [u8; 32]) -> Result<Self, ConnectError> {
        let host_id = EndpointId::from_bytes(&host_id)
            .map_err(|e| ConnectError::Unreachable(format!("bad host endpoint id: {e}")))?;
        Ok(Self { endpoint, host_id, direct_addrs: Vec::new() })
    }

    /// Attach direct socket hints (LAN / same-machine) so the dial doesn't depend
    /// on public discovery.
    pub fn with_direct_addrs(mut self, addrs: Vec<SocketAddr>) -> Self {
        self.direct_addrs = addrs;
        self
    }
}

#[async_trait]
impl Connector for IrohConnector {
    async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError> {
        let mut addr = EndpointAddr::from(self.host_id);
        for &socket in &self.direct_addrs {
            addr = addr.with_ip_addr(socket);
        }
        let conn = self
            .endpoint
            .connect(addr, OXIMUX_ALPN)
            .await
            .map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        let (send, recv) =
            conn.open_bi().await.map_err(|e| ConnectError::Unreachable(e.to_string()))?;
        Ok(Arc::new(IrohTransport::new(conn, send, recv)))
    }
}
