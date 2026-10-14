//! An app's requests, from the debugger's `Network` events: correlated by
//! `requestId`, redacted as they come in ([`super::redact`]), and bounded
//! (at most [`MAX_REQUESTS`], [`MAX_BYTES`] of text together).

use std::collections::HashMap;

use serde_json::Value;

use super::redact;

/// The most requests kept: later ones are dropped (and said to be).
pub const MAX_REQUESTS: usize = 200;
/// The most text kept, over every request's URL, headers and bodies.
pub const MAX_BYTES: usize = 64 * 1024;
/// The longest body kept.
pub const MAX_BODY: usize = 4096;
/// The longest URL kept: one huge URL must not crowd out every other request.
const MAX_URL: usize = 2048;
/// The largest response body fetched (it is cut to [`MAX_BODY`] anyway).
const MAX_FETCH: u64 = 1 << 20;

/// One request, as far as it got.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub method: String,
    /// Redacted.
    pub url: String,
    pub status: Option<u16>,
    pub ms: Option<u32>,
    pub size: Option<u64>,
    pub mime: Option<String>,
    pub failed: Option<String>,
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    /// The debugger's clock (seconds), for the time it took.
    started: f64,
    /// Bytes counted from `dataReceived`, when the end has none.
    received: u64,
}

impl Request {
    fn text_len(&self) -> usize {
        let headers: usize = self.request_headers.iter().chain(&self.response_headers).map(|(k, v)| k.len() + v.len()).sum();
        self.url.len() + headers + self.request_body.as_ref().map_or(0, String::len) + self.response_body.as_ref().map_or(0, String::len)
    }
}

/// Requests seen so far.
#[derive(Debug, Default)]
pub struct Collector {
    requests: Vec<Request>,
    by_id: HashMap<String, usize>,
    grep: Option<String>,
    details: bool,
    dropped: bool,
}

impl Collector {
    /// `grep`: only URLs containing it; `details`: headers and bodies too.
    pub fn new(grep: Option<String>, details: bool) -> Self {
        Self { grep: grep.filter(|g| !g.is_empty()), details, ..Self::default() }
    }

    /// Take one debugger event. Returns the request whose response body to
    /// fetch now (`Network.getResponseBody`): it just finished, details are
    /// on, and its body is text.
    pub fn event(&mut self, method: &str, params: &Value) -> Option<String> {
        let id = params.get("requestId").and_then(Value::as_str)?.to_owned();
        let time = params.get("timestamp").and_then(Value::as_f64);
        if method == "Network.requestWillBeSent" {
            self.started(id, params, time.unwrap_or_default());
            return None;
        }
        let details = self.details;
        let request = self.by_id.get(&id).and_then(|&i| self.requests.get_mut(i))?;
        let took = |r: &Request| time.map(|t| ((t - r.started).max(0.0) * 1000.0).round() as u32);
        match method {
            "Network.responseReceived" => {
                let response = params.get("response")?;
                request.status = response.get("status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok());
                request.mime = response.get("mimeType").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_owned);
                if details {
                    request.response_headers = headers(response.get("headers"));
                }
            }
            "Network.dataReceived" => {
                request.received += params.get("dataLength").and_then(Value::as_u64).unwrap_or(0);
            }
            "Network.loadingFinished" => {
                request.ms = took(request);
                let encoded = params.get("encodedDataLength").and_then(Value::as_u64).filter(|n| *n > 0);
                request.size = encoded.or((request.received > 0).then_some(request.received)).or(Some(0));
                // The encoded size may be compressed: the larger count rules.
                let small = encoded.unwrap_or(0).max(request.received) <= MAX_FETCH;
                return (details && small && request.mime.as_deref().is_some_and(is_text)).then_some(id);
            }
            "Network.loadingFailed" => {
                request.ms = took(request);
                let canceled = params.get("canceled").and_then(Value::as_bool) == Some(true);
                let why = params.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty());
                request.failed = Some(if canceled { "canceled".into() } else { why.unwrap_or("failed").to_owned() });
            }
            _ => {}
        }
        None
    }

    fn started(&mut self, id: String, params: &Value, time: f64) {
        let Some(req) = params.get("request") else { return };
        let raw_url = req.get("url").and_then(Value::as_str).unwrap_or_default();
        if self.grep.as_deref().is_some_and(|g| !raw_url.contains(g)) {
            return;
        }
        // A redirect re-sends the same id: the request starts over.
        let i = match self.by_id.get(&id) {
            Some(&i) => i,
            None if self.requests.len() >= MAX_REQUESTS => {
                self.dropped = true;
                return;
            }
            None => {
                self.by_id.insert(id, self.requests.len());
                self.requests.push(Request::default());
                self.requests.len() - 1
            }
        };
        let request = &mut self.requests[i];
        *request = Request::default();
        request.started = time;
        request.method = req.get("method").and_then(Value::as_str).unwrap_or("GET").to_owned();
        request.url = redact::cut(&redact::url(raw_url), MAX_URL);
        if self.details {
            request.request_headers = headers(req.get("headers"));
            request.request_body = req.get("postData").and_then(Value::as_str).map(|b| redact::cut(&redact::body(b), MAX_BODY));
        }
    }

    /// A response body fetched for `id`.
    pub fn body(&mut self, id: &str, body: &str, base64: bool) {
        let Some(request) = self.by_id.get(id).and_then(|&i| self.requests.get_mut(i)) else { return };
        request.response_body = Some(if base64 { "(binary)".into() } else { redact::cut(&redact::body(body), MAX_BODY) });
    }

    /// The requests in the order they started, and whether any were
    /// dropped (over [`MAX_REQUESTS`] or [`MAX_BYTES`]).
    pub fn finish(self) -> (Vec<Request>, bool) {
        let mut dropped = self.dropped;
        let mut budget = MAX_BYTES;
        let mut out = Vec::with_capacity(self.requests.len());
        for request in self.requests {
            let len = request.text_len();
            if len > budget {
                dropped = true;
                break;
            }
            budget -= len;
            out.push(request);
        }
        (out, dropped)
    }
}

/// A CDP headers object as redacted pairs, in a stable order.
fn headers(value: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(map)) = value else { return Vec::new() };
    let mut out: Vec<(String, String)> = map
        .iter()
        .map(|(name, v)| (name.clone(), redact::header(name, v.as_str().map_or_else(|| v.to_string(), str::to_owned).as_str())))
        .collect();
    out.sort_by_key(|(name, _)| name.to_ascii_lowercase());
    out
}

/// A body worth fetching as text.
fn is_text(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime.starts_with("text/")
        || mime.ends_with("json")
        || mime.ends_with("+xml")
        || matches!(mime.as_str(), "application/xml" | "application/javascript" | "application/x-www-form-urlencoded")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: &str = include_str!("../../tests/fixtures/rn/network_events.ndjson");

    fn replay(grep: Option<&str>, details: bool) -> (Collector, Vec<String>) {
        let mut c = Collector::new(grep.map(Into::into), details);
        let mut fetch = Vec::new();
        for line in EVENTS.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).unwrap();
            if let Some(id) = c.event(v["method"].as_str().unwrap(), &v["params"]) {
                fetch.push(id);
            }
        }
        (c, fetch)
    }

    #[test]
    fn requests_are_correlated_and_timed() {
        let (c, fetch) = replay(None, false);
        assert!(fetch.is_empty(), "no bodies unless asked");
        let (requests, dropped) = c.finish();
        assert!(!dropped);
        assert_eq!(requests.len(), 3, "{requests:?}");
        let post = &requests[0];
        assert_eq!((post.method.as_str(), post.status, post.ms, post.size), ("POST", Some(200), Some(412), Some(586)));
        assert_eq!(post.url, "https://httpbin.org/anything/probe?user=[redacted]&token=[redacted]");
        assert!(post.request_headers.is_empty() && post.request_body.is_none(), "details only with --bodies");
        assert_eq!(requests[1].failed.as_deref(), Some("The Internet connection appears to be offline."));
        assert_eq!(requests[2].status, None, "still pending");
    }

    #[test]
    fn details_are_redacted() {
        let (mut c, fetch) = replay(None, true);
        assert_eq!(fetch, ["1"], "a finished JSON response is fetched");
        c.body("1", r#"{"json":{"password":"FAKE-BODY-SECRET"},"headers":{"Authorization":"Bearer FAKE-HEADER-SECRET"}}"#, false);
        let (requests, _) = c.finish();
        let all = format!("{requests:?}");
        for secret in ["FAKE-QUERY-SECRET", "FAKE-HEADER-SECRET", "FAKE-BODY-SECRET"] {
            assert!(!all.contains(secret), "{secret} leaked: {all}");
        }
        let post = &requests[0];
        assert!(post.request_headers.iter().any(|(k, v)| k == "Authorization" && v == "[redacted]"), "{:?}", post.request_headers);
        assert!(post.request_body.as_deref().is_some_and(|b| b.contains("\"note\":\"hello\"")));
        assert!(post.response_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")));
    }

    #[test]
    fn grep_and_bounds_hold() {
        let (c, _) = replay(Some("offline-check"), false);
        assert_eq!(c.finish().0.len(), 1);
        let mut c = Collector::new(None, false);
        for i in 0..MAX_REQUESTS + 5 {
            let params = serde_json::json!({ "requestId": i.to_string(), "timestamp": 1.0, "request": { "url": "https://a.b/", "method": "GET" } });
            c.event("Network.requestWillBeSent", &params);
        }
        let (requests, dropped) = c.finish();
        assert_eq!((requests.len(), dropped), (MAX_REQUESTS, true));
        // A URL is cut; the bytes, over every request, are bounded.
        let mut c = Collector::new(None, true);
        let long = format!("https://a.b/{}", "x".repeat(MAX_BYTES));
        for i in 0..20 {
            let params = serde_json::json!({ "requestId": i.to_string(), "timestamp": 1.0, "request": { "url": long, "method": "POST", "postData": "y".repeat(MAX_BYTES) } });
            c.event("Network.requestWillBeSent", &params);
        }
        let (requests, dropped) = c.finish();
        assert!(requests.len() < 20 && dropped, "{}", requests.len());
        assert!(requests.iter().all(|r| r.url.len() <= MAX_URL + '…'.len_utf8()));
    }
}
