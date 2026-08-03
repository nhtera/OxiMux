//! Local control transport: the owner-only socket the `oximux` CLI uses to
//! reach a host on the same machine.
//!
//! Trust is **two factors, never reachability alone**:
//!
//! 1. **Reachability** — the socket lives in the host's runtime directory,
//!    which is held at owner-only permissions and *verified by readback*
//!    before the listener binds ([`secure`]). On Windows the pipe carries an
//!    explicit owner-only security descriptor instead, because the pipe
//!    namespace has no containing directory.
//! 2. **A bearer token** — proven by the relay's HMAC-over-nonces handshake
//!    ([`hello`]): the token itself never crosses the socket, the host proves
//!    it holds the token *first*, and the comparison is constant-time. A
//!    caller that cannot prove the token is refused before any RPC flows.
//!
//! The scope a connection is granted (operator-level, or confined to one agent
//! session) follows from **which credential proved out**, never from anything
//! the caller declares: the listener registers one secret per identity, and a
//! connection earns that identity's scope by proving its secret. Naming an
//! identity you cannot prove fails the handshake and grants nothing.
//!
//! # Threat boundary
//!
//! Agent processes get a per-session secret in their environment at spawn, so
//! an agent that misuses the protocol — asking for operator scope, or naming
//! another session — is refused. What this does **not** stop is an agent that
//! goes around the protocol: it runs as the same OS user as the desktop, so it
//! can read the operator token file and present that instead. File permissions
//! cannot separate two processes of one user, and closing this needs OS-level
//! isolation for agent children (a macOS sandbox profile, a separate uid, or a
//! namespace). That work is not in this crate; treat local access as a surface
//! that assumes the agents the desktop spawns are not actively hostile to their
//! own host.

mod dial;
mod hello;
mod secure;
mod serve;
mod transport;

pub use dial::{DialError, credential, dial, dial_as};
pub use hello::{LocalClaim, LocalIdentity};
pub use secure::{generate_token, read_token_file, write_token_file};
pub use serve::LocalControlListener;
pub use transport::{LocalSocketTransport, MAX_FRAME};

use std::path::{Path, PathBuf};

/// Versioned like the relay's `relay-v8.*`: a wire or handshake change bumps
/// the name so an old CLI dials nothing rather than a socket it misreads.
pub const SOCKET_FILENAME: &str = "control-v1.sock";
pub const TOKEN_FILENAME: &str = "control-v1.token";

/// The environment variable an agent-spawned process carries to identify its
/// session. Part of the naming contract: the injector (the host spawning
/// agents) and the CLI must agree on it.
///
/// This names the identity; it grants nothing on its own. The authority comes
/// from [`SESSION_TOKEN_ENV_VAR`], and setting this alone reaches exactly
/// nothing — which is the point. An earlier design took this variable as the
/// scope claim itself, which meant any process able to set an environment
/// variable could pick its own confinement (or opt out of it).
pub const SESSION_ENV_VAR: &str = "OXIMUX_SESSION_ID";

/// The per-session secret injected beside [`SESSION_ENV_VAR`] at agent spawn.
///
/// In the environment rather than a file on purpose: a file under the runtime
/// dir is readable by every process running as this user, including the very
/// agent subprocesses this credential exists to confine, so a file could not
/// separate them at all. An environment variable is inherited only by the
/// child it was spawned for.
pub const SESSION_TOKEN_ENV_VAR: &str = "OXIMUX_SESSION_TOKEN";

/// The control socket's path inside a host's runtime directory.
pub fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(SOCKET_FILENAME)
}

/// The bearer-token file beside it.
pub fn token_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(TOKEN_FILENAME)
}

/// Where a host keeps its control socket by default: the desktop app's data
/// root. Lives here — the naming-contract crate — so the CLI's dial and the
/// desktop's bind cannot drift apart; the desktop's own `app_paths::data_dir`
/// computes the same path and asserts agreement by test.
///
/// `data_local_dir`, never the roaming `data_dir`: a live socket and a bearer
/// token must not follow a Windows user to another machine.
pub fn default_runtime_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("dev.nhtera.oximux"))
}
