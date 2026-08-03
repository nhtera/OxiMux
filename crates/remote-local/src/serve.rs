//! The listener half: bind the control socket owner-only, authenticate each
//! connection against the token, hand back a ready [`Transport`] + the scope
//! the caller claimed. The host decides what the claim is worth.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, ToFsName as _, ToNsName as _};
use oximux_relay_proto::endpoint::{Endpoint, endpoint_for};

use crate::hello::{HelloError, LocalClaim, LocalIdentity, server_handshake};
use crate::secure;
use crate::transport::LocalSocketTransport;

/// A bound control-socket listener and the credentials it authenticates
/// against. Dropping it closes the socket; the socket file is removed so a
/// later bind never trips over the stale node.
///
/// **Scope is a property of the credential, not of the caller's word.** The
/// table maps each registered identity to its own secret, and a connection
/// earns exactly the scope of whichever secret it proved. That is what makes
/// an agent's confinement hold against an adversarial agent rather than only a
/// cooperative one: to reach operator scope it would have to hold the operator
/// secret, not merely ask for it.
pub struct LocalControlListener {
    listener: interprocess::local_socket::tokio::Listener,
    /// identity → its secret. Seeded with the operator credential at bind;
    /// per-session credentials are added by [`grant_session`] as agents spawn.
    ///
    /// [`grant_session`]: Self::grant_session
    credentials: Mutex<HashMap<LocalIdentity, String>>,
    /// `Some` on unix (the file to unlink on drop); Windows pipes have no node.
    socket_file: Option<PathBuf>,
}

impl LocalControlListener {
    /// Prepare the runtime directory (owner-only, readback-verified), write
    /// `token` beside the socket (owner-only, readback-verified), and bind.
    ///
    /// Order matters and is the relay's: the token file exists and is
    /// restricted **before** the socket is reachable, so there is no window
    /// where a caller can connect against a token that is not yet the one on
    /// disk.
    pub fn bind(runtime_dir: &Path, token: &str) -> Result<Self> {
        secure::prepare_runtime_dir(runtime_dir)?;
        secure::write_token_file(&crate::token_path(runtime_dir), token)?;

        let socket_path = crate::socket_path(runtime_dir);
        // A stale node from a crashed host would make bind fail with
        // AddrInUse; nothing can be listening on it (we are the host).
        #[cfg(unix)]
        let _ = std::fs::remove_file(&socket_path);

        let name = match endpoint_for(&socket_path) {
            Endpoint::FsPath(path) => path
                .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                .with_context(|| format!("socket path unusable: {}", socket_path.display()))?,
            Endpoint::Namespaced(name) => name
                .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
                .context("derived pipe name unusable")?,
        };
        let options = ListenerOptions::new().name(name);
        // `?`, never a fallback: an unprotected pipe must not exist at all.
        #[cfg(windows)]
        let options = {
            use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
            options.security_descriptor(owner_only_descriptor()?)
        };
        let listener = options
            .create_tokio()
            .with_context(|| format!("bind {}", socket_path.display()))?;

        // Bind creates the node; restrict it and take the readback receipt.
        #[cfg(unix)]
        secure::restrict_socket(&socket_path)?;

        let credentials =
            HashMap::from([(LocalIdentity::Operator, token.to_string())]);
        Ok(Self {
            listener,
            credentials: Mutex::new(credentials),
            socket_file: cfg!(unix).then(|| socket_path),
        })
    }

    /// Mint and register a credential confining its holder to `session_id`,
    /// returning the secret to hand that one agent process at spawn (via its
    /// environment — never a file, which every same-UID process could read).
    ///
    /// This is the half that makes the narrowing real: the agent can only
    /// reach the scope of the secret it was given, because operator scope
    /// requires the operator secret and nothing it can say substitutes for
    /// holding it.
    pub fn grant_session(&self, session_id: &str) -> String {
        let secret = crate::generate_token();
        self.credentials
            .lock()
            .unwrap()
            .insert(LocalIdentity::Session(session_id.to_string()), secret.clone());
        secret
    }

    /// Drop a session's credential when its agent ends, so a leaked secret
    /// stops working with the process it was minted for.
    pub fn revoke_session(&self, session_id: &str) {
        self.credentials
            .lock()
            .unwrap()
            .remove(&LocalIdentity::Session(session_id.to_string()));
    }

    /// Accept one connection and run the credential handshake. `Ok` hands back
    /// the framed transport and the scope **the proven credential earns**; a
    /// caller that fails the proof is answered and dropped, surfacing as
    /// `Err(Denied)` for the host's log — never a panic, never a served
    /// connection.
    pub async fn accept(&self) -> Result<(Arc<LocalSocketTransport>, LocalClaim), HelloError> {
        let stream = self
            .listener
            .accept()
            .await
            .map_err(|e| HelloError::Transport(e.to_string()))?;
        let (recv, send) = stream.split();
        let transport = Arc::new(LocalSocketTransport::new(send, recv));
        // The lookup is a snapshot per call, so a credential revoked between
        // connections is gone for the next one.
        let claim = server_handshake(transport.as_ref(), |identity| {
            self.credentials.lock().unwrap().get(identity).cloned()
        })
        .await?;
        Ok((transport, claim))
    }
}

impl Drop for LocalControlListener {
    fn drop(&mut self) {
        if let Some(path) = &self.socket_file {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Owner-only security descriptor for the Windows pipe — one definition of
/// "only this account", from `owner-only`, exactly as the relay's pipe does.
#[cfg(windows)]
fn owner_only_descriptor()
-> Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    let sddl = oximux_owner_only::owner_only_sddl().context("build owner-only SDDL")?;
    let wide = widestring::U16CString::from_str(&sddl)
        .context("security descriptor string is not valid UTF-16")?;
    interprocess::os::windows::security_descriptor::SecurityDescriptor::deserialize(&wide)
        .with_context(|| format!("parse security descriptor {sddl}"))
}
