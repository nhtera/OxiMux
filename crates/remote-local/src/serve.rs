//! The listener half: bind the control socket owner-only, authenticate each
//! connection against the token, hand back a ready [`Transport`] + the scope
//! the caller claimed. The host decides what the claim is worth.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, ToFsName as _, ToNsName as _};
use oximux_relay_proto::endpoint::{Endpoint, endpoint_for};

use crate::hello::{HelloError, LocalClaim, LocalIdentity, server_handshake};
use crate::secure;
use crate::transport::LocalSocketTransport;

/// How long a caller has to finish the credential handshake after connecting.
///
/// The handshake is the first I/O an unauthenticated peer controls, so it needs
/// a deadline of its own: without one a process that connects and then says
/// nothing holds its slot forever. Generous enough that no honest local caller
/// can reach it — this is a socket on the same machine.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The identity → credential table, shared with each connection being
/// authenticated so the handshake can run off the accept path.
///
/// The value carries the scope as well as the secret because a session
/// credential outlives the label it was minted under: an agent is handed its
/// credential at spawn, before its session id exists, so the entry is created
/// under an opaque handle and re-pointed at the real id by
/// [`LocalControlListener::bind_session`] once the agent announces it.
type Credentials = Arc<Mutex<HashMap<LocalIdentity, (String, LocalClaim)>>>;

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
    credentials: Credentials,
    /// `Some` on unix (the file to unlink on drop); Windows pipes have no node.
    socket_file: Option<PathBuf>,
    /// The (device, inode) that path carried at bind, so [`Drop`] can tell the
    /// node this listener created from one a later bind put in its place.
    #[cfg(unix)]
    socket_node: Option<(u64, u64)>,
}

/// The usable bytes in `sockaddr_un.sun_path`, including its NUL terminator.
///
/// 104 on the BSDs and macOS, 108 on Linux. Taking the smaller of the two on
/// any other unix is the safe direction: a false rejection is a clear message,
/// a false acceptance is the opaque bind failure this exists to prevent.
#[cfg(unix)]
const SUN_PATH_CAPACITY: usize = if cfg!(target_os = "linux") { 108 } else { 104 };

/// Refuse a socket path the kernel cannot hold, naming the real constraint.
///
/// `pub(crate)` so [`crate::check_data_dir`] can offer the same refusal to a
/// host that has not committed to the directory yet. One implementation, so the
/// early answer and the bind's cannot disagree.
#[cfg(unix)]
pub(crate) fn check_socket_path_length(socket_path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let len = socket_path.as_os_str().as_bytes().len();
    // `<` not `<=`: the stored path is NUL-terminated, so the last byte is
    // spoken for.
    if len < SUN_PATH_CAPACITY {
        return Ok(());
    }
    let budget = SUN_PATH_CAPACITY - 1 - crate::SOCKET_FILENAME.len() - 1; // '/' + NUL
    anyhow::bail!(
        "the data directory path is too long for a unix socket: `{}` needs {len} bytes and \
         the kernel allows {}. Use a data directory of at most {budget} characters \
         (`--data-dir`), or a shorter path to the same place.",
        socket_path.display(),
        SUN_PATH_CAPACITY - 1,
    )
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
        let socket_path = crate::socket_path(runtime_dir);
        // Checked BEFORE anything is created, because the failure it prevents is
        // unrecognisable from the other side.
        //
        // `sockaddr_un.sun_path` is a fixed 104 bytes on macOS and 108 on Linux,
        // and a data dir long enough to push the socket past it fails the bind
        // with "local socket name length exceeds capacity of sun_path" — wrapped
        // by the caller in "is another OxiMux host already serving here?", which
        // sends the reader hunting for a conflicting process that does not
        // exist. Plausible on a server, where a data dir like
        // `/var/lib/oximux/instances/<customer>/<env>/data` is ordinary.
        //
        // Named here rather than left to the OS so the message says the thing
        // the operator can act on: the path is too long, by this much.
        #[cfg(unix)]
        check_socket_path_length(&socket_path)?;

        secure::prepare_runtime_dir(runtime_dir)?;
        secure::write_token_file(&crate::token_path(runtime_dir), token)?;

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
        // Name reclamation OFF, so this listener never unlinks the socket on its
        // own. `interprocess` enables it by default and would delete whatever
        // node sits at the path when the listener drops — including one a LATER
        // bind put there, since a listener can outlive its owner (an aborted
        // accept task is dropped whenever the runtime gets to it). Unlinking is
        // this crate's job instead, and `Drop` only removes a node it can still
        // identify as its own. No-op on Windows: a pipe has no name to reclaim.
        let options = ListenerOptions::new().name(name).reclaim_name(false);
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

        let credentials = HashMap::from([(
            LocalIdentity::Operator,
            (token.to_string(), LocalClaim::Operator),
        )]);
        // Taken after the bind that created the node, so it identifies OUR
        // socket rather than whatever happened to be at the path before.
        #[cfg(unix)]
        let socket_node = node_identity(&socket_path);
        Ok(Self {
            listener,
            credentials: Arc::new(Mutex::new(credentials)),
            socket_file: cfg!(unix).then(|| socket_path),
            #[cfg(unix)]
            socket_node,
        })
    }

    /// Mint and register a credential confining its holder to `label`,
    /// returning the secret to hand that one agent process at spawn (via its
    /// environment — never a file, which every same-UID process could read).
    ///
    /// This is the half that makes the narrowing real: the agent can only
    /// reach the scope of the secret it was given, because operator scope
    /// requires the operator secret and nothing it can say substitutes for
    /// holding it.
    ///
    /// `label` is whatever the host will also put in the agent's environment.
    /// A host that already knows the session id may pass it directly; one
    /// spawning a *fresh* agent does not yet have an id to use (it arrives with
    /// the agent's own `SessionInit`), so it mints an opaque handle here and
    /// calls [`bind_session`](Self::bind_session) when the id lands. Until then
    /// the credential is scoped to the handle, which matches no session — a
    /// deliberate fail-closed window rather than a wide-open one.
    pub fn grant_session(&self, label: &str) -> String {
        let secret = crate::generate_token();
        let identity = LocalIdentity::Session(label.to_string());
        let claim = identity.granted_claim();
        self.credentials.lock().unwrap().insert(identity, (secret.clone(), claim));
        secret
    }

    /// Point an already-granted credential at the session it turned out to
    /// belong to. Connections authenticated *after* this call get the new
    /// scope; ones already open keep the scope they proved, which is the
    /// fail-closed direction (the handle scope reaches nothing).
    ///
    /// A no-op for an unknown label, so a session that ended before its agent
    /// announced itself cannot resurrect a credential.
    pub fn bind_session(&self, label: &str, session_id: &str) {
        if let Some((_, claim)) =
            self.credentials.lock().unwrap().get_mut(&LocalIdentity::Session(label.to_string()))
        {
            *claim = LocalClaim::Session(session_id.to_string());
        }
    }

    /// Drop a credential when its agent ends, so a leaked secret stops working
    /// with the process it was minted for. Keyed on the mint-time label, not
    /// the session id — the label is what the agent's environment carries.
    pub fn revoke_session(&self, label: &str) {
        self.credentials.lock().unwrap().remove(&LocalIdentity::Session(label.to_string()));
    }

    /// Accept one connection **without** authenticating it.
    ///
    /// The handshake deliberately does not run here. It is the first I/O the
    /// peer controls, so running it on the accept path lets one
    /// connected-but-silent caller hold the listener and starve every later
    /// one. The returned value owns everything the handshake needs, so a host
    /// serving concurrent callers finishes it in a per-connection task.
    pub async fn accept_pending(&self) -> Result<PendingConnection, HelloError> {
        let stream = self
            .listener
            .accept()
            .await
            .map_err(|e| HelloError::Transport(e.to_string()))?;
        let (recv, send) = stream.split();
        Ok(PendingConnection {
            transport: Arc::new(LocalSocketTransport::new(send, recv)),
            credentials: Arc::clone(&self.credentials),
        })
    }

    /// Accept one connection and run the credential handshake on it. `Ok` hands
    /// back the framed transport and the scope **the proven credential earns**;
    /// a caller that fails the proof is answered and dropped, surfacing as
    /// `Err(Denied)` for the host's log — never a panic, never a served
    /// connection.
    ///
    /// Both steps in one await, for callers that serve one connection at a time.
    /// A host that serves callers concurrently wants
    /// [`accept_pending`](Self::accept_pending) instead, so a stalled handshake
    /// cannot block the next accept.
    pub async fn accept(&self) -> Result<(Arc<LocalSocketTransport>, LocalClaim), HelloError> {
        self.accept_pending().await?.authenticate().await
    }
}

/// A connected but not yet authenticated caller.
///
/// Carries its own handle on the credential table so the handshake can run in a
/// task of its own, away from the accept path.
pub struct PendingConnection {
    transport: Arc<LocalSocketTransport>,
    credentials: Credentials,
}

impl PendingConnection {
    /// Run the credential handshake, bounded by [`HANDSHAKE_TIMEOUT`]. Returns
    /// the framed transport and the scope the proven credential earns.
    pub async fn authenticate(
        self,
    ) -> Result<(Arc<LocalSocketTransport>, LocalClaim), HelloError> {
        let Self { transport, credentials } = self;
        // The lookup is a snapshot per call, so a credential revoked between
        // connections is gone for the next one.
        let handshake = server_handshake(transport.as_ref(), |identity| {
            credentials.lock().unwrap().get(identity).cloned()
        });
        let claim = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| HelloError::HandshakeTimeout)??;
        Ok((transport, claim))
    }
}

impl Drop for LocalControlListener {
    fn drop(&mut self) {
        let Some(path) = &self.socket_file else {
            return;
        };
        // Unlink only the node this listener bound. A handle owning this
        // listener aborts its accept task asynchronously, so this drop can land
        // after a later bind has already replaced the socket at the same path —
        // and removing THAT node would leave a live listener with no filesystem
        // entry, so every dial fails with ENOENT while the toggle reads on.
        #[cfg(unix)]
        if self.socket_node.is_none() || self.socket_node != node_identity(path) {
            return;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// The (device, inode) pair naming a filesystem node, or `None` if it is gone.
/// Two listeners that bound the same path in turn get different pairs, which is
/// what lets a late `Drop` tell its own node from a successor's.
#[cfg(unix)]
fn node_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
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
