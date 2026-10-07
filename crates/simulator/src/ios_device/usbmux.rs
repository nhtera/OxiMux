//! A client for `usbmuxd`, macOS's USB multiplexer: it lists the iPhones on a
//! cable and opens a byte stream to a TCP port **on the phone** — how the
//! control runner, listening on the phone's loopback, is reached from here.
//!
//! The protocol: on `/var/run/usbmuxd`, each message is a 16-byte header —
//! length (header included), version 1 (plist), type 8 (plist), a tag, all
//! little-endian `u32` — then an XML plist. `ListDevices` answers a
//! `DeviceList`; `Connect` (the port **byte-swapped**, as the daemon expects)
//! answers `Result` 0, after which the same socket *is* the stream.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use plist::{Dictionary, Value};

/// Where macOS's daemon listens.
pub const SOCKET: &str = "/var/run/usbmuxd";

const VERSION_PLIST: u32 = 1;
const TYPE_PLIST: u32 = 8;
/// A reply longer than this is not one of the daemon's.
const MAX_MESSAGE: u32 = 1 << 20;
const PROG_NAME: &str = "oximux";

/// One device the daemon knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuxDevice {
    pub id: u64,
    /// The phone's UDID, as the daemon spells it.
    pub serial: String,
    /// Over USB (not the daemon's network devices).
    pub usb: bool,
}

/// Why a `Connect` failed, by the daemon's result number.
#[derive(Debug, thiserror::Error)]
pub enum UsbmuxError {
    #[error("the iPhone is not connected over USB")]
    NotConnected,
    #[error("nothing is listening on port {0} on the iPhone")]
    Refused(u16),
    #[error("usbmuxd answered {0}")]
    Result(u64),
    #[error("usbmuxd: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The daemon at a socket path (the system's, or a test's).
#[derive(Clone, Debug)]
pub struct Usbmux {
    path: PathBuf,
    timeout: Duration,
}

impl Usbmux {
    pub fn system() -> Self {
        Self::at(Path::new(SOCKET))
    }

    pub fn at(path: &Path) -> Self {
        Self { path: path.to_path_buf(), timeout: Duration::from_secs(5) }
    }

    /// Every device the daemon lists.
    pub fn devices(&self) -> Result<Vec<MuxDevice>, UsbmuxError> {
        let mut socket = self.open()?;
        send(&mut socket, 1, request("ListDevices"))?;
        let reply = receive(&mut socket)?;
        let list = reply.get("DeviceList").and_then(Value::as_array).ok_or_else(|| protocol("no DeviceList"))?;
        Ok(list.iter().filter_map(device).collect())
    }

    /// A stream to `port` on the iPhone `udid` (over USB only).
    pub fn connect(&self, udid: &str, port: u16) -> Result<UnixStream, UsbmuxError> {
        let wanted = normalize(udid);
        let device = self
            .devices()?
            .into_iter()
            .find(|d| d.usb && normalize(&d.serial) == wanted)
            .ok_or(UsbmuxError::NotConnected)?;
        let mut socket = self.open()?;
        let mut message = request("Connect");
        message.insert("DeviceID".into(), Value::Integer(device.id.into()));
        // The daemon reads the port in network byte order.
        message.insert("PortNumber".into(), Value::Integer(u64::from(port.swap_bytes()).into()));
        send(&mut socket, 2, message)?;
        let reply = receive(&mut socket)?;
        match reply.get("Number").and_then(Value::as_unsigned_integer) {
            Some(0) => {
                // The stream's own reads and writes have their own deadlines.
                socket.set_read_timeout(None)?;
                socket.set_write_timeout(None)?;
                Ok(socket)
            }
            Some(2) => Err(UsbmuxError::NotConnected),
            Some(3) => Err(UsbmuxError::Refused(port)),
            Some(n) => Err(UsbmuxError::Result(n)),
            None => Err(protocol("Connect had no result")),
        }
    }

    fn open(&self) -> Result<UnixStream, UsbmuxError> {
        let socket = UnixStream::connect(&self.path)?;
        socket.set_read_timeout(Some(self.timeout))?;
        socket.set_write_timeout(Some(self.timeout))?;
        Ok(socket)
    }
}

/// A UDID compared without its dash and case (the daemon and devicectl may
/// spell it differently).
fn normalize(udid: &str) -> String {
    udid.chars().filter(|c| *c != '-').map(|c| c.to_ascii_uppercase()).collect()
}

fn request(kind: &str) -> Dictionary {
    let mut message = Dictionary::new();
    message.insert("MessageType".into(), Value::String(kind.into()));
    message.insert("ProgName".into(), Value::String(PROG_NAME.into()));
    message.insert("ClientVersionString".into(), Value::String(format!("{PROG_NAME}-{}", env!("CARGO_PKG_VERSION"))));
    message
}

fn device(entry: &Value) -> Option<MuxDevice> {
    let entry = entry.as_dictionary()?;
    let properties = entry.get("Properties")?.as_dictionary()?;
    Some(MuxDevice {
        id: entry.get("DeviceID").or_else(|| properties.get("DeviceID"))?.as_unsigned_integer()?,
        serial: properties.get("SerialNumber")?.as_string()?.to_owned(),
        usb: properties.get("ConnectionType").and_then(Value::as_string) == Some("USB"),
    })
}

fn protocol(why: &str) -> UsbmuxError {
    UsbmuxError::Protocol(why.to_owned())
}

/// One message: the header, then `body` as an XML plist.
fn encode(tag: u32, body: Dictionary) -> Result<Vec<u8>, UsbmuxError> {
    let mut payload = Vec::new();
    plist::to_writer_xml(&mut payload, &Value::Dictionary(body)).map_err(|e| protocol(&e.to_string()))?;
    let length = u32::try_from(16 + payload.len()).map_err(|_| protocol("message too long"))?;
    let mut message = Vec::with_capacity(length as usize);
    for field in [length, VERSION_PLIST, TYPE_PLIST, tag] {
        message.extend_from_slice(&field.to_le_bytes());
    }
    message.extend_from_slice(&payload);
    Ok(message)
}

fn send(socket: &mut impl Write, tag: u32, body: Dictionary) -> Result<(), UsbmuxError> {
    socket.write_all(&encode(tag, body)?)?;
    Ok(())
}

fn receive(socket: &mut impl Read) -> Result<Dictionary, UsbmuxError> {
    let mut header = [0u8; 16];
    socket.read_exact(&mut header)?;
    let field = |i: usize| u32::from_le_bytes(header[i * 4..i * 4 + 4].try_into().unwrap_or_default());
    let length = field(0);
    if !(16..=MAX_MESSAGE).contains(&length) || field(2) != TYPE_PLIST {
        return Err(protocol("an unexpected message"));
    }
    let mut payload = vec![0u8; (length - 16) as usize];
    socket.read_exact(&mut payload)?;
    plist::from_bytes::<Value>(&payload)
        .map_err(|e| protocol(&e.to_string()))?
        .into_dictionary()
        .ok_or_else(|| protocol("not a dictionary"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    /// A stand-in daemon on a temporary socket: answers each message with
    /// `reply(request)`, then (for a `Connect` answered 0) echoes the stream.
    fn fake_daemon(reply: impl Fn(&Dictionary) -> Dictionary + Send + 'static) -> (tempfile::TempDir, Usbmux) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usbmuxd");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Ok(request) = receive(&mut stream) else { continue };
                let answer = reply(&request);
                let connected = answer.get("Number").and_then(Value::as_unsigned_integer) == Some(0);
                let _ = stream.write_all(&encode(1, answer).unwrap());
                if connected {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = stream.read(&mut buf) {
                        let _ = stream.write_all(&buf[..n]);
                    }
                }
            }
        });
        (dir, Usbmux::at(&path))
    }

    fn device_list() -> Dictionary {
        let phone = |id: u64, serial: &str, kind: &str| {
            let mut properties = Dictionary::new();
            properties.insert("SerialNumber".into(), Value::String(serial.into()));
            properties.insert("ConnectionType".into(), Value::String(kind.into()));
            let mut entry = Dictionary::new();
            entry.insert("DeviceID".into(), Value::Integer(id.into()));
            entry.insert("Properties".into(), Value::Dictionary(properties));
            Value::Dictionary(entry)
        };
        let mut reply = Dictionary::new();
        reply.insert(
            "DeviceList".into(),
            Value::Array(vec![phone(3, "00008130-000A1B2C3D4E5F60", "Network"), phone(7, "00008130000A1B2C3D4E5F60", "USB")]),
        );
        reply
    }

    #[test]
    fn messages_have_the_daemons_framing() {
        let bytes = encode(9, request("ListDevices")).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize, bytes.len());
        assert_eq!(&bytes[4..16], [1, 0, 0, 0, 8, 0, 0, 0, 9, 0, 0, 0]);
        let back = receive(&mut &bytes[..]).unwrap();
        assert_eq!(back.get("MessageType").and_then(Value::as_string), Some("ListDevices"));
    }

    #[test]
    fn devices_are_listed_and_a_usb_one_connected_to() {
        let (_dir, mux) = fake_daemon(|request| match request.get("MessageType").and_then(Value::as_string) {
            Some("ListDevices") => device_list(),
            Some("Connect") => {
                let id = request.get("DeviceID").and_then(Value::as_unsigned_integer);
                let port = request.get("PortNumber").and_then(Value::as_unsigned_integer);
                let mut reply = Dictionary::new();
                // Only the USB device (7) on port 50755, byte-swapped.
                let ok = id == Some(7) && port == Some(u64::from(50755u16.swap_bytes()));
                reply.insert("Number".into(), Value::Integer(if ok { 0u64 } else { 3u64 }.into()));
                reply
            }
            _ => Dictionary::new(),
        });
        let devices = mux.devices().unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices[1].usb && !devices[0].usb);
        // The devicectl spelling (with a dash) finds the daemon's.
        let mut stream = mux.connect("00008130-000A1B2C3D4E5F60", 50755).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut echo = [0u8; 4];
        stream.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ping");
        assert!(matches!(mux.connect("00008130-000A1B2C3D4E5F60", 1), Err(UsbmuxError::Refused(1))));
        assert!(matches!(mux.connect("FFFF", 50755), Err(UsbmuxError::NotConnected)));
    }

    #[test]
    fn a_garbled_reply_is_an_error_not_a_hang() {
        let mut bogus = Vec::new();
        for field in [MAX_MESSAGE + 1, 1, 8, 1] {
            bogus.extend_from_slice(&field.to_le_bytes());
        }
        assert!(matches!(receive(&mut &bogus[..]), Err(UsbmuxError::Protocol(_))));
    }

    /// The real daemon answers on this Mac (with or without a phone).
    #[test]
    fn the_system_daemon_lists_devices() {
        if !Path::new(SOCKET).exists() {
            return;
        }
        assert!(Usbmux::system().devices().is_ok());
    }
}
