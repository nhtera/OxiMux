//! The client half: read the token, connect, prove it, claim a scope.

use std::path::Path;
use std::sync::Arc;

use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{ToFsName as _, ToNsName as _};
use oximux_relay_proto::endpoint::{Endpoint, endpoint_for};

use crate::hello::{HelloError, LocalIdentity, client_handshake};
use crate::secure::read_token_file;
use crate::transport::LocalSocketTransport;

/// Why a dial failed — split so the CLI can keep its exit-code contract
/// (unreachable vs denied) without string-matching.
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// No token file / no socket: local access has not been enabled on this
    /// host, or the host is not running.
    #[error("the host is not reachable at {runtime_dir}: {reason}")]
    Unreachable { runtime_dir: String, reason: String },
    /// This process cannot present a usable credential — an agent session whose
    /// per-session secret never arrived, most of all.
    ///
    /// Separate from [`Unreachable`](Self::Unreachable) because it is an ACCESS
    /// failure, and the host may well be running and healthy. Folding the two
    /// together told a caller to check whether the app was up, and left a
    /// retry-on-unreachable wrapper retrying a credential problem forever.
    #[error("{0}")]
    NoCredential(String),
    /// The handshake refused us (bad/stale token) — or we refused the host
    /// (it could not prove the token; a squatter or a half-rotated state).
    #[error("{0}")]
    Denied(#[source] HelloError),
    /// Transport-level failure mid-handshake.
    #[error("handshake failed: {0}")]
    Handshake(#[source] HelloError),
}

/// The credential this process holds, and the identity it names.
///
/// An agent process is handed a per-session secret in its environment at
/// spawn; everything else is an operator at the keyboard and reads the token
/// file. Read here rather than passed in so no caller can accidentally
/// present the operator credential while naming a session identity.
///
/// **A process that identifies as an agent never falls back to the operator
/// token**, even when its per-session secret is missing or malformed — it gets
/// an error instead. Falling back would mean an agent whose credential failed
/// to arrive silently ran with the operator's authority, which is the exact
/// outcome the confinement exists to prevent.
///
/// What this cannot do — stated plainly because the boundary matters: an agent
/// process runs as the same OS user as the desktop, so it can *read the
/// operator token file itself* and present that. No file permission separates
/// two processes of one user; closing that needs OS-level isolation for agent
/// children (a macOS sandbox profile, a separate uid, a namespace). Until that
/// exists, this crate's confinement holds against an agent that misuses the
/// protocol, not against one that goes around it. See
/// [`oximux-remote-local`'s module docs](crate).
pub fn credential(runtime_dir: &Path) -> Result<(LocalIdentity, String), std::io::Error> {
    let session_id = std::env::var(crate::SESSION_ENV_VAR)
        .ok()
        .filter(|id| !id.trim().is_empty());
    if let Some(session_id) = session_id {
        let secret = std::env::var(crate::SESSION_TOKEN_ENV_VAR)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "${} is set but ${} is not; an agent session cannot use the \
                         operator credential",
                        crate::SESSION_ENV_VAR,
                        crate::SESSION_TOKEN_ENV_VAR
                    ),
                )
            })?;
        return Ok((LocalIdentity::Session(session_id), secret));
    }
    let token = read_token_file(&crate::token_path(runtime_dir))?;
    Ok((LocalIdentity::Operator, token))
}

/// Connect to the host at `runtime_dir` and authenticate with whichever
/// credential this process holds. The scope granted follows from that
/// credential — this side cannot ask for more than it can prove.
pub async fn dial(runtime_dir: &Path) -> Result<Arc<LocalSocketTransport>, DialError> {
    let (identity, token) = credential(runtime_dir).map_err(|e| {
        // A missing per-session secret is `PermissionDenied` from `credential`,
        // and it stays an access failure here. Only a missing/unreadable token
        // file means "no host has ever served from this directory".
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            DialError::NoCredential(e.to_string())
        } else {
            DialError::Unreachable {
                runtime_dir: runtime_dir.display().to_string(),
                reason: format!("no control credential ({e})"),
            }
        }
    })?;
    dial_as(runtime_dir, identity, &token).await
}

/// Dial presenting an explicit identity/secret pair, rather than whichever
/// credential the environment supplies.
///
/// Public so a test can present a *mismatched* pair — the escalation attempt
/// the credential model exists to refuse. Production callers use [`dial`],
/// which cannot construct a mismatch.
pub async fn dial_as(
    runtime_dir: &Path,
    identity: LocalIdentity,
    token: &str,
) -> Result<Arc<LocalSocketTransport>, DialError> {
    let unreachable = |reason: String| DialError::Unreachable {
        runtime_dir: runtime_dir.display().to_string(),
        reason,
    };

    let socket_path = crate::socket_path(runtime_dir);
    let name = match endpoint_for(&socket_path) {
        Endpoint::FsPath(path) => path
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()
            .map_err(|e| unreachable(format!("socket path unusable: {e}")))?,
        Endpoint::Namespaced(name) => name
            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
            .map_err(|e| unreachable(format!("pipe name unusable: {e}")))?,
    };
    let stream = interprocess::local_socket::tokio::Stream::connect(name)
        .await
        .map_err(|e| unreachable(e.to_string()))?;
    let (recv, send) = stream.split();
    let transport = Arc::new(LocalSocketTransport::new(send, recv));
    match client_handshake(transport.as_ref(), token, identity).await {
        Ok(()) => Ok(transport),
        Err(e @ (HelloError::Denied | HelloError::HostNotTrusted)) => Err(DialError::Denied(e)),
        Err(e) => Err(DialError::Handshake(e)),
    }
}

// The environment-driven half of `credential` is covered by
// `tests/credential_env.rs`, which is a test binary of its own. It cannot live
// here: `std::env::set_var` mutates process-global state, and libtest runs the
// tests in one binary on concurrent threads, so it would race the `getenv`
// inside every `tempfile::tempdir()` call this crate's other unit tests make.
