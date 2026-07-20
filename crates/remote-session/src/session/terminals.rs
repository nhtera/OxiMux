//! The terminal RPCs: list, attach, type, resize, detach.
//!
//! Attaching returns only the replay snapshot. The live frames that follow
//! arrive on the connection's terminal stream ([`RemoteSession::take_terminals`])
//! rather than being returned here, because they are *pushed* — the pump routes
//! them off the reply path, exactly as it does session events, so an RPC issued
//! while a terminal is streaming still gets its own answer back.

use oximux_remote_proto::messages::TerminalSummary;
use oximux_remote_proto::proto::{Request, Response};

use crate::error::SessionError;
use crate::session::{RemoteSession, Result};

/// A terminal's replay snapshot and the grid it was drawn at.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalAttached {
    /// Raw bytes to feed an emulator built at exactly `cols` x `rows`.
    pub replay: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}

impl RemoteSession {
    /// Every terminal the host exposes to this device.
    pub async fn list_terminals(&self) -> Result<Vec<TerminalSummary>> {
        match self.call(Request::ListTerminals).await? {
            Response::Terminals(rows) => Ok(rows),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Terminals" }),
        }
    }

    /// Attach to a terminal, returning its replay snapshot.
    ///
    /// Safe to call again on a terminal already attached — that is exactly how a
    /// client recovers from a `Gapped` push, and the host serves the fresh
    /// snapshot without opening a second stream.
    pub async fn term_attach(&self, pty_id: &str) -> Result<TerminalAttached> {
        let req = Request::TermAttach { pty_id: pty_id.to_string() };
        match self.call(req).await? {
            Response::TermAttached { replay, cols, rows } => {
                Ok(TerminalAttached { replay, cols, rows })
            }
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "TermAttached" }),
        }
    }

    /// Send keystrokes. Refused for a read-only device.
    pub async fn term_input(&self, pty_id: &str, bytes: &[u8]) -> Result<()> {
        let req = Request::TermInput { pty_id: pty_id.to_string(), bytes: bytes.to_vec() };
        match self.call(req).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }

    /// Resize the terminal's grid. Refused for a read-only device — the size is
    /// shared with every other attachment, including the desktop's own window.
    pub async fn term_resize(&self, pty_id: &str, cols: u16, rows: u16) -> Result<()> {
        let req = Request::TermResize { pty_id: pty_id.to_string(), cols, rows };
        match self.call(req).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }

    /// Stop streaming a terminal. Idempotent host-side.
    pub async fn term_detach(&self, pty_id: &str) -> Result<()> {
        let req = Request::TermDetach { pty_id: pty_id.to_string() };
        match self.call(req).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }
}
