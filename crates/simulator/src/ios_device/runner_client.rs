//! A client for the iPhone control runner (protocol `oximux-runner/1`; the
//! contract is the fork's `oximux/ios-runner/PROTOCOL.md`).
//!
//! One HTTP/1.1 `POST /` per command over a fresh stream (usbmux in the app,
//! TCP in tests), `Content-Length` framed, with the run's bearer token. The
//! reply is an envelope: `{ok: true, data, reactivated?}` or `{ok: false,
//! error: {code, message, hint?}}`.
//!
//! Send-once: a mutating command carries a `commandId`, and the runner does
//! it at most once per id. So a lost reply, or `RUNNER_BUSY` (it never ran),
//! sends the **same** id again; `IN_PROGRESS` and `RUNNER_WEDGED` (it still
//! runs) are followed through `status{statusCommandId}` until its real reply
//! lands. A gesture is never replayed under a new id.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng as _;
use rand::rngs::OsRng;
use serde_json::{Map, Value, json};

use super::usbmux::{Usbmux, UsbmuxError};

/// What `status` reports the runner speaks.
pub const PROTOCOL: &str = "oximux-runner/1";

/// The commands the runner does once per `commandId`.
const MUTATING: &[&str] = &["tap", "longPress", "drag", "type", "keyboardReturn", "keyboardDelete", "button"];
/// A reply's head longer than this is not the runner's.
const MAX_HEAD: usize = 16 * 1024;
/// Between two looks at a command that is still running, or a busy runner.
const POLL: Duration = Duration::from_millis(250);

/// A byte stream to the runner, with a deadline for each read and write.
pub trait RunnerStream: Read + Write + Send {
    fn set_deadline(&self, timeout: Duration) -> io::Result<()>;
}

impl RunnerStream for UnixStream {
    fn set_deadline(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))?;
        self.set_write_timeout(Some(timeout))
    }
}

impl RunnerStream for TcpStream {
    fn set_deadline(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))?;
        self.set_write_timeout(Some(timeout))
    }
}

/// Opens a fresh stream to the runner (one per request).
pub type Connector = Arc<dyn Fn() -> Result<Box<dyn RunnerStream>, RunnerError> + Send + Sync>;

/// Streams to `port` on the iPhone `udid`, through usbmux.
pub fn usbmux_connector(mux: Usbmux, udid: String, port: u16) -> Connector {
    Arc::new(move || match mux.connect(&udid, port) {
        Ok(stream) => Ok(Box::new(stream) as Box<dyn RunnerStream>),
        Err(UsbmuxError::Refused(_)) => Err(RunnerError::Refused),
        Err(UsbmuxError::NotConnected) => Err(RunnerError::NotConnected),
        Err(e) => Err(RunnerError::Unreachable(e.to_string())),
    })
}

/// A fresh token for one run of the runner: 32 random bytes, as hex.
pub fn new_token() -> String {
    let bytes: [u8; 32] = OsRng.r#gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the runner is given instead of `token`: its SHA-256, as hex. XCTest
/// writes the test's environment into the result bundle's session log, which
/// any process of the user's can read, so the token itself never goes there.
pub fn token_digest(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// Nothing listens on the runner's port: it has ended.
    #[error("the iPhone's control runner is not running")]
    Refused,
    #[error("the iPhone is not connected over USB")]
    NotConnected,
    #[error("the iPhone's control runner cannot be reached: {0}")]
    Unreachable(String),
    /// The token is not this run's.
    #[error("the iPhone's control runner refused OxiMux's token")]
    Unauthorized,
    #[error("the iPhone's control runner answered HTTP {0}")]
    Http(u16),
    /// The command ran and failed; `code` is the runner's.
    #[error("{message}")]
    Command { code: String, message: String, hint: Option<String> },
    /// The stream broke after the request went out.
    #[error("the iPhone's control runner's reply was lost: {0}")]
    Lost(String),
    #[error("the iPhone's control runner did not answer within {0}s")]
    Timeout(u64),
    #[error("the iPhone's control runner's reply was unreadable: {0}")]
    Protocol(String),
}

impl RunnerError {
    /// The runner's error code, for a command that failed.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Command { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// What a command answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub data: Value,
    /// The command first brought its app back to the front.
    pub reactivated: bool,
}

#[derive(Clone)]
pub struct RunnerClient {
    connect: Connector,
    token: Arc<str>,
}

impl std::fmt::Debug for RunnerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("RunnerClient").finish_non_exhaustive()
    }
}

impl RunnerClient {
    pub fn new(connect: Connector, token: &str) -> Self {
        Self { connect, token: token.into() }
    }

    /// Runs `command` with `fields` (an object's members) and waits for its
    /// reply, within [`timeout_for`] it.
    pub fn call(&self, command: &str, fields: Value) -> Result<Reply, RunnerError> {
        let mut body = match fields {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => return Err(RunnerError::Protocol(format!("fields must be an object, not {other}"))),
        };
        let deadline = Deadline::after(timeout_for(command, &body));
        body.insert("command".into(), command.into());
        let id = MUTATING.contains(&command).then(new_command_id);
        if let Some(id) = &id {
            body.insert("commandId".into(), id.as_str().into());
        }
        let body = serde_json::to_vec(&Value::Object(body)).map_err(|e| RunnerError::Protocol(e.to_string()))?;
        let mut lost = false;
        loop {
            let error = match self.post(&body, deadline) {
                Ok(envelope) => match decode(envelope) {
                    Ok(reply) => return Ok(reply),
                    Err(error) => error,
                },
                // The reply comes before the run ends; the end may beat it.
                Err(RunnerError::Lost(_)) if command == "shutdown" => return Ok(Reply::default()),
                // Once: the same id again is answered from the runner's
                // journal (a read-only command is simply asked again).
                Err(RunnerError::Lost(_)) if !lost => {
                    lost = true;
                    continue;
                }
                // Gone after the request went out: it may have run, so this
                // is a lost reply, never "it never ran".
                Err(RunnerError::Refused | RunnerError::Unauthorized) if lost => return Err(gone_after_sending()),
                Err(error) => return Err(error),
            };
            match (error.code(), &id) {
                // It never ran: the same id may come again.
                (Some("RUNNER_BUSY"), _) => pause(deadline)?,
                // It runs still: its reply will be in the journal.
                (Some("IN_PROGRESS" | "RUNNER_WEDGED"), Some(id)) => return self.await_done(id, deadline),
                _ => return Err(error),
            }
        }
    }

    /// The reply of the command `id`, once the runner has it.
    fn await_done(&self, id: &str, deadline: Deadline) -> Result<Reply, RunnerError> {
        let body = serde_json::to_vec(&json!({"command": "status", "statusCommandId": id}))
            .map_err(|e| RunnerError::Protocol(e.to_string()))?;
        loop {
            pause(deadline)?;
            let status = match self.post(&body, deadline) {
                Ok(envelope) => decode(envelope)?,
                Err(RunnerError::Lost(_)) => continue,
                // The command was sent (it is running): not "never ran".
                Err(RunnerError::Refused | RunnerError::Unauthorized) => return Err(gone_after_sending()),
                Err(error) => return Err(error),
            };
            let command = status.data.get("command").cloned().unwrap_or(Value::Null);
            match command.get("state").and_then(Value::as_str) {
                Some("pending") => {}
                Some("done") => return decode(command.get("reply").cloned().unwrap_or(Value::Null)),
                _ => return Err(RunnerError::Protocol(format!("the runner has no record of command {id}"))),
            }
        }
    }

    /// One request and its reply's JSON body.
    fn post(&self, body: &[u8], deadline: Deadline) -> Result<Value, RunnerError> {
        let mut stream = (self.connect)()?;
        let mut request = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.token,
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        stream.set_deadline(remaining(deadline)?).map_err(lost)?;
        stream.write_all(&request).map_err(|e| io_error(e, deadline))?;
        stream.flush().map_err(|e| io_error(e, deadline))?;
        let (status, payload) = read_reply(stream.as_mut(), deadline)?;
        match status {
            200 => serde_json::from_slice(&payload).map_err(|e| RunnerError::Protocol(e.to_string())),
            401 => Err(RunnerError::Unauthorized),
            other => Err(RunnerError::Http(other)),
        }
    }
}

/// How long `command` may take, end to end: above the runner's own deadline
/// for it (30 s, plus its length: a gesture's time, ~50 ms a typed
/// character), so its `RUNNER_WEDGED` is heard rather than a timeout here.
pub fn timeout_for(command: &str, fields: &Map<String, Value>) -> Duration {
    if !MUTATING.contains(&command) {
        return Duration::from_secs(40);
    }
    let ms: f64 = ["durationMs", "holdMs", "settle"]
        .iter()
        .filter_map(|k| fields.get(*k).and_then(Value::as_f64))
        .map(|v| v.max(0.0))
        .sum();
    let typing = fields.get("text").and_then(Value::as_str).map_or(0, |t| t.chars().count()) as f64 * 50.0;
    Duration::from_secs(45) + Duration::from_millis((ms + typing).min(300_000.0) as u64)
}

fn gone_after_sending() -> RunnerError {
    RunnerError::Lost("the runner stopped after the command was sent".into())
}

/// When a call gives up, and how long it was given.
#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    total: Duration,
}

impl Deadline {
    fn after(total: Duration) -> Self {
        Self { at: Instant::now() + total, total }
    }

    fn expired(self) -> RunnerError {
        RunnerError::Timeout(self.total.as_secs())
    }
}

fn new_command_id() -> String {
    let bytes: [u8; 12] = OsRng.r#gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An envelope as its reply, or its error.
fn decode(envelope: Value) -> Result<Reply, RunnerError> {
    match envelope.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(Reply {
            reactivated: envelope.get("reactivated").and_then(Value::as_bool).unwrap_or(false),
            data: envelope.get("data").cloned().unwrap_or(Value::Null),
        }),
        Some(false) => {
            let error = envelope.get("error");
            let field = |k: &str| error.and_then(|e| e.get(k)).and_then(Value::as_str).map(str::to_owned);
            Err(RunnerError::Command {
                code: field("code").unwrap_or_default(),
                message: field("message").unwrap_or_else(|| "the command failed".into()),
                hint: field("hint"),
            })
        }
        None => Err(RunnerError::Protocol("not an envelope".into())),
    }
}

fn pause(deadline: Deadline) -> Result<(), RunnerError> {
    if Instant::now() + POLL >= deadline.at {
        return Err(deadline.expired());
    }
    std::thread::sleep(POLL);
    Ok(())
}

fn remaining(deadline: Deadline) -> Result<Duration, RunnerError> {
    let left = deadline.at.saturating_duration_since(Instant::now());
    if left.is_zero() { Err(deadline.expired()) } else { Ok(left) }
}

fn lost(e: io::Error) -> RunnerError {
    RunnerError::Lost(e.to_string())
}

fn io_error(e: io::Error, deadline: Deadline) -> RunnerError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => deadline.expired(),
        _ => lost(e),
    }
}

/// The status code and body of a `Connection: close` reply: `Content-Length`
/// bytes, or to the end when it has none.
fn read_reply(stream: &mut dyn RunnerStream, deadline: Deadline) -> Result<(u16, Vec<u8>), RunnerError> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 64 * 1024];
    let head_end = loop {
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buffer.len() > MAX_HEAD {
            return Err(RunnerError::Protocol("the reply's head is too long".into()));
        }
        let n = read_some(stream, &mut chunk, deadline)?;
        if n == 0 {
            return Err(RunnerError::Lost("the stream ended before a reply".into()));
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| RunnerError::Protocol("a non-UTF-8 head".into()))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1.1 ").or_else(|| line.strip_prefix("HTTP/1.0 ")))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| RunnerError::Protocol("no status line".into()))?;
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().map_err(|_| RunnerError::Protocol("a bad Content-Length".into())))
        .transpose()?;
    let mut body = buffer.split_off(head_end);
    loop {
        if length.is_some_and(|n| body.len() >= n) {
            body.truncate(length.unwrap_or_default());
            return Ok((status, body));
        }
        let n = read_some(stream, &mut chunk, deadline)?;
        if n == 0 {
            return match length {
                None => Ok((status, body)),
                Some(_) => Err(RunnerError::Lost("the stream ended inside the reply".into())),
            };
        }
        body.extend_from_slice(&chunk[..n]);
    }
}

fn read_some(stream: &mut dyn RunnerStream, chunk: &mut [u8], deadline: Deadline) -> Result<usize, RunnerError> {
    stream.set_deadline(remaining(deadline)?).map_err(lost)?;
    loop {
        match stream.read(chunk) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other.map_err(|e| io_error(e, deadline)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;

    /// The runner gets the token's SHA-256 as lower-case hex (its
    /// `HTTP.digest(hex:)` reads exactly this), never the token.
    #[test]
    fn the_runner_is_given_the_tokens_digest() {
        assert_eq!(token_digest("secret"), "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b");
        let token = new_token();
        assert_eq!(token_digest(&token).len(), 64);
        assert_ne!(token_digest(&token), token);
    }

    enum Answer {
        Envelope(Value),
        Status(u16),
        /// Read the request, then close without a reply.
        Drop,
    }

    /// A stand-in runner on a TCP port: each request's head and JSON body go
    /// to `answer`, and every body is kept.
    fn fake_runner(answer: impl Fn(&str, &Value) -> Answer + Send + 'static) -> (RunnerClient, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (String::new(), Value::Null);
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        break (head, serde_json::from_slice(&buffer[end + 4..end + 4 + length]).unwrap_or(Value::Null));
                    }
                };
                kept.lock().unwrap().push(body.clone());
                let reply = match answer(&head, &body) {
                    Answer::Envelope(envelope) => {
                        let json = envelope.to_string();
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len())
                    }
                    Answer::Status(code) => format!("HTTP/1.1 {code} No\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    Answer::Drop => continue,
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        let connect: Connector = Arc::new(move || {
            TcpStream::connect(("127.0.0.1", port))
                .map(|s| Box::new(s) as Box<dyn RunnerStream>)
                .map_err(|e| RunnerError::Unreachable(e.to_string()))
        });
        (RunnerClient::new(connect, "t0ken-of-this-run"), seen)
    }

    fn ok(data: Value) -> Answer {
        Answer::Envelope(json!({"ok": true, "data": data}))
    }

    fn failure(code: &str) -> Answer {
        Answer::Envelope(json!({"ok": false, "error": {"code": code, "message": format!("{code} happened"), "hint": "tap the field first"}}))
    }

    #[test]
    fn a_command_carries_the_token_and_reads_its_envelope() {
        let (client, seen) = fake_runner(|head, body| {
            if !head.contains("Authorization: Bearer t0ken-of-this-run") || !head.contains("Connection: close") {
                return Answer::Status(401);
            }
            match body["command"].as_str() {
                Some("viewport") => ok(json!({"width": 430, "height": 932})),
                Some("tap") => Answer::Envelope(json!({"ok": true, "data": {}, "reactivated": true})),
                _ => failure("UNKNOWN_COMMAND"),
            }
        });
        let viewport = client.call("viewport", json!({"app": "com.apple.springboard"})).unwrap();
        assert_eq!(viewport.data["width"], 430);
        assert!(!viewport.reactivated);
        assert!(client.call("tap", json!({"x": 10, "y": 20})).unwrap().reactivated);
        let seen = seen.lock().unwrap();
        // Read-only commands carry no id; gestures do.
        assert!(seen[0].get("commandId").is_none() && seen[0]["app"] == "com.apple.springboard");
        assert!(seen[1]["commandId"].as_str().is_some_and(|id| id.len() == 24));
    }

    #[test]
    fn a_wrong_token_and_refusals_are_typed() {
        let (client, _) = fake_runner(|_, body| match body["command"].as_str() {
            Some("status") => Answer::Status(401),
            Some("viewport") => Answer::Status(413),
            _ => failure("XCTEST_FAILED"),
        });
        assert!(matches!(client.call("status", Value::Null), Err(RunnerError::Unauthorized)));
        assert!(matches!(client.call("viewport", json!({})), Err(RunnerError::Http(413))));
        let Err(RunnerError::Command { code, message, hint }) = client.call("type", json!({"text": "hi"})) else { panic!() };
        assert_eq!((code.as_str(), message.as_str(), hint.as_deref()), ("XCTEST_FAILED", "XCTEST_FAILED happened", Some("tap the field first")));
    }

    #[test]
    fn a_lost_reply_sends_the_same_id_again_and_the_gesture_happens_once() {
        let taps = Arc::new(Mutex::new(Vec::<String>::new()));
        let done = taps.clone();
        let (client, seen) = fake_runner(move |_, body| {
            let id = body["commandId"].as_str().unwrap_or_default().to_owned();
            let mut done = done.lock().unwrap();
            if done.contains(&id) {
                // The runner's journal: the first reply, without acting.
                return ok(json!({"from": "journal"}));
            }
            done.push(id);
            Answer::Drop
        });
        let reply = client.call("tap", json!({"x": 1, "y": 2})).unwrap();
        assert_eq!(reply.data["from"], "journal");
        assert_eq!(taps.lock().unwrap().len(), 1);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["commandId"], seen[1]["commandId"]);
    }

    #[test]
    fn a_reply_lost_twice_is_an_error() {
        let (client, seen) = fake_runner(|_, _| Answer::Drop);
        assert!(matches!(client.call("snapshot", json!({})), Err(RunnerError::Lost(_))));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn busy_retries_the_same_id_and_wedged_follows_status() {
        let calls = Arc::new(Mutex::new(0));
        let count = calls.clone();
        let (client, seen) = fake_runner(move |_, body| {
            let mut n = count.lock().unwrap();
            *n += 1;
            match (body["command"].as_str(), *n) {
                (Some("drag"), 1) => failure("RUNNER_BUSY"),
                (Some("drag"), 2) => failure("RUNNER_WEDGED"),
                (Some("status"), 3) => ok(json!({"command": {"state": "pending"}})),
                (Some("status"), _) => ok(json!({"command": {"state": "done", "reply": {"ok": true, "data": {"late": true}}}})),
                _ => failure("BAD_REQUEST"),
            }
        });
        let reply = client.call("drag", json!({"from": {"x": 1, "y": 1}, "to": {"x": 1, "y": 300}, "durationMs": 300})).unwrap();
        assert_eq!(reply.data["late"], true);
        let seen = seen.lock().unwrap();
        let id = &seen[0]["commandId"];
        assert_eq!(&seen[1]["commandId"], id);
        assert_eq!(&seen[2]["statusCommandId"], id);
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn an_unknown_command_in_status_is_an_error_not_a_hang() {
        let (client, _) = fake_runner(|_, body| match body["command"].as_str() {
            Some("status") => ok(json!({"command": null})),
            _ => failure("IN_PROGRESS"),
        });
        assert!(matches!(client.call("button", json!({"name": "home"})), Err(RunnerError::Protocol(_))));
    }

    #[test]
    fn shutdown_whose_reply_is_lost_has_still_ended_the_run() {
        let (client, _) = fake_runner(|_, _| Answer::Drop);
        assert_eq!(client.call("shutdown", Value::Null).unwrap(), Reply::default());
    }

    #[test]
    fn a_runner_that_is_gone_is_refused_without_retrying() {
        let attempts = Arc::new(Mutex::new(0));
        let counted = attempts.clone();
        let connect: Connector = Arc::new(move || {
            *counted.lock().unwrap() += 1;
            Err(RunnerError::Refused)
        });
        let client = RunnerClient::new(connect, "x");
        assert!(matches!(client.call("tap", json!({"x": 1, "y": 1})), Err(RunnerError::Refused)));
        assert_eq!(*attempts.lock().unwrap(), 1);
    }

    #[test]
    fn a_reply_without_content_length_reads_to_the_end() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"ok\":true,\"data\":{\"protocol\":\"oximux-runner/1\"}}");
        });
        let connect: Connector =
            Arc::new(move || Ok(Box::new(TcpStream::connect(("127.0.0.1", port)).unwrap()) as Box<dyn RunnerStream>));
        let reply = RunnerClient::new(connect, "x").call("status", Value::Null).unwrap();
        assert_eq!(reply.data["protocol"], PROTOCOL);
    }

    #[test]
    fn timeouts_sit_above_the_runners_own_deadlines() {
        let fields = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(timeout_for("snapshot", &Map::new()), Duration::from_secs(40));
        assert_eq!(timeout_for("tap", &fields(json!({"x": 1}))), Duration::from_secs(45));
        assert_eq!(timeout_for("drag", &fields(json!({"durationMs": 2000, "holdMs": 500, "settle": 150}))), Duration::from_millis(47_650));
        assert_eq!(timeout_for("type", &fields(json!({"text": "héllo"}))), Duration::from_millis(45_250));
        // Capped like the runner's.
        assert_eq!(timeout_for("longPress", &fields(json!({"durationMs": 9e9}))), Duration::from_secs(345));
    }

    #[test]
    fn neither_debug_nor_errors_show_the_token() {
        let (client, _) = fake_runner(|_, _| Answer::Status(401));
        assert!(!format!("{client:?}").contains("t0ken"));
        let error = client.call("status", Value::Null).unwrap_err();
        assert!(!error.to_string().contains("t0ken"));
        assert_eq!(new_token().len(), 64);
        assert_ne!(new_token(), new_token());
    }

    #[test]
    fn a_reply_head_longer_than_16_kib_is_a_protocol_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                // Send a response with an extremely long header line (exceeds MAX_HEAD)
                let long_header = "X-Long: ".to_string() + &"x".repeat(20 * 1024);
                let response = format!("HTTP/1.1 200 OK\r\n{}\r\n\r\n", long_header);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let connect: Connector =
            Arc::new(move || Ok(Box::new(TcpStream::connect(("127.0.0.1", port)).unwrap()) as Box<dyn RunnerStream>));
        let error = RunnerClient::new(connect, "x").call("status", Value::Null).unwrap_err();
        assert!(matches!(error, RunnerError::Protocol(_)), "got error: {:?}", error);
    }

    /// A reply lost, then the runner gone: the command may have run, so
    /// it must not read as one that never did (a relaunch would replay it).
    #[test]
    fn a_runner_gone_after_the_request_went_out_is_a_lost_reply_not_a_refusal() {
        let attempts = Arc::new(Mutex::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Takes the first request and drops it; then nothing listens.
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
        });
        let counted = attempts.clone();
        let connect: Connector = Arc::new(move || {
            let mut n = counted.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Ok(Box::new(TcpStream::connect(("127.0.0.1", port)).unwrap()) as Box<dyn RunnerStream>)
            } else {
                Err(RunnerError::Refused)
            }
        });
        let error = RunnerClient::new(connect, "x").call("tap", json!({"x": 1, "y": 1})).unwrap_err();
        assert!(matches!(error, RunnerError::Lost(_)), "{error:?}");
        assert_eq!(*attempts.lock().unwrap(), 2);
    }
}
