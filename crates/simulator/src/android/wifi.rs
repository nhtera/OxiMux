//! Wireless debugging (Android 11+): pairing a phone over Wi-Fi with its
//! six-digit code or a QR code, then connecting to it.
//!
//! Secrets (the pairing code, the QR password) go to `adb` on **stdin**, never
//! on its command line: argv shows in `ps`, and a timeout's error names the
//! command it ran. OxiMux remembers no address — the connect port changes
//! whenever wireless debugging is turned on again; adb's own mDNS
//! auto-connect brings back a phone paired over TLS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rand::Rng as _;
use rand::rngs::OsRng;

use super::adb::Adb;
use crate::{Result, SimError};

/// The mDNS service a phone advertises while "Pair device with QR code" is
/// open (its instance is the QR's service name).
pub const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp";
/// The mDNS service of a phone that can be connected to (paired over TLS).
pub const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp";

/// How long QR pairing waits for the phone to scan the code.
pub const QR_WAIT: Duration = Duration::from_secs(120);
/// How long a pairing waits for the phone's connect service to show up.
pub const CONNECT_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(1);
const ADB_QUICK: Duration = Duration::from_secs(10);
/// `adb pair` waits on the phone (TLS + SPAKE2), a few seconds at most.
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// What macOS says when it blocks the adb server's local network access.
pub const LOCAL_NETWORK_BLOCKED: &str = "macOS blocked local network access for the adb server. Allow it in \
     System Settings › Privacy & Security › Local Network, then try again.";

/// One line of `adb mdns services`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdnsService {
    /// e.g. `adb-R58M123ABC-x1Y2z3` or a QR pairing's `oximux-ab12cd`.
    pub instance: String,
    /// e.g. [`CONNECT_SERVICE`].
    pub service: String,
    /// `ip:port`.
    pub addr: String,
}

/// `adb mdns services`: `<instance>\t<service>\t<ip:port>` per line (older
/// builds pad with spaces), after a header.
pub fn parse_mdns_services(out: &str) -> Vec<MdnsService> {
    out.lines()
        .filter(|l| !l.starts_with("List of"))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let (instance, service, addr) = (words.next()?, words.next()?, words.next()?);
            // Older adb prints the service with a trailing dot.
            let service = service.trim_end_matches('.');
            service.starts_with("_adb").then(|| MdnsService { instance: instance.into(), service: service.into(), addr: addr.into() })
        })
        .collect()
}

/// How a pairing or connection went, read from what adb prints: it exits 0
/// on some failures, so the words decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// macOS Local Network privacy blocked the adb server.
    LocalNetworkBlocked,
    /// The phone refused: a wrong or expired code, or nothing listening.
    Failed(String),
}

/// `adb pair` → `Successfully paired to <addr> [guid=…]`.
pub fn classify_pair(out: &str) -> Outcome {
    classify(out, |o| o.contains("Successfully paired"))
}

/// The phone's mDNS instance (`adb-<id>`) from `adb pair`'s success line
/// (`… [guid=adb-R58M123ABC-x1Y2z3]`): adb's own word for which phone paired,
/// never what the network announces.
pub fn paired_guid(out: &str) -> Option<String> {
    let start = out.find("[guid=")? + "[guid=".len();
    let guid = &out[start..];
    let guid = &guid[..guid.find(']')?];
    (!guid.is_empty() && guid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))).then(|| guid.to_owned())
}

/// `adb connect` → `connected to <addr>` / `already connected to <addr>`.
pub fn classify_connect(out: &str) -> Outcome {
    classify(out, |o| o.contains("connected to") && !o.contains("failed to connect") && !o.contains("cannot connect"))
}

fn classify(out: &str, ok: impl Fn(&str) -> bool) -> Outcome {
    if ok(out) {
        return Outcome::Ok;
    }
    if out.contains("No route to host") {
        return Outcome::LocalNetworkBlocked;
    }
    // adb's own last line (never an argument: nothing secret is in argv).
    // The prompt and the reply share a line (stdin is not a terminal).
    let line = out.lines().map(|l| l.trim().trim_start_matches("Enter pairing code:").trim()).rfind(|l| !l.is_empty());
    Outcome::Failed(line.unwrap_or("adb gave no reason").to_owned())
}

/// A QR pairing's service name and password: `oximux-` plus six of
/// `[a-z0-9]`, and ten of `[A-Za-z0-9]`, from the OS's RNG.
#[derive(Clone, PartialEq, Eq)]
pub struct QrSecret {
    pub service: String,
    password: String,
}

impl std::fmt::Debug for QrSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrSecret").field("service", &self.service).field("password", &"<redacted>").finish()
    }
}

impl QrSecret {
    pub fn new() -> Self {
        let pick = |set: &[u8], n: usize| -> String { (0..n).map(|_| set[OsRng.gen_range(0..set.len())] as char).collect() };
        const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        const MIXED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        Self { service: format!("oximux-{}", pick(LOWER, 6)), password: pick(MIXED, 10) }
    }

    /// What the QR code carries (the phone's "Pair device with QR code" reads
    /// it like a Wi-Fi network).
    pub fn payload(&self) -> String {
        format!("WIFI:T:ADB;S:{};P:{};;", self.service, self.password)
    }
}

impl Default for QrSecret {
    fn default() -> Self {
        Self::new()
    }
}

impl Adb<'_> {
    /// `adb mdns check`: whether adb's mDNS discovery runs.
    pub fn mdns_available(&self) -> bool {
        self.raw(&["mdns", "check"], None, ADB_QUICK).is_ok_and(|o| {
            let out = o.stdout_str() + &String::from_utf8_lossy(&o.stderr);
            o.success() && !out.contains("unavailable") && !out.contains("ERROR")
        })
    }

    pub fn mdns_services(&self) -> Result<Vec<MdnsService>> {
        Ok(parse_mdns_services(&self.raw(&["mdns", "services"], None, ADB_QUICK)?.stdout_str()))
    }

    /// `adb pair <addr>`, the code on stdin: how it went, and the paired
    /// phone's mDNS instance (from adb's own reply).
    pub fn pair(&self, addr: &str, code: &str) -> Result<(Outcome, Option<String>)> {
        let input = format!("{code}\n");
        let out = self.raw(&["pair", addr], Some(input.as_bytes()), PAIR_TIMEOUT)?;
        let text = out.stdout_str() + &String::from_utf8_lossy(&out.stderr);
        Ok((classify_pair(&text), paired_guid(&text)))
    }

    pub fn connect(&self, addr: &str) -> Result<Outcome> {
        let out = self.raw(&["connect", addr], None, ADB_QUICK)?;
        Ok(classify_connect(&(out.stdout_str() + &String::from_utf8_lossy(&out.stderr))))
    }

    /// `adb disconnect <transport>` (best effort: a phone already gone is fine).
    pub fn disconnect(&self, transport: &str) -> Result<()> {
        self.raw(&["disconnect", transport], None, ADB_QUICK).map(drop)
    }
}

/// After a pairing: wait for the phone at `host` to advertise its connect
/// service, then connect to it. Returns the connected instance (`None` when
/// it never showed up: the caller asks for the connect port).
pub fn connect_after_pairing(adb: &Adb<'_>, host: &str, cancel: &AtomicBool) -> Result<Option<(MdnsService, Outcome)>> {
    let deadline = Instant::now() + CONNECT_WAIT;
    loop {
        let found = adb.mdns_services()?.into_iter().find(|s| s.service == CONNECT_SERVICE && host_of(&s.addr) == host);
        if let Some(service) = found {
            let outcome = adb.connect(&service.addr)?;
            return Ok(Some((service, outcome)));
        }
        wait(deadline, cancel)?;
        if Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

/// QR pairing: wait (≤ [`QR_WAIT`]) for the phone that scanned `secret` to
/// advertise its pairing service, pair with the password on stdin, then
/// connect. `cancel`: the dialog closed.
pub fn pair_by_qr(adb: &Adb<'_>, secret: &QrSecret, cancel: &AtomicBool) -> Result<QrResult> {
    let deadline = Instant::now() + QR_WAIT;
    let pairing = loop {
        let found = adb.mdns_services()?.into_iter().find(|s| s.service == PAIRING_SERVICE && s.instance == secret.service);
        if let Some(found) = found {
            break found;
        }
        wait(deadline, cancel)?;
        if Instant::now() >= deadline {
            return Err(SimError::Timeout { what: "the phone to scan the QR code".into(), secs: QR_WAIT.as_secs() });
        }
    };
    let guid = match adb.pair(&pairing.addr, &secret.password)? {
        (Outcome::Ok, guid) => guid,
        (other, _) => return Ok(QrResult::Pairing(other)),
    };
    let host = host_of(&pairing.addr).to_owned();
    Ok(match connect_after_pairing(adb, &host, cancel)? {
        Some((service, outcome)) => QrResult::Connected(service, outcome, guid),
        None => QrResult::PairedNotConnected(host, guid),
    })
}

/// How a QR pairing ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QrResult {
    /// Pairing itself failed.
    Pairing(Outcome),
    /// Paired (as this mDNS instance, when adb said), and the connect attempt
    /// to this service went as said.
    Connected(MdnsService, Outcome, Option<String>),
    /// Paired, but the phone at this host never advertised a connect port.
    PairedNotConnected(String, Option<String>),
}

fn wait(deadline: Instant, cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(SimError::Cancelled);
    }
    std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    if cancel.load(Ordering::Acquire) {
        return Err(SimError::Cancelled);
    }
    Ok(())
}

/// The host of `ip:port` (an IPv6 `[a::b]:port` keeps its brackets off).
pub fn host_of(addr: &str) -> &str {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.trim_start_matches('[').trim_end_matches(']')
}

/// A network transport's adb serial: `ip:port`, or an mDNS name
/// (`adb-<id>._adb-tls-connect._tcp`).
pub fn is_network_transport(serial: &str) -> bool {
    serial.contains("._adb-tls-connect._tcp") || serial.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok())
}

/// The mDNS instance a TLS transport's serial names (`adb-<id>` of
/// `adb-<id>._adb-tls-connect._tcp`), if it is one.
pub fn tls_instance(serial: &str) -> Option<&str> {
    serial.strip_suffix("._adb-tls-connect._tcp").or_else(|| serial.strip_suffix("._adb-tls-connect._tcp."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};
    use std::path::Path;

    #[test]
    fn mdns_services_parse_tabs_spaces_and_trailing_dots() {
        let out = "List of discovered mdns services\n\
adb-R58M123ABC-x1Y2z3\t_adb-tls-connect._tcp\t192.168.1.5:37123\n\
oximux-ab12cd   _adb-tls-pairing._tcp.   192.168.1.5:41000\n\
garbage line\n";
        let services = parse_mdns_services(out);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0], MdnsService { instance: "adb-R58M123ABC-x1Y2z3".into(), service: CONNECT_SERVICE.into(), addr: "192.168.1.5:37123".into() });
        assert_eq!(services[1].service, PAIRING_SERVICE);
        assert!(parse_mdns_services("List of discovered mdns services\n\n").is_empty());
    }

    /// adb exits 0 on some failures: the words decide.
    #[test]
    fn pair_and_connect_replies_classify() {
        assert_eq!(classify_pair("Enter pairing code: Successfully paired to 192.168.1.5:41000 [guid=adb-R58-x1]\n"), Outcome::Ok);
        assert_eq!(classify_pair("Enter pairing code: Failed: Wrong password or connection was dropped.\n"), Outcome::Failed("Failed: Wrong password or connection was dropped.".into()));
        assert_eq!(classify_connect("connected to 192.168.1.5:37123\n"), Outcome::Ok);
        assert_eq!(classify_connect("already connected to 192.168.1.5:37123\n"), Outcome::Ok);
        assert!(matches!(classify_connect("failed to connect to '192.168.1.5:37123': Connection refused\n"), Outcome::Failed(_)));
        assert_eq!(classify_connect("failed to connect to 192.168.1.5:37123: No route to host\n"), Outcome::LocalNetworkBlocked);
    }

    /// The pairing code goes to stdin: never into argv, so never into a
    /// timeout's or a failure's text.
    #[test]
    fn the_code_goes_on_stdin_never_argv() {
        let runner = ScriptedRunner::default()
            .expect("/sdk/adb pair 192.168.1.5:41000", CmdOutput::ok("Successfully paired to 192.168.1.5:41000 [guid=adb-R58-x1Y2]\n"));
        let adb = Adb::new(&runner, Path::new("/sdk/adb"));
        assert_eq!(adb.pair("192.168.1.5:41000", "482913").unwrap(), (Outcome::Ok, Some("adb-R58-x1Y2".into())));
        assert!(runner.calls().iter().all(|c| !c.contains("482913")), "{:?}", runner.calls());
        let timeout = SimError::Timeout { what: runner.calls()[0].clone(), secs: 30 };
        assert!(!timeout.to_string().contains("482913"));
    }

    #[test]
    fn a_qr_secret_has_the_right_shape_and_never_prints_its_password() {
        let a = QrSecret::new();
        let suffix = a.service.strip_prefix("oximux-").expect("prefix");
        assert_eq!(suffix.len(), 6);
        assert!(suffix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(a.password.len(), 10);
        assert!(a.password.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(a.payload(), format!("WIFI:T:ADB;S:{};P:{};;", a.service, a.password));
        assert!(!format!("{a:?}").contains(&a.password));
        assert_ne!(a, QrSecret::new(), "fresh every time");
    }

    #[test]
    fn the_paired_instance_comes_from_adbs_own_reply() {
        assert_eq!(paired_guid("Successfully paired to 192.168.1.5:41000 [guid=adb-R58M123-x1Y2z3]"), Some("adb-R58M123-x1Y2z3".into()));
        assert_eq!(paired_guid("Successfully paired to 192.168.1.5:41000"), None);
        assert_eq!(paired_guid("[guid=a b;c]"), None, "nothing odd gets through");
    }

    #[test]
    fn network_transports_are_recognized() {
        assert!(is_network_transport("192.168.1.5:37123"));
        assert!(is_network_transport("adb-R58M123ABC-x1Y2z3._adb-tls-connect._tcp"));
        assert!(!is_network_transport("R58M123ABC") && !is_network_transport("emulator-5554"));
        assert_eq!(tls_instance("adb-R58-x1._adb-tls-connect._tcp"), Some("adb-R58-x1"));
        assert_eq!(tls_instance("192.168.1.5:37123"), None);
        assert_eq!(host_of("192.168.1.5:37123"), "192.168.1.5");
        assert_eq!(host_of("[fe80::1]:5555"), "fe80::1");
    }
}
