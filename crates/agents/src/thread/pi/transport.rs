//! Newline-JSON client over `pi --mode rpc`'s stdio.
//!
//! The framing, correlation, stderr fold, and EOF drain live in the shared
//! [`ndjson_transport`] core (pi and omp speak the same envelope; the tricky
//! parts must not fork). This wrapper keeps pi's typed surface: commands go
//! out as [`PiCommand`], correlated answers come back as [`RpcResponse`], and
//! everything else is classified into [`Inbound`] for the worker.
//!
//! One deliberate behavior refinement vs. the pre-extraction client: a
//! correlated response that fails to parse as [`RpcResponse`] now fails the
//! request immediately (with the parse error) instead of timing it out while
//! the frame detoured to the event channel — strictly more diagnosable, and
//! unreachable with well-formed pi output (every `RpcResponse` field beyond
//! `command`/`success` is optional).
//!
//! [`ndjson_transport`]: super::super::ndjson_transport

use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::super::ndjson_transport::NdjsonRpcClient;
use super::protocol::{classify, Inbound, PiCommand, RpcResponse};

/// Diagnostic name for error strings — kept byte-identical to the
/// pre-extraction messages ("pi --mode rpc exited", …).
const NAME: &str = "pi --mode rpc";

/// A cloneable handle to a running `pi --mode rpc` child's stdin + pending
/// registry. Cloning shares the same child.
#[derive(Clone)]
pub struct PiRpcClient {
    inner: NdjsonRpcClient,
}

impl PiRpcClient {
    /// Spawn an already-built command (the real `pi`, or a fake in tests) and
    /// wire its stdout into the reader/router.
    pub fn spawn_command(cmd: Command) -> Result<(PiRpcClient, Receiver<Inbound>, Child)> {
        let (inner, raw_rx, child) = NdjsonRpcClient::spawn_command(cmd, NAME, None)?;
        // Classify off the reader thread's raw channel; the worker keeps the
        // same `Inbound` vocabulary it always consumed.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for v in raw_rx {
                if tx.send(classify(v)).is_err() {
                    break;
                }
            }
        });
        Ok((PiRpcClient { inner }, rx, child))
    }

    /// Whether pi is still running (its stdout hasn't hit EOF).
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// pi's captured stderr so far (bounded tail).
    pub fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    /// A fresh correlation id.
    pub fn next_id(&self, prefix: &str) -> String {
        self.inner.next_id(prefix)
    }

    /// Send a command and block (up to `timeout`) for its response.
    ///
    /// NOTE on `abort`: pi emits `response:abort` *after* `agent_settled`, so a
    /// request-shaped abort naturally blocks until the turn has fully settled.
    /// That is useful (it is a free cancel-and-wait) but it is not instant —
    /// callers that must not block should use [`Self::send`].
    pub fn request(&self, cmd: PiCommand, timeout: Duration) -> Result<RpcResponse> {
        let line = serde_json::to_string(&cmd).context("serialize pi command")?;
        let v = self.inner.request_value(cmd.id(), &line, timeout)?;
        serde_json::from_value::<RpcResponse>(v).context("parse pi response")
    }

    /// Send a command without waiting for its response.
    pub fn send(&self, cmd: PiCommand) -> Result<()> {
        let line = serde_json::to_string(&cmd).context("serialize pi command")?;
        self.inner.send_line(&line)
    }

    /// Close stdin, signalling pi that no more commands are coming. Idempotent;
    /// any later write fails rather than silently going nowhere.
    pub fn close_stdin(&self) {
        self.inner.close_stdin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_correlates_by_string_id_and_events_broadcast() {
        // A fake pi: read the command, answer with a matching id, then emit an event.
        let script = r#"
read line
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"abc"}}\n'
printf '{"type":"agent_settled"}\n'
sleep 0.2
"#;
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg(script);
        let (rpc, inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let r = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("response");
        assert_eq!(r.command, "get_state");
        let data = r.into_data().expect("data");
        assert_eq!(data["sessionId"], "abc");
        match inbound.recv_timeout(Duration::from_secs(5)) {
            Ok(Inbound::Event(v)) => assert_eq!(v["type"], "agent_settled"),
            _ => panic!("expected the agent_settled event"),
        }
    }

    #[test]
    fn child_exit_fails_pending_fast_and_carries_stderr() {
        // Read the command, complain on stderr, exit without responding. The
        // pending request must fail via the EOF drain (fast), and the error must
        // carry pi's stderr — "exited" alone can't distinguish bad auth from a
        // bad flag.
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg("read line; echo 'boom: bad auth' >&2; exit 1");
        let (rpc, _inbound, mut child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        assert!(rpc.is_alive());
        let start = std::time::Instant::now();
        let err = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(30))
            .expect_err("must fail once the child exits");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must fail fast via the EOF drain, not wait out the 30s timeout"
        );
        assert!(err.to_string().contains("bad auth"), "stderr must reach the error: {err}");
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!rpc.is_alive(), "must be marked dead after exit");
    }

    #[test]
    fn framing_splits_on_lf_only_and_preserves_u2028() {
        // pi warns payload strings may contain U+2028/U+2029 and that clients
        // must split on \n alone. The separator must survive the round trip.
        let sep = "\u{2028}";
        let payload = json!({
            "id": "s1", "type": "response", "command": "get_state", "success": true,
            "data": {"sessionId": format!("a{sep}b")}
        });
        // serde_json emits U+2028 raw (it is valid inside a JSON string).
        let line = serde_json::to_string(&payload).unwrap();
        assert!(line.contains(sep), "the fixture must actually contain U+2028");
        let script = format!("read line\nprintf '%s\\n' '{line}'\nsleep 0.2\n");
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let data = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("response")
            .into_data()
            .expect("data");
        assert_eq!(
            data["sessionId"], format!("a{sep}b"),
            "U+2028 must not be treated as a line break"
        );
    }

    #[test]
    fn non_json_and_unknown_events_are_not_fatal() {
        let script = r#"
read line
printf 'pi: some human diagnostic\n'
printf '{"type":"some_future_event","x":1}\n'
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"ok"}}\n'
sleep 0.2
"#;
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let data = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("a non-JSON line and an unknown event must not break the stream")
            .into_data()
            .expect("data");
        assert_eq!(data["sessionId"], "ok");
    }

    #[test]
    fn a_failed_response_surfaces_pis_error_text() {
        let script = r#"
read line
printf '{"id":"c1","type":"response","command":"compact","success":false,"error":"Nothing to compact (session too small)"}\n'
sleep 0.2
"#;
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let r = rpc
            .request(PiCommand::Abort { id: "c1".into() }, Duration::from_secs(5))
            .expect("the response itself arrives");
        assert!(!r.success);
        let err = r.into_data().expect_err("a failed response is an Err");
        assert!(err.to_string().contains("session too small"), "got {err}");
    }
}
