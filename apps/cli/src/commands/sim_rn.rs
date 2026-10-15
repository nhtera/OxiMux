//! `oximux sim rn network|profile` (v32): what a React Native app did while
//! the host watched it through Metro, printed as text (or `--json`). The host
//! picks the app and redacts secrets before anything reaches here.

use std::path::Path;
use std::time::Duration;

use oximux_remote_proto::simulator::{SimCmdWire, SimReplyWire, SimRnCmdWire, SimRnFunctionWire, SimRnNetworkWire, SimRnProfileWire, SimRnRequestWire, SimRnWire};
use serde_json::json;

use super::sim::{Outcome, call, unexpected};
use crate::cli_sim_rn::RnCommand;
use crate::client::Client;

/// On top of the watch itself: waking the device (≤ 60 s), finding Metro,
/// connecting and the setup calls, and a profile's transfer once it stops.
const OVERHEAD: Duration = Duration::from_secs(90);

pub(super) async fn rn(client: &Client, worktree: &Path, command: RnCommand) -> Outcome {
    let (cmd, seconds) = match command {
        RnCommand::Network(a) => (SimRnCmdWire::Network { seconds: a.seconds, grep: a.grep, bodies: a.bodies, app: a.app }, a.seconds),
        RnCommand::Profile(a) => (SimRnCmdWire::Profile { seconds: a.seconds, top: a.top, app: a.app }, a.seconds),
    };
    let floor = Duration::from_secs(u64::from(seconds)) + OVERHEAD;
    match call(client, worktree, SimCmdWire::Rn(cmd), floor).await? {
        SimReplyWire::Rn(SimRnWire::Network(net)) => Ok((network_json(&net), network_human(&net, seconds))),
        SimReplyWire::Rn(SimRnWire::Profile(profile)) => Ok((profile_json(&profile), profile_human(&profile))),
        _ => Err(unexpected("Rn")),
    }
}

fn network_json(net: &SimRnNetworkWire) -> serde_json::Value {
    let request = |r: &SimRnRequestWire| {
        json!({
            "method": r.method, "url": r.url, "status": r.status, "ms": r.ms, "size": r.size, "mime": r.mime, "failed": r.failed,
            "request_headers": r.request_headers, "response_headers": r.response_headers,
            "request_body": r.request_body, "response_body": r.response_body,
        })
    };
    json!({ "app": net.app, "requests": net.requests.iter().map(request).collect::<Vec<_>>(), "truncated": net.truncated, "warnings": net.warnings })
}

/// `METHOD STATUS MS SIZE URL` per request, oldest first; headers and
/// bodies indented under theirs when asked for.
fn network_human(net: &SimRnNetworkWire, seconds: u32) -> String {
    let mut out = Vec::new();
    if net.requests.is_empty() {
        out.push(format!("(no requests from {} in {seconds} s)", net.app));
        return with_warnings(out, &net.warnings);
    }
    for r in &net.requests {
        let status = match (&r.failed, r.status) {
            (Some(why), _) => format!("failed ({why})"),
            (None, Some(status)) => status.to_string(),
            (None, None) => "pending".into(),
        };
        let ms = r.ms.map_or_else(|| "-".to_owned(), |ms| format!("{ms}ms"));
        let size = r.size.map_or_else(|| "-".to_owned(), bytes);
        out.push(format!("{:<6} {status:<7} {ms:>7} {size:>8}  {}", r.method, r.url));
        for (name, value) in &r.request_headers {
            out.push(format!("    > {name}: {value}"));
        }
        if let Some(body) = &r.request_body {
            out.push(format!("    > {body}"));
        }
        for (name, value) in &r.response_headers {
            out.push(format!("    < {name}: {value}"));
        }
        if let Some(body) = &r.response_body {
            out.push(format!("    < {body}"));
        }
    }
    let n = net.requests.len();
    out.push(format!("({n} request{} from {} in {seconds} s{})", if n == 1 { "" } else { "s" }, net.app, if net.truncated {
        "; more than fit were dropped: narrow with --grep or --seconds"
    } else {
        ""
    }));
    with_warnings(out, &net.warnings)
}

/// `out`'s lines, then each warning (`warning: …`).
fn with_warnings(mut out: Vec<String>, warnings: &[String]) -> String {
    out.extend(warnings.iter().map(|w| format!("warning: {w}")));
    out.join("\n")
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1}MB", n as f64 / f64::from(1 << 20)),
        n if n >= 1 << 10 => format!("{:.1}KB", n as f64 / f64::from(1 << 10)),
        n => format!("{n}B"),
    }
}

fn profile_json(p: &SimRnProfileWire) -> serde_json::Value {
    let function = |f: &SimRnFunctionWire| json!({ "name": f.name, "url": f.url, "line": f.line, "self_ms": f.self_ms });
    json!({
        "app": p.app, "duration_ms": p.duration_ms, "sampled_ms": p.sampled_ms,
        "functions": p.functions.iter().map(function).collect::<Vec<_>>(), "warnings": p.warnings,
    })
}

/// `SELF_MS % NAME FILE:LINE`, most self time first. Lines are the
/// bundle's (Metro serves one file), not the source's.
fn profile_human(p: &SimRnProfileWire) -> String {
    if p.functions.is_empty() {
        return with_warnings(vec![format!("(no samples from {} in {:.1} s)", p.app, p.duration_ms / 1000.0)], &p.warnings);
    }
    let mut out = vec![format!("{:>9} {:>6}  function", "self ms", "%")];
    for f in &p.functions {
        let share = if p.sampled_ms > 0.0 { f.self_ms / p.sampled_ms * 100.0 } else { 0.0 };
        let place = if f.url.is_empty() { String::new() } else { format!("  {}:{}", file_of(&f.url), f.line) };
        out.push(format!("{:>9.1} {share:>5.1}%  {}{place}", f.self_ms, f.name));
    }
    out.push(format!("({} profiled for {:.1} s, {:.1} s sampled; lines are the bundle's)", p.app, p.duration_ms / 1000.0, p.sampled_ms / 1000.0));
    with_warnings(out, &p.warnings)
}

/// A script URL's last path segment, without its query: `index.bundle`
/// (Metro's own URLs start theirs with `//&`).
fn file_of(url: &str) -> &str {
    let path = url.split(['?', '#', '&']).next().unwrap_or(url);
    path.trim_end_matches('/').rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, status: Option<u16>, failed: Option<&str>) -> SimRnRequestWire {
        SimRnRequestWire {
            method: method.into(),
            url: "https://api.example.com/items?page=[redacted]".into(),
            status,
            ms: Some(120),
            size: Some(2048),
            mime: Some("application/json".into()),
            failed: failed.map(Into::into),
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            request_body: None,
            response_body: None,
        }
    }

    #[test]
    fn a_request_is_one_line() {
        let net = SimRnNetworkWire {
            app: "com.a.b".into(),
            requests: vec![request("GET", Some(200), None), request("POST", None, Some("net::ERR_INTERNET_DISCONNECTED"))],
            truncated: true,
            warnings: vec!["it closed DevTools".into()],
        };
        let text = network_human(&net, 10);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("GET    200") && lines[0].contains("120ms") && lines[0].contains("2.0KB"), "{}", lines[0]);
        assert!(lines[1].contains("failed (net::ERR_INTERNET_DISCONNECTED)"), "{}", lines[1]);
        assert!(lines[2].contains("2 requests from com.a.b in 10 s") && lines[2].contains("dropped"), "{}", lines[2]);
        assert_eq!(lines[3], "warning: it closed DevTools");
        let empty = SimRnNetworkWire { app: "com.a.b".into(), requests: Vec::new(), truncated: false, warnings: Vec::new() };
        assert_eq!(network_human(&empty, 5), "(no requests from com.a.b in 5 s)");
    }

    #[test]
    fn a_function_names_its_bundle_line() {
        let f = |name: &str, url: &str, ms| SimRnFunctionWire { name: name.into(), url: url.into(), line: 77, self_ms: ms };
        let p = SimRnProfileWire {
            app: "com.a.b".into(),
            duration_ms: 1000.0,
            sampled_ms: 1000.0,
            functions: vec![f("busy", "http://127.0.0.1:8081/index.bundle//&platform=ios&dev=true", 400.0), f("(garbage collector)", "", 50.0)],
            warnings: Vec::new(),
        };
        let text = profile_human(&p);
        assert!(text.contains("400.0  40.0%  busy  index.bundle:77"), "{text}");
        assert!(text.lines().nth(2).is_some_and(|l| l.ends_with("(garbage collector)")), "{text}");
        assert_eq!(file_of("http://h:1/a/b.bundle?x=1"), "b.bundle");
    }
}
