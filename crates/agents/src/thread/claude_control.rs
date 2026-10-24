//! The parts of a Claude session the host waits on: control requests whose
//! reply matters (taking a queued line back, an interrupt), and whether the
//! session takes messages in the middle of a turn.
//!
//! **Mid-turn messages.** From [`CLAUDE_MIN_ORCH`] the CLI queues a stdin line
//! that arrives during a turn, reports it by the sender's uuid
//! (`command_lifecycle`), and folds it in at the next tool boundary or runs it
//! as the next turn. The CLI updates itself under a running app, so beyond the
//! version the session checks itself: a tagged line must draw a
//! `command_lifecycle` frame within [`SELF_CHECK`] (counted from the later of
//! the send and the CLI's `system/init`, so a slow cold start is not a fail).
//! Until it does, the session is treated as taking mid-turn lines; once it
//! fails, it never is again.
//!
//! **Replies.** A request's reply comes back on stdout among everything else.
//! The reader thread hands a `control_response` to whoever registered its
//! request id and drops the rest, so a reply never reaches the transcript.
//!
//! [`CLAUDE_MIN_ORCH`]: super::launch_posture::CLAUDE_MIN_ORCH

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// How long a tagged line may go without a `command_lifecycle` frame before
/// the session is judged not to take mid-turn lines.
pub const SELF_CHECK: Duration = Duration::from_secs(2);

/// How long a Stop waits for each reply. A wedged CLI is reaped by the
/// existing shutdown path; Stop never blocks longer than a few of these.
pub const REPLY_WITHIN: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct Controls {
    /// Request id → whoever waits for its reply.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    /// The installed CLI is new enough.
    version_ok: AtomicBool,
    /// The self-check failed: never steer this session.
    failed: AtomicBool,
    lifecycle_seen: AtomicBool,
    init_at: Mutex<Option<Instant>>,
    first_tagged_at: Mutex<Option<Instant>>,
}

impl Controls {
    pub(super) fn set_version_ok(&self, ok: bool) {
        self.version_ok.store(ok, Ordering::Relaxed);
    }

    /// The reader thread's look at each stdout line, before decoding. Returns
    /// `true` for a reply someone was waiting for (consumed here).
    pub(super) fn observe_line(&self, line: &str) -> bool {
        if line.contains("\"command_lifecycle\"") {
            self.lifecycle_seen.store(true, Ordering::Relaxed);
        }
        if line.contains("\"subtype\":\"init\"") || line.contains("\"subtype\": \"init\"") {
            self.init_at.lock().unwrap().get_or_insert_with(Instant::now);
        }
        if !line.contains("\"control_response\"") {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else { return false };
        let Some(id) = value.pointer("/response/request_id").and_then(Value::as_str) else { return false };
        match self.waiting.lock().unwrap().remove(id) {
            Some(waiter) => {
                let _ = waiter.send(value);
                true
            }
            None => false,
        }
    }

    /// A line went out under a uuid: start the self-check clock.
    pub(super) fn note_tagged_send(&self) {
        self.first_tagged_at.lock().unwrap().get_or_insert_with(Instant::now);
    }

    /// Whether this session takes mid-turn lines right now.
    pub(super) fn steer_available(&self) -> bool {
        if !self.version_ok.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed) {
            return false;
        }
        if self.lifecycle_seen.load(Ordering::Relaxed) {
            return true;
        }
        let sent = *self.first_tagged_at.lock().unwrap();
        let init = *self.init_at.lock().unwrap();
        if let (Some(sent), Some(init)) = (sent, init)
            && sent.max(init).elapsed() > SELF_CHECK
        {
            self.failed.store(true, Ordering::Relaxed);
            tracing::info!("this claude session does not report its stdin queue; child events wait for turn end");
            return false;
        }
        true
    }

    /// Send `request` (a control request carrying `request_id`) with `write`
    /// and wait up to `within` for its reply.
    pub(super) fn request(
        &self,
        request_id: &str,
        request: &Value,
        within: Duration,
        write: impl FnOnce(&Value) -> Result<()>,
    ) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(request_id.to_string(), tx);
        if let Err(err) = write(request) {
            self.waiting.lock().unwrap().remove(request_id);
            return Err(err);
        }
        let reply = rx.recv_timeout(within);
        self.waiting.lock().unwrap().remove(request_id);
        reply.map_err(|_| anyhow!("no reply to {request_id} within {} ms", within.as_millis()))
    }
}

fn next_request_id(kind: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("oximux-{kind}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// `cancel_async_message`: take a queued line back before the CLI starts on it.
pub(super) fn cancel_queued_request(uuid: &str) -> (String, Value) {
    let id = next_request_id("dequeue");
    let request = json!({"type": "control_request", "request_id": id,
        "request": {"subtype": "cancel_async_message", "message_uuid": uuid}});
    (id, request)
}

/// `interrupt`, with an id the caller knows so it can read the reply.
pub(super) fn interrupt_request() -> (String, Value) {
    let id = next_request_id("interrupt");
    let request = json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}});
    (id, request)
}

/// Whether a `cancel_async_message` reply says the line was taken back.
pub(super) fn was_dequeued(reply: &Value) -> bool {
    reply.pointer("/response/response/cancelled").and_then(Value::as_bool) == Some(true)
}

/// The lines an interrupt's reply says will still run.
pub(super) fn still_queued(reply: &Value) -> Vec<String> {
    reply
        .pointer("/response/response/still_queued")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_reaches_its_waiter_and_nothing_else_is_consumed() {
        let controls = Controls::default();
        let (id, request) = cancel_queued_request("u-1");
        assert_eq!(request["request"]["message_uuid"], "u-1");
        let reply = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{{"cancelled":true}}}}}}"#
        );
        let got = controls
            .request(&id, &request, REPLY_WITHIN, |_| {
                assert!(controls.observe_line(&reply), "consumed by its waiter");
                Ok(())
            })
            .unwrap();
        assert!(was_dequeued(&got));
        // A reply nobody waits for (the CLI answering a permission, say) is left
        // for the decoder.
        assert!(!controls.observe_line(&reply));
    }

    #[test]
    fn no_reply_in_time_is_an_error_not_a_hang() {
        let controls = Controls::default();
        let (id, request) = interrupt_request();
        assert!(controls.request(&id, &request, Duration::from_millis(20), |_| Ok(())).is_err());
    }

    #[test]
    fn an_interrupt_reply_names_what_will_still_run() {
        let reply: Value = serde_json::from_str(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"still_queued":["3123e1cb-fd95-4678-9f0b-e4f1a6aceedc"]}}}"#,
        )
        .unwrap();
        assert_eq!(still_queued(&reply), ["3123e1cb-fd95-4678-9f0b-e4f1a6aceedc"]);
        assert!(!was_dequeued(&reply));
    }

    #[test]
    fn steering_needs_the_version_and_a_session_that_reports_its_queue() {
        let controls = Controls::default();
        assert!(!controls.steer_available(), "an old or unknown CLI");
        controls.set_version_ok(true);
        assert!(controls.steer_available(), "trusted until a tagged line goes unreported");
        // A tagged line long before, and init long before: nothing reported.
        let long_ago = Instant::now() - SELF_CHECK * 3;
        *controls.first_tagged_at.lock().unwrap() = Some(long_ago);
        *controls.init_at.lock().unwrap() = Some(long_ago);
        assert!(!controls.steer_available());
        // Once failed, a late frame does not bring it back.
        controls.observe_line(r#"{"type":"command_lifecycle","command_uuid":"u","state":"queued"}"#);
        assert!(!controls.steer_available());
    }

    #[test]
    fn a_reported_queue_passes_the_check() {
        let controls = Controls::default();
        controls.set_version_ok(true);
        let long_ago = Instant::now() - SELF_CHECK * 3;
        *controls.first_tagged_at.lock().unwrap() = Some(long_ago);
        *controls.init_at.lock().unwrap() = Some(long_ago);
        controls.observe_line(r#"{"type":"command_lifecycle","command_uuid":"u","state":"queued"}"#);
        assert!(controls.steer_available());
    }

    /// Against a stand-in CLI that answers both requests: a Stop takes the
    /// unstarted line back first and interrupts second, and reports what it
    /// took back.
    #[test]
    fn a_stop_takes_back_before_it_interrupts() {
        use crate::thread::AgentConnection as _;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("order");
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *cancel_async_message*) echo dequeue >> "$LOG"
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"cancelled":true}}}\n' "$id" ;;
    *'"interrupt"'*) echo interrupt >> "$LOG"
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"still_queued":[]}}}\n' "$id" ;;
  esac
done
"#;
        let mut cmd = crate::thread::sh_fixture::sh_script(script);
        cmd.env("LOG", &log);
        let (conn, _rx) = super::super::claude_stream_json::ClaudeStreamJsonConnection::spawn_command(cmd).unwrap();
        let taken = conn.stop_turn(&["u-1".to_string()]).unwrap();
        assert_eq!(taken, ["u-1"]);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "dequeue\ninterrupt\n");
    }
}
