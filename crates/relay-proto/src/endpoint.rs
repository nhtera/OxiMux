//! The endpoint-naming contract: how a relay socket path becomes the name both
//! ends actually bind and dial.
//!
//! This is a pure string mapping on purpose. The *convention* has to be shared
//! — if the two sides disagree, the client dials a name nothing is listening on
//! and the failure looks like "daemon is down" rather than "we named it
//! differently". The transport machinery does not have to be shared, so this
//! crate stays free of tokio and of any socket library.

use std::path::Path;

/// Whether a relay endpoint is a filesystem path or a kernel-namespace name.
///
/// The two are not interchangeable: a filesystem socket is protected by its
/// directory's permissions and is invisible to other users who cannot traverse
/// there, while a namespaced pipe is globally visible and needs an explicit
/// ACL to get anywhere near the same guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A socket file at this path (Unix domain sockets).
    FsPath(std::path::PathBuf),
    /// A name in the system's flat pipe namespace (Windows `\\.\pipe\`).
    Namespaced(String),
}

/// Map a relay socket path to the endpoint both ends use.
///
/// Unix keeps the path verbatim: the socket file is the endpoint, its directory
/// already restricts access, and existing installs have this path recorded in
/// supervisor state.
///
/// Windows has no socket files. `\\.\pipe\` is a single flat namespace shared by
/// every session on the machine, so only a name survives the translation — and
/// because the supervisor's path is the only per-user input available, the name
/// has to encode it or two users would collide on one pipe. See
/// [`namespaced_name`] for how that is done and what it does *not* buy.
pub fn endpoint_for(socket_path: &Path) -> Endpoint {
    if cfg!(windows) {
        Endpoint::Namespaced(namespaced_name(socket_path))
    } else {
        Endpoint::FsPath(socket_path.to_path_buf())
    }
}

/// Build the flat-namespace pipe name for `socket_path`.
///
/// Shape: `<file stem>-<16 hex digits of the parent directory>`, e.g.
/// `relay-v7-3a9f21c8b40de756`. The stem keeps the name recognizable in Process
/// Explorer and in logs; the digest of the parent directory makes it per-user,
/// since the supervisor roots that directory under the user's local app-data.
///
/// The digest is a uniqueness device, NOT a secret and NOT an access control —
/// anyone can compute it, and the pipe name is world-readable regardless. What
/// keeps another account out is the owner-only DACL on the pipe plus the token
/// handshake. All this prevents is two users' relays fighting over one name.
///
/// The parent directory is lowercased first because Windows paths compare
/// case-insensitively: without it, two call sites that spelled the same
/// directory differently would derive different names and silently fail to find
/// each other.
pub fn namespaced_name(socket_path: &Path) -> String {
    let stem = socket_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("relay");
    let parent = socket_path
        .parent()
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    format!("{stem}-{:016x}", fnv1a64(parent.as_bytes()))
}

/// FNV-1a, written out rather than pulled from `DefaultHasher`.
///
/// `DefaultHasher`'s output is explicitly not guaranteed stable across Rust
/// releases. Both ends normally run from the same build, so an unstable hash
/// would agree — right up until an app update lands mid-session and the newly
/// spawned client can no longer find the daemon still holding every live PTY.
/// This function is frozen by definition, so that cannot happen.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a socket path with the running platform's own separators.
    ///
    /// Windows-style literals must not appear in these tests: `\` is an
    /// ordinary character on unix, so `Path` would see the whole literal as one
    /// component with no parent, and every assertion below would be checking
    /// something other than what it claims to.
    fn socket_path(dirs: &[&str], file: &str) -> PathBuf {
        let mut p = PathBuf::new();
        for d in dirs {
            p.push(d);
        }
        p.push(file);
        p
    }

    #[test]
    fn unix_keeps_the_socket_path_verbatim() {
        // Existing installs have this exact path in supervisor state; changing
        // the mapping would strand every live daemon.
        if !cfg!(windows) {
            let path = socket_path(&["/Users", "me", "runtime"], "relay-v7.sock");
            assert_eq!(endpoint_for(&path), Endpoint::FsPath(path));
        }
    }

    #[test]
    fn windows_maps_to_a_namespaced_name() {
        if cfg!(windows) {
            let path = socket_path(&["C:\\Users", "me", "AppData", "Local"], "relay-v7.sock");
            assert!(matches!(endpoint_for(&path), Endpoint::Namespaced(_)));
        }
    }

    #[test]
    fn name_keeps_the_stem_so_it_stays_recognizable() {
        let path = socket_path(&["home", "me", "app"], "relay-v7.sock");
        let name = namespaced_name(&path);
        assert!(name.starts_with("relay-v7-"), "got {name}");
    }

    #[test]
    fn different_users_get_different_names() {
        // The whole reason the parent directory is in the digest: two accounts
        // must not land on one pipe and fight over FIRST_PIPE_INSTANCE.
        let a = namespaced_name(&socket_path(&["home", "alice", "app"], "relay-v7.sock"));
        let b = namespaced_name(&socket_path(&["home", "bob", "app"], "relay-v7.sock"));
        assert_ne!(a, b);
    }

    #[test]
    fn the_same_path_always_derives_the_same_name() {
        let path = socket_path(&["home", "me", "app"], "relay-v7.sock");
        assert_eq!(namespaced_name(&path), namespaced_name(&path));
    }

    #[test]
    fn directory_case_does_not_change_the_name() {
        // Windows paths compare case-insensitively, so two call sites spelling
        // the same directory differently must still meet on one pipe.
        let lower = namespaced_name(&socket_path(&["home", "me", "app"], "relay-v7.sock"));
        let upper = namespaced_name(&socket_path(&["Home", "Me", "App"], "relay-v7.sock"));
        assert_eq!(lower, upper);
    }

    #[test]
    fn a_bumped_socket_version_derives_a_fresh_name() {
        // The `relay-vN` bump is how a protocol change sheds an incompatible
        // daemon. That only works if the name changes with it.
        let v7 = namespaced_name(&socket_path(&["home", "me", "app"], "relay-v7.sock"));
        let v8 = namespaced_name(&socket_path(&["home", "me", "app"], "relay-v8.sock"));
        assert_ne!(v7, v8);
    }

    #[test]
    fn fnv1a_matches_the_published_vectors() {
        // Frozen by definition — these are the reference values for FNV-1a/64.
        // If this test ever fails, every running daemon just became unreachable.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
