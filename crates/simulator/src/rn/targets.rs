//! Which React Native app to look at: Metro lists its debugger targets at
//! `/json/list`; the one running on the worktree's device (and, when asked,
//! with the app id) is the one. Only Fusebox-era targets (React Native 0.76
//! and later, with a `reactNative` record) are taken, and only on a
//! loopback debugger URL: nothing here ever connects off the Mac.

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::time::Duration;

/// Where Metro is looked for (8081, and the ports it moves to when 8081 is
/// taken).
pub const METRO_PORTS: RangeInclusive<u16> = 8081..=8089;
/// How long a port may take to accept: nothing listening answers at once.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a listing may take once connected.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// The most of a listing read.
const MAX_LISTING: u64 = 1 << 20;

/// One app connected to Metro.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RnTarget {
    /// Its bundle id or package.
    pub app_id: String,
    /// As the app reports it: a simulator's (or iPhone's) name; on Android
    /// `MODEL - RELEASE - API N` (some emulators: `MODEL` alone).
    pub device_name: String,
    /// Metro's id for the device (one per device, whatever its name).
    pub device_id: String,
    /// `127.0.0.1` / `localhost` / `[::1]`.
    pub host: String,
    pub port: u16,
    /// The debugger's path and query.
    pub path: String,
    /// Metro lets this debugger in beside an open React Native DevTools
    /// (without it, connecting closes DevTools' session).
    pub multiple_debuggers: bool,
}

impl RnTarget {
    /// The origin Metro's debugger endpoint accepts (it refuses others).
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// The React Native targets in a `/json/list` body.
pub fn parse_list(body: &str) -> Vec<RnTarget> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    entries.iter().filter_map(target).collect()
}

fn target(entry: &serde_json::Value) -> Option<RnTarget> {
    let rn = entry.get("reactNative")?;
    let text = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_str).map(str::to_owned);
    let url = text(entry.get("webSocketDebuggerUrl"))?;
    let Some((host, port, path)) = loopback_ws(&url) else {
        tracing::warn!("ignoring a React Native debugger off this Mac");
        return None;
    };
    Some(RnTarget {
        app_id: text(entry.get("appId")).unwrap_or_default(),
        device_name: text(entry.get("deviceName")).unwrap_or_default(),
        device_id: rn.get("logicalDeviceId").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
        host,
        port,
        path,
        multiple_debuggers: rn.pointer("/capabilities/supportsMultipleDebuggers").and_then(serde_json::Value::as_bool) == Some(true),
    })
}

/// `ws://HOST:PORT/PATH` split, when HOST is this Mac.
pub fn loopback_ws(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("ws://")?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = bare.eq_ignore_ascii_case("localhost") || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    (loopback && !authority.contains('@')).then(|| (host.to_owned(), port, path.to_owned()))
}

/// Why no single app was picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoPick {
    /// Nothing connected to any Metro.
    NoMetro,
    /// Apps are connected, none from this device (`connected`: "app on
    /// device").
    NotOnDevice { connected: Vec<String> },
    /// This device's apps, not the one asked for.
    NoSuchApp { app: String, on_device: Vec<String> },
    /// Several apps on this device: `--app` picks one.
    Several(Vec<String>),
    /// Several connected devices go by this device's name: which one is
    /// this cannot be told.
    SameName(String),
}

impl std::fmt::Display for NoPick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMetro => write!(
                f,
                "no React Native app is connected to a Metro dev server on 127.0.0.1:{}–{} (a debug build of React Native 0.83 or later, with Metro running)",
                METRO_PORTS.start(),
                METRO_PORTS.end()
            ),
            Self::NotOnDevice { connected } => {
                write!(f, "no React Native app on this device is connected to Metro (connected: {})", connected.join(", "))
            }
            Self::NoSuchApp { app, on_device } => write!(f, "{app} is not connected to Metro from this device (connected: {})", on_device.join(", ")),
            Self::Several(apps) => write!(f, "several React Native apps on this device are connected to Metro: {}; pick one with --app", apps.join(", ")),
            Self::SameName(name) => write!(
                f,
                "several devices connected to Metro are called “{name}”, so this one cannot be told apart; close the app on the others"
            ),
        }
    }
}

/// Whether a target's device is the one called `name`: its name, or (on
/// Android) its model followed by the OS release.
fn same_device(name: &str, target: &RnTarget) -> bool {
    let (name, theirs) = (name.to_lowercase(), target.device_name.to_lowercase());
    !name.is_empty() && (theirs == name || theirs.starts_with(&format!("{name} - ")))
}

/// The app on the device called one of `names` (case aside), and `app` when
/// given. One app with several targets on one device (several Metros) takes
/// the first; on several devices of that name, none.
pub fn pick(targets: &[RnTarget], names: &[String], app: Option<&str>) -> Result<RnTarget, NoPick> {
    if targets.is_empty() {
        return Err(NoPick::NoMetro);
    }
    let here: Vec<&RnTarget> = targets.iter().filter(|t| names.iter().any(|n| same_device(n, t))).collect();
    let mut devices: Vec<&str> = here.iter().map(|t| t.device_id.as_str()).collect();
    devices.sort_unstable();
    devices.dedup();
    if devices.len() > 1 {
        return Err(NoPick::SameName(here[0].device_name.clone()));
    }
    if here.is_empty() {
        let connected = targets.iter().map(|t| format!("{} on {}", t.app_id, t.device_name)).collect();
        return Err(NoPick::NotOnDevice { connected });
    }
    let mut apps: Vec<String> = here.iter().map(|t| t.app_id.clone()).collect();
    apps.dedup();
    if let Some(app) = app {
        return here.iter().find(|t| t.app_id == app).map(|t| (*t).clone()).ok_or_else(|| NoPick::NoSuchApp { app: app.into(), on_device: apps });
    }
    let mut distinct = apps.clone();
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        return Err(NoPick::Several(distinct));
    }
    Ok(here[0].clone())
}

/// Every React Native target on Metro's ports, probed at once.
pub fn discover() -> Vec<RnTarget> {
    let agent = ureq::AgentBuilder::new().timeout_connect(CONNECT_TIMEOUT).timeout(READ_TIMEOUT).redirects(0).build();
    let probes: Vec<_> = METRO_PORTS
        .map(|port| {
            let agent = agent.clone();
            std::thread::spawn(move || listing(&agent, port))
        })
        .collect();
    probes.into_iter().filter_map(|p| p.join().ok()).flatten().collect()
}

fn listing(agent: &ureq::Agent, port: u16) -> Vec<RnTarget> {
    use std::io::Read as _;
    let Ok(response) = agent.get(&format!("http://127.0.0.1:{port}/json/list")).call() else {
        return Vec::new();
    };
    let mut body = String::new();
    if response.into_reader().take(MAX_LISTING).read_to_string(&mut body).is_err() {
        return Vec::new();
    }
    // A listing names only its own Metro: never another loopback port.
    parse_list(&body).into_iter().filter(|t| t.port == port).collect()
}

/// Whether `launchctl list` (a simulator's, through `simctl spawn`) shows
/// `bundle` running: a name matches several devices, the running app only
/// this one.
pub fn runs_in_launchctl(list: &str, bundle: &str) -> bool {
    let label = format!("UIKitApplication:{bundle}[");
    list.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let pid = fields.next().unwrap_or("-");
        pid.parse::<u32>().is_ok() && fields.nth(1).is_some_and(|l| l.starts_with(&label))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../../tests/fixtures/rn/json_list.json");

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn metros_listing_reads_as_targets() {
        let targets = parse_list(LIST);
        assert_eq!(targets.len(), 2, "the legacy page is skipped: {targets:?}");
        let t = &targets[0];
        assert_eq!((t.app_id.as_str(), t.device_name.as_str(), t.host.as_str(), t.port), ("com.nhtera.rnprobe", "iPhone 17", "127.0.0.1", 8081));
        assert!(t.path.starts_with("/inspector/debug?device=") && t.multiple_debuggers);
        assert_eq!(t.origin(), "http://127.0.0.1:8081");
    }

    #[test]
    fn a_debugger_off_this_mac_is_never_a_target() {
        assert!(loopback_ws("ws://192.168.1.5:8081/inspector/debug").is_none());
        assert!(loopback_ws("ws://evil.example:8081/x").is_none());
        assert!(loopback_ws("ws://127.0.0.1@evil.example:8081/x").is_none());
        assert!(loopback_ws("wss://127.0.0.1:8081/x").is_none());
        assert_eq!(loopback_ws("ws://[::1]:8082/x?y=1"), Some(("[::1]".into(), 8082, "/x?y=1".into())));
        assert!(loopback_ws("ws://localhost:8081/").is_some() && loopback_ws("ws://127.0.0.2:8081/").is_some());
        let lan = LIST.replace("127.0.0.1", "192.168.1.5");
        assert!(parse_list(&lan).is_empty());
    }

    #[test]
    fn a_running_app_is_found_in_launchctl() {
        let list = "PID\tStatus\tLabel\n55664\t0\tUIKitApplication:com.a.b[6597][rb-legacy]\n-\t0\tUIKitApplication:com.a.c[11][rb-legacy]\n";
        assert!(runs_in_launchctl(list, "com.a.b"));
        assert!(!runs_in_launchctl(list, "com.a.c"), "listed, not running");
        assert!(!runs_in_launchctl(list, "com.a"), "a prefix is not the app");
    }

    #[test]
    fn the_app_on_this_device_is_picked() {
        let targets = parse_list(LIST);
        assert_eq!(pick(&targets, &names(&["iphone 17"]), None).unwrap().app_id, "com.nhtera.rnprobe");
        assert_eq!(pick(&targets, &names(&["sdk_gphone64_arm64"]), None).unwrap().app_id, "com.example.other", "Android: model, then release");
        assert!(pick(&targets, &names(&["sdk_gphone64"]), None).is_err(), "a model is matched whole");
        assert!(matches!(pick(&targets, &names(&["Pixel 9"]), None), Err(NoPick::NotOnDevice { connected }) if connected.len() == 2));
        assert!(matches!(pick(&targets, &names(&["iPhone 17"]), Some("x.y")), Err(NoPick::NoSuchApp { .. })));
        assert_eq!(pick(&[], &names(&["iPhone 17"]), None), Err(NoPick::NoMetro));
        let mut twins = targets.clone();
        twins[1].device_name = "iPhone 17".into();
        assert_eq!(pick(&twins, &names(&["iPhone 17"]), None), Err(NoPick::SameName("iPhone 17".into())), "two devices, one name");
        let mut two = twins;
        two[1].device_id = two[0].device_id.clone();
        assert_eq!(pick(&two, &names(&["iPhone 17"]), None), Err(NoPick::Several(names(&["com.example.other", "com.nhtera.rnprobe"]))));
        assert_eq!(pick(&two, &names(&["iPhone 17"]), Some("com.example.other")).unwrap().port, 8082);
    }
}
