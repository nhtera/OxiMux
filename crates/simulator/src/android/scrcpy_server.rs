//! The scrcpy server: fetching the pinned jar, starting it on the device, and
//! opening its video and control sockets through an adb forward.
//!
//! The server is scrcpy's own (Apache-2.0), downloaded on first use from the
//! pinned GitHub release and checked against its published SHA-256 — nothing
//! of scrcpy is built or bundled here. Client and server must be the exact same
//! version (the server refuses any other), so [`VERSION`] is also the protocol
//! version `scrcpy_control` / `scrcpy_video` speak.
//!
//! The only sockets are to `127.0.0.1`, where adb's own forward listens.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::scrcpy_video;
use crate::{Result, SimError};

/// The scrcpy release whose server and protocol we speak.
pub const VERSION: &str = "4.1";
/// `scrcpy-server-v4.1` from the release's `SHA256SUMS.txt`.
pub const JAR_SHA256: &str = "deacb991ed2509715160ffdc7907e47b4160eb30d1566217e9047fd5b8850cae";
const JAR_URL: &str = "https://github.com/Genymobile/scrcpy/releases/download/v4.1/scrcpy-server-v4.1";
/// Where the jar goes on the device: readable by `shell`, not world-writable.
pub const DEVICE_JAR: &str = "/data/local/tmp/scrcpy-server.jar";

/// How long the server may take to accept after it is started.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// What to stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    /// Longest side of the video, in pixels (0: the device's own size).
    pub max_size: u16,
    pub max_fps: u16,
}

/// A 31-bit id naming this connection's socket on the device, so two clients
/// starting at once never collide.
pub fn new_scid() -> u32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    (nanos ^ std::process::id().rotate_left(13)) & 0x7fff_ffff
}

/// The device-side socket name for `scid`.
pub fn socket_name(scid: u32) -> String {
    format!("scrcpy_{scid:08x}")
}

/// The command that runs the server on the device, after `adb -s <serial>`.
pub fn server_command(scid: u32, opts: StreamOptions) -> Vec<String> {
    let mut args: Vec<String> = ["shell", &format!("CLASSPATH={DEVICE_JAR}"), "app_process", "/", "com.genymobile.scrcpy.Server", VERSION]
        .into_iter()
        .map(str::to_owned)
        .collect();
    args.extend([
        format!("scid={scid:08x}"),
        "log_level=warn".into(),
        "audio=false".into(),
        "video_codec=h264".into(),
        format!("max_size={}", opts.max_size),
        format!("max_fps={}", opts.max_fps),
        "tunnel_forward=true".into(),
        // The phone's own screen stays as the user left it.
        "power_on=false".into(),
        // Nothing reads device messages from the control socket.
        "clipboard_autosync=false".into(),
    ]);
    args
}

/// Whether `bytes` is the pinned jar.
pub fn is_pinned_jar(bytes: &[u8]) -> bool {
    hex(&Sha256::digest(bytes)) == JAR_SHA256
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The pinned jar in `cache_dir`, downloading and verifying it on first use.
/// A cached file that does not match the pin is replaced.
pub fn ensure_jar(cache_dir: &Path) -> Result<PathBuf> {
    let path = cache_dir.join(format!("scrcpy-server-v{VERSION}.jar"));
    if std::fs::read(&path).is_ok_and(|b| is_pinned_jar(&b)) {
        return Ok(path);
    }
    let bytes = download(JAR_URL)?;
    if !is_pinned_jar(&bytes) {
        return Err(SimError::HelperFailed("the downloaded scrcpy server did not match its published checksum".into()));
    }
    std::fs::create_dir_all(cache_dir)?;
    // Unique per process and call: two first-use downloads never share it.
    let tmp = path.with_extension(format!("jar.{}.{}.tmp", std::process::id(), new_scid()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new().timeout(DOWNLOAD_TIMEOUT).build();
    let response = agent
        .get(url)
        .call()
        .map_err(|e| SimError::HelperFailed(format!("could not download the scrcpy server: {e}")))?;
    let mut bytes = Vec::new();
    response.into_reader().take(16 * 1024 * 1024).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A running server and its two sockets.
pub struct Connection {
    pub video: TcpStream,
    pub control: TcpStream,
    /// The device's name, as the server reported it.
    pub device_name: String,
    /// `adb shell … app_process …`: the server lives as long as this does.
    pub server: Child,
    /// The local port of the adb forward (removed on close).
    pub port: u16,
}

/// Start the server (`adb` and the device's `serial` are already known, the
/// jar pushed, the forward to [`socket_name`] set up on `port`) and open its
/// video then control socket. adb accepts a connection even when nothing
/// listens on the device yet, so a connection only counts once the server's
/// dummy byte arrives; until then it is retried.
pub fn start(adb: &Path, serial: &str, scid: u32, opts: StreamOptions, port: u16) -> Result<Connection> {
    let mut server = Command::new(adb)
        .arg("-s")
        .arg(serial)
        .args(server_command(scid, opts))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let opened = open_sockets(port, deadline);
    match opened {
        Ok((video, control, device_name)) => Ok(Connection { video, control, device_name, server, port }),
        Err(e) => {
            let _ = server.kill();
            let _ = server.wait();
            Err(e)
        }
    }
}

fn open_sockets(port: u16, deadline: Instant) -> Result<(TcpStream, TcpStream, String)> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)).and_then(|mut video| {
            video.set_read_timeout(Some(Duration::from_millis(1500)))?;
            scrcpy_video::read_dummy_byte(&mut video).map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(video)
        }) {
            Ok(mut video) => {
                // The server is there. The name follows once control connects.
                let control = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
                control.set_nodelay(true)?;
                video.set_read_timeout(Some(Duration::from_secs(5)))?;
                let device_name = scrcpy_video::read_device_name(&mut video)?;
                video.set_read_timeout(None)?;
                return Ok((video, control, device_name));
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => {
                return Err(SimError::Timeout { what: format!("the scrcpy server ({e})"), secs: CONNECT_TIMEOUT.as_secs() });
            }
        }
    }
}

/// Send one control message.
pub fn send(control: &mut TcpStream, msg: &super::scrcpy_control::ControlMsg) -> Result<()> {
    control.write_all(&msg.serialize())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_command_matches_the_pinned_version() {
        let cmd = server_command(0x1234abcd, StreamOptions { max_size: 1280, max_fps: 60 });
        assert_eq!(&cmd[..6], ["shell", "CLASSPATH=/data/local/tmp/scrcpy-server.jar", "app_process", "/", "com.genymobile.scrcpy.Server", "4.1"]);
        for want in ["scid=1234abcd", "audio=false", "video_codec=h264", "max_size=1280", "max_fps=60", "tunnel_forward=true"] {
            assert!(cmd.iter().any(|a| a == want), "{want} in {cmd:?}");
        }
        assert_eq!(socket_name(0x1234abcd), "scrcpy_1234abcd");
        assert!(new_scid() <= 0x7fff_ffff);
    }

    #[test]
    fn only_the_pinned_jar_is_accepted() {
        assert!(!is_pinned_jar(b"not the server"));
        assert_eq!(JAR_SHA256.len(), 64);
    }

    /// A listener that accepts but never speaks — what adb's forward looks
    /// like before the server is up — is not a connection.
    #[test]
    fn a_silent_forward_is_not_a_server() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = open_sockets(port, Instant::now() + Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, SimError::Timeout { .. }), "{err}");
    }

    /// Once the server's dummy byte and name arrive, both sockets open.
    #[test]
    fn a_real_answer_opens_video_then_control() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // As `DesktopConnection` does: the dummy byte on accept, the
            // name only after the control socket is accepted too.
            let (mut video, _) = listener.accept().unwrap();
            video.write_all(&[0]).unwrap();
            let (_control, _) = listener.accept().unwrap();
            let mut name = [0u8; scrcpy_video::DEVICE_NAME_LEN];
            name[..5].copy_from_slice(b"Pixel");
            video.write_all(&name).unwrap();
            video
        });
        let (_video, _control, name) = open_sockets(port, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(name, "Pixel");
        server.join().unwrap();
    }
}
