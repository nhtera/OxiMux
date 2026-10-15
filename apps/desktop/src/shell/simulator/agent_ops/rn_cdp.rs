//! A short-lived Chrome DevTools Protocol session with a React Native app's
//! debugger, through Metro (one per `sim rn` verb): JSON-RPC over a
//! WebSocket, on the app's tokio runtime.
//!
//! Loopback only (the target was checked: [`RnTarget`] is never off this
//! Mac), and with the `Origin` Metro insists on: its debugger endpoint
//! refuses a connection without one (a browser page's guard).

use std::collections::VecDeque;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use http::{HeaderName, HeaderValue};
use oximux_simulator::rn::targets::RnTarget;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_websockets::{ClientBuilder, Message, WebSocketStream};

/// Connecting, and the WebSocket upgrade.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A call's answer (a profile's `Profiler.stop` is given longer).
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// The most events held while a call waits for its answer.
const MAX_QUEUED: usize = 10_000;

/// Why a call failed.
#[derive(Debug, PartialEq)]
pub(super) enum CdpError {
    /// The debugger answered with an error (an unknown method, …).
    Remote(String),
    /// The connection failed or closed, or no answer came in time.
    Transport(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(why) | Self::Transport(why) => f.write_str(why),
        }
    }
}

pub(super) struct Cdp {
    ws: WebSocketStream<TcpStream>,
    next_id: u64,
    /// Events that came in while a call waited.
    events: VecDeque<(String, Value)>,
    /// Why the connection ended early, once it did.
    lost: Option<String>,
}

impl Cdp {
    pub(super) async fn connect(target: &RnTarget) -> Result<Self, CdpError> {
        let transport = |what: &str, e: &dyn std::fmt::Display| CdpError::Transport(format!("{what}: {e}"));
        let host = target.host.trim_start_matches('[').trim_end_matches(']').to_owned();
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), target.port)))
            .await
            .map_err(|_| CdpError::Transport("Metro did not accept the connection in time".into()))?
            .map_err(|e| transport("cannot reach Metro", &e))?;
        let uri = format!("ws://{}:{}{}", target.host, target.port, target.path);
        let origin = HeaderValue::from_str(&target.origin()).map_err(|e| transport("bad origin", &e))?;
        let builder = ClientBuilder::new()
            .uri(&uri)
            .map_err(|e| transport("bad debugger address", &e))?
            .add_header(HeaderName::from_static("origin"), origin)
            .map_err(|e| transport("bad header", &e))?;
        let (ws, _) = timeout(CONNECT_TIMEOUT, builder.connect_on(stream))
            .await
            .map_err(|_| CdpError::Transport("Metro did not open the debugger in time".into()))?
            .map_err(|e| transport("Metro refused the debugger connection", &e))?;
        Ok(Self { ws, next_id: 0, events: VecDeque::new(), lost: None })
    }

    /// Call `method` and wait (up to `wait`) for its answer, holding the
    /// events that arrive meanwhile for [`Self::next_event`].
    pub(super) async fn call(&mut self, method: &str, params: Value, wait: Duration) -> Result<Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws.send(Message::text(request)).await.map_err(|e| CdpError::Transport(format!("the debugger connection failed: {e}")))?;
        let deadline = Instant::now() + wait;
        loop {
            let message = self.read(deadline).await?.ok_or_else(|| CdpError::Transport(format!("no answer to {method} in time")))?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    let why = error.get("message").and_then(Value::as_str).unwrap_or("an error");
                    return Err(CdpError::Remote(format!("{method}: {why}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(event) = event_of(message)
                && self.events.len() < MAX_QUEUED
            {
                self.events.push_back(event);
            }
        }
    }

    /// The next event, waiting until `until`; `None` once it passes, or
    /// once the connection ended (then [`Self::lost`] says why).
    pub(super) async fn next_event(&mut self, until: Instant) -> Option<(String, Value)> {
        if let Some(event) = self.events.pop_front() {
            return Some(event);
        }
        if self.lost.is_some() {
            return None;
        }
        loop {
            match self.read(until).await {
                Ok(Some(message)) => {
                    if let Some(event) = event_of(message) {
                        return Some(event);
                    }
                }
                Ok(None) => return None,
                Err(e) => {
                    self.lost = Some(e.to_string());
                    return None;
                }
            }
        }
    }

    /// Why the connection ended before the watch did, if it did.
    pub(super) fn lost(&self) -> Option<&str> {
        self.lost.as_deref()
    }

    /// One JSON message before `deadline`: `Ok(None)` when it passed.
    async fn read(&mut self, deadline: Instant) -> Result<Option<Value>, CdpError> {
        loop {
            let next = match timeout_at(deadline, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err(CdpError::Transport("the app's debugger closed the connection (did the app reload?)".into())),
                Ok(Some(Err(e))) => return Err(CdpError::Transport(format!("the debugger connection failed: {e}"))),
                Ok(Some(Ok(message))) => message,
            };
            if let Some((_, reason)) = next.as_close() {
                return Err(CdpError::Transport(format!("Metro closed the debugger connection{}", if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                })));
            }
            if let Some(text) = next.as_text()
                && let Ok(value) = serde_json::from_str::<Value>(text)
            {
                return Ok(Some(value));
            }
        }
    }

    /// Close politely (best effort: the app may be gone).
    pub(super) async fn close(mut self) {
        let _ = timeout(Duration::from_millis(500), self.ws.close()).await;
    }
}

fn event_of(message: Value) -> Option<(String, Value)> {
    let method = message.get("method")?.as_str()?.to_owned();
    Some((method, message.get("params").cloned().unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio_websockets::ServerBuilder;

    use super::*;

    /// A stand-in debugger on a loopback port: answers each call with the
    /// method's echo, after sending an event first; refuses `Bad.method`.
    async fn debugger() -> (RnTarget, tokio::task::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (request, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            let origin = request.headers().get("origin").and_then(|o| o.to_str().ok()).map(str::to_owned);
            while let Some(Ok(message)) = ws.next().await {
                let Some(text) = message.as_text() else { continue };
                let call: Value = serde_json::from_str(text).unwrap();
                let event = json!({ "method": "Network.requestWillBeSent", "params": { "requestId": "1" } });
                ws.send(Message::text(event.to_string())).await.unwrap();
                let reply = if call["method"] == "Bad.method" {
                    json!({ "id": call["id"], "error": { "code": -32601, "message": "Unsupported method 'Bad.method'" } })
                } else {
                    json!({ "id": call["id"], "result": { "echo": call["method"] } })
                };
                ws.send(Message::text(reply.to_string())).await.unwrap();
            }
            origin
        });
        let target = RnTarget {
            app_id: "com.a.b".into(),
            device_name: "iPhone 17".into(),
            device_id: "d".into(),
            host: "127.0.0.1".into(),
            port,
            path: "/inspector/debug?device=x&page=1".into(),
            multiple_debuggers: true,
        };
        (target, server)
    }

    #[tokio::test]
    async fn calls_are_answered_and_events_kept_for_later() {
        let (target, server) = debugger().await;
        let mut cdp = Cdp::connect(&target).await.unwrap();
        let answer = cdp.call("Network.enable", json!({}), CALL_TIMEOUT).await.unwrap();
        assert_eq!(answer["echo"], "Network.enable");
        let refused = cdp.call("Bad.method", json!({}), CALL_TIMEOUT).await.unwrap_err();
        assert!(matches!(&refused, CdpError::Remote(why) if why.contains("Unsupported")), "{refused:?}");
        // Two events came in while the calls waited.
        let soon = Instant::now() + Duration::from_millis(200);
        assert_eq!(cdp.next_event(soon).await.map(|e| e.0).as_deref(), Some("Network.requestWillBeSent"));
        assert!(cdp.next_event(soon).await.is_some());
        assert!(cdp.next_event(Instant::now() + Duration::from_millis(50)).await.is_none(), "nothing more: the wait ends");
        assert_eq!(cdp.lost(), None, "a wait that ran out is not a lost connection");
        cdp.close().await;
        assert_eq!(server.await.unwrap().as_deref(), Some("http://127.0.0.1:".to_owned() + &target.port.to_string()).as_deref());
    }

    #[tokio::test]
    async fn a_closed_port_fails_fast() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let target = RnTarget {
            app_id: String::new(),
            device_name: String::new(),
            device_id: String::new(),
            host: "127.0.0.1".into(),
            port,
            path: "/".into(),
            multiple_debuggers: false,
        };
        assert!(matches!(Cdp::connect(&target).await.err(), Some(CdpError::Transport(_))));
    }
}
