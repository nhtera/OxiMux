// RelaySupervisor — ensures an `oximux-relay` daemon is alive and
// returns a connected `RelayClient`. On every app launch:
//
// 1. Try a quick socket-connect to the canonical path. If the
//    handshake succeeds, an existing daemon is alive — reuse it.
// 2. Else, unlink the (possibly stale) socket, generate a fresh
//    token, spawn the daemon detached, then poll-connect until it
//    starts listening.
//
// Detach recipe (Unix): `process_group(0)` (own session group, so
// SIGHUP on the app's PG doesn't reach the daemon) + redirect stdio
// to /dev/null + log file + `mem::forget(child)` so the kernel
// reparents to PID 1 when the app dies. No `waitpid` — no zombie.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oximux_relay_client::{ClientError, RelayClient};
use oximux_relay_proto::ErrCode;
use thiserror::Error;
use tokio::net::UnixStream;
use uuid::Uuid;

/// Typed boot outcome so the caller can branch on `VersionMismatch` —
/// per phase-07 spec, that path must NOT auto-respawn (a running daemon
/// from another build may belong to other windows).
#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("relay version mismatch with running daemon")]
    VersionMismatch,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

const SOCKET_FILENAME: &str = "relay-v1.sock";
const TOKEN_FILENAME: &str = "relay-v1.token";
const PID_FILENAME: &str = "relay-v1.pid";

const HANDSHAKE_QUICK_TIMEOUT: Duration = Duration::from_millis(500);
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_READY_POLL_INTERVAL: Duration = Duration::from_millis(40);

pub struct RelaySupervisor {
    runtime_dir: PathBuf,
    log_dir: PathBuf,
}

impl RelaySupervisor {
    // `runtime_dir` is the app's data dir (e.g.
    // `~/Library/Application Support/dev.nhtera.oximux`). `log_dir`
    // is where the daemon's stdout/stderr lands (e.g.
    // `~/Library/Logs/dev.nhtera.oximux`).
    pub fn new(runtime_dir: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            runtime_dir,
            log_dir,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join(SOCKET_FILENAME)
    }

    pub fn token_path(&self) -> PathBuf {
        self.runtime_dir.join(TOKEN_FILENAME)
    }

    pub fn pid_path(&self) -> PathBuf {
        self.runtime_dir.join(PID_FILENAME)
    }

    pub fn log_path(&self) -> PathBuf {
        self.log_dir.join("relay.log")
    }

    /// Read the daemon's PID from the on-disk pid file. Returns `None`
    /// when the file is missing or unparseable — supervisor callers
    /// treat that as "we can't watch, skip the heartbeat" rather than
    /// fatal.
    pub fn read_pid(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.pid_path()).ok()?;
        raw.trim().parse().ok()
    }

    /// Spawn a per-second `kill(pid, 0)` heartbeat. The task exits as
    /// soon as the PID is no longer alive, invoking `on_death` once.
    /// Caller holds the returned join handle to abort the watch on
    /// app shutdown.
    pub fn watch_pid<F>(&self, pid: u32, on_death: F) -> tokio::task::JoinHandle<()>
    where
        F: FnOnce() + Send + 'static,
    {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if !pid_alive(pid) {
                    tracing::warn!(pid, "relay daemon PID no longer alive");
                    on_death();
                    return;
                }
            }
        })
    }

    pub async fn ensure_running(&self) -> Result<RelayClient, SupervisorError> {
        std::fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("create runtime dir {}", self.runtime_dir.display()))
            .map_err(SupervisorError::Other)?;
        std::fs::create_dir_all(&self.log_dir)
            .with_context(|| format!("create log dir {}", self.log_dir.display()))
            .map_err(SupervisorError::Other)?;

        // Path 1: existing daemon answers quickly. Best case — no
        // spawn, no probe delay. VersionMismatch propagates as a
        // typed error so the caller can surface a banner without
        // killing a daemon that other windows may still own.
        match self.try_connect_existing().await {
            ExistingConnect::Ok(client) => {
                tracing::info!("relay already running, reusing");
                return Ok(client);
            }
            ExistingConnect::VersionMismatch => {
                tracing::warn!("running relay daemon speaks a different protocol version");
                return Err(SupervisorError::VersionMismatch);
            }
            ExistingConnect::Absent => {}
        }

        // Path 2: spawn fresh. Generate a new token first so the
        // existing token file (if any) is invalidated atomically.
        let _ = std::fs::remove_file(self.socket_path());
        let _ = std::fs::remove_file(self.pid_path());
        let token = generate_token();
        write_token(&self.token_path(), &token).map_err(SupervisorError::Other)?;
        let binary = resolve_binary_path().map_err(SupervisorError::Other)?;
        spawn_detached(
            &binary,
            &self.socket_path(),
            &self.token_path(),
            &self.pid_path(),
            &self.log_dir,
            &self.log_path(),
        )
        .map_err(SupervisorError::Other)?;
        tracing::info!(
            binary = %binary.display(),
            socket = %self.socket_path().display(),
            "spawned oximux-relay detached"
        );

        // Path 3: wait for the daemon to start listening. The
        // socket-file-exists check isn't sufficient — bind happens
        // before accept in tokio, so we connect-retry as the real
        // readiness signal.
        let deadline = std::time::Instant::now() + SPAWN_READY_TIMEOUT;
        loop {
            match RelayClient::connect(&self.socket_path(), &token).await {
                Ok(client) => return Ok(client),
                Err(e) if is_version_mismatch(&e) => {
                    return Err(SupervisorError::VersionMismatch);
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(SPAWN_READY_POLL_INTERVAL).await;
                }
                Err(e) => {
                    return Err(SupervisorError::Other(anyhow::anyhow!(
                        "relay never became reachable: {e}"
                    )));
                }
            }
        }
    }

    async fn try_connect_existing(&self) -> ExistingConnect {
        // Token file presence is a proxy for "we believe a daemon
        // was started"; absence means a fresh boot or a clean
        // uninstall. Skip the connect attempt to keep the boot path
        // free of stray socket errors in the common case.
        let Ok(token) = std::fs::read_to_string(self.token_path()) else {
            return ExistingConnect::Absent;
        };
        let token = token.trim();
        if token.is_empty() {
            return ExistingConnect::Absent;
        }
        // Quick TCP-level reachability before paying for Hello.
        // tokio::UnixStream::connect returns immediately if the
        // socket file is missing or refused.
        let reachable = tokio::time::timeout(
            HANDSHAKE_QUICK_TIMEOUT,
            UnixStream::connect(self.socket_path()),
        )
        .await;
        if !matches!(reachable, Ok(Ok(_))) {
            return ExistingConnect::Absent;
        }
        // Now the full handshake — verifies the daemon also speaks
        // our protocol version and accepts the token.
        match tokio::time::timeout(
            HANDSHAKE_QUICK_TIMEOUT,
            RelayClient::connect(&self.socket_path(), token),
        )
        .await
        {
            Ok(Ok(c)) => ExistingConnect::Ok(c),
            Ok(Err(e)) if is_version_mismatch(&e) => ExistingConnect::VersionMismatch,
            _ => ExistingConnect::Absent,
        }
    }
}

enum ExistingConnect {
    Ok(RelayClient),
    VersionMismatch,
    Absent,
}

fn is_version_mismatch(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Daemon {
            code: ErrCode::VersionMismatch,
            ..
        }
    )
}

// `kill(pid, 0)` semantics: returns 0 if the process is alive AND we
// can signal it; ESRCH if it's gone; EPERM if we lack permission
// (process exists but is owned by someone else — treat as alive).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid_i32) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(2) with sig=0 has no side effects; success
        // means the process exists, failure means errno tells us
        // why. We read errno through `std::io::Error::last_os_error`
        // instead of the raw `__errno_location()` pointer so the
        // probe stays portable + safe (and works on FreeBSD).
        let r = unsafe { libc::kill(pid_i32, 0) };
        if r == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

// 32 random bytes hex-encoded (64 chars). UUIDs use the OS CSPRNG so
// chaining two gives 256 bits of entropy — well over what's needed
// to authenticate a local-only socket.
pub fn generate_token() -> String {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(a.as_bytes());
    bytes[16..].copy_from_slice(b.as_bytes());
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
        s
    })
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open token file {}", path.display()))?;
    f.write_all(token.as_bytes()).context("write token bytes")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o600);
        f.set_permissions(perm)?;
    }
    Ok(())
}

// Where to find the `oximux-relay` binary. Resolution order:
// 1. `OXIMUX_RELAY_BINARY` env var (tests + power-user override).
// 2. Sibling of the current executable. Works for both dev
//    (`target/debug/oximux` → `target/debug/oximux-relay`) and prod
//    (`OxiMux.app/Contents/MacOS/oximux` → same dir +
//    `/oximux-relay`).
pub fn resolve_binary_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("OXIMUX_RELAY_BINARY") {
        let p = PathBuf::from(p);
        if !p.exists() {
            bail!("OXIMUX_RELAY_BINARY={} does not exist", p.display());
        }
        return Ok(p);
    }
    let me = std::env::current_exe().context("current_exe")?;
    let parent = me.parent().context("current_exe has no parent")?;
    let candidate = parent.join("oximux-relay");
    if !candidate.exists() {
        bail!(
            "expected oximux-relay binary at {} (override with OXIMUX_RELAY_BINARY)",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn spawn_detached(
    binary: &Path,
    socket: &Path,
    token_file: &Path,
    pid_file: &Path,
    log_dir: &Path,
    log_file: &Path,
) -> Result<()> {
    let log_handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .with_context(|| format!("open log {}", log_file.display()))?;

    let mut cmd = Command::new(binary);
    cmd.arg("--socket")
        .arg(socket)
        .arg("--token")
        .arg(token_file)
        .arg("--pid-file")
        .arg(pid_file)
        .arg("--log-dir")
        .arg(log_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_handle.try_clone()?))
        .stderr(Stdio::from(log_handle));

    #[cfg(unix)]
    {
        cmd.process_group(0);
        unsafe {
            cmd.pre_exec(|| {
                // Belt-and-suspenders: the daemon writes its own
                // socket file with default umask. Force the
                // restrictive mask in the child's pre-exec so
                // anything created in the daemon's lifetime is 0600.
                libc::umask(0o077);
                Ok(())
            });
        }
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;
    // Critical: do NOT keep the Child or call .wait(). The kernel
    // reparents the daemon to PID 1 when the app exits; tracking
    // parent-child waitpid state would block on app exit until the
    // daemon also exits — defeating the whole point of detach.
    std::mem::forget(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_tokens_are_distinct() {
        // UUIDv4 collision is astronomically unlikely; this test
        // catches gross mistakes (e.g. always returning the same
        // value, or seeding from a constant).
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn write_token_sets_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tok");
        write_token(&path, "deadbeef").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn resolve_binary_uses_env_override_when_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = dir.path().join("oximux-relay-fake");
        std::fs::write(&fake, "").unwrap();
        // SAFETY: tests run in a single-threaded harness when using
        // serial-test, but cargo test parallelism could race here.
        // The env-var assertion is just enough to prove the override
        // is consulted; we don't depend on the binary actually being
        // executable.
        unsafe {
            std::env::set_var("OXIMUX_RELAY_BINARY", &fake);
        }
        let resolved = resolve_binary_path().unwrap();
        unsafe {
            std::env::remove_var("OXIMUX_RELAY_BINARY");
        }
        assert_eq!(resolved, fake);
    }

    #[test]
    fn supervisor_paths_under_runtime_dir() {
        let s = RelaySupervisor::new(PathBuf::from("/tmp/runtime"), PathBuf::from("/tmp/logs"));
        assert_eq!(s.socket_path(), PathBuf::from("/tmp/runtime/relay-v1.sock"));
        assert_eq!(s.token_path(), PathBuf::from("/tmp/runtime/relay-v1.token"));
        assert_eq!(s.pid_path(), PathBuf::from("/tmp/runtime/relay-v1.pid"));
        assert_eq!(s.log_path(), PathBuf::from("/tmp/logs/relay.log"));
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        assert_eq!(s.read_pid(), None);
    }

    #[test]
    fn read_pid_parses_written_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        std::fs::write(s.pid_path(), "12345\n").unwrap();
        assert_eq!(s.read_pid(), Some(12345));
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_reports_true_for_own_pid() {
        // Phase-07 crash-heartbeat: kill(0) on our own pid is the
        // canonical "process exists" probe and must return true.
        assert!(pid_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_reports_false_for_almost_certainly_dead_pid() {
        // PID 2^31 - 1 is reserved/never assigned on macOS+Linux. The
        // probe must report dead (the alternative would silently make
        // the crash heartbeat into a no-op).
        assert!(!pid_alive(i32::MAX as u32));
    }
}
