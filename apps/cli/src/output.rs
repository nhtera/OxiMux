//! One output convention for every verb: humans get prose with a `Next step:`
//! hint; `--json` gets `{"ok":true,"data":…}` or
//! `{"ok":false,"error":{code,message,next_steps}}` — the same `next_steps`
//! field driving both, so the guidance cannot drift between the two renderings.

use serde_json::{Value, json};

/// A verb's failure: what happened, what to do about it, and the exit code
/// that classifies it for scripts.
#[derive(Debug)]
pub struct Failure {
    /// Stable machine-readable identifier (`"unreachable"`, `"denied"`, …).
    pub code: &'static str,
    pub message: String,
    /// Concrete follow-ups, most likely first. Rendered verbatim in JSON and
    /// as `Next step:` lines for humans.
    pub next_steps: Vec<String>,
    /// Machine-readable detail for failures that leave something behind worth
    /// addressing — chiefly a `session_id`.
    ///
    /// A failure is not always the end of the work. `run --turn-timeout` gives
    /// up on a turn while the agent keeps going, and that is precisely when a
    /// script most needs the handle: to decide the permission that stalled it,
    /// or to stop it. Without this the id survived only inside `message` and
    /// `next_steps` prose, leaving a caller to regex it back out of an English
    /// sentence. Omitted from the JSON entirely when `None`, so the documented
    /// `{code, message, next_steps}` envelope is unchanged for every failure
    /// that has nothing to add.
    ///
    /// Boxed because `Failure` is the `Err` of nearly every function in this
    /// crate: an inline `Value` is 32 bytes and pushed the type past
    /// `clippy::result_large_err`'s threshold, taxing every `Result` in the CLI
    /// to carry a payload almost none of them use. The indirection is paid only
    /// by the failures that actually have data.
    pub data: Option<Box<Value>>,
    pub exit: u8,
}

impl Failure {
    pub fn new(code: &'static str, exit: u8, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), next_steps: Vec::new(), data: None, exit }
    }

    pub fn with_steps(mut self, steps: impl IntoIterator<Item = String>) -> Self {
        self.next_steps = steps.into_iter().collect();
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }
}

/// Print a verb's outcome and return the process exit code.
pub fn render(json_mode: bool, outcome: Result<(Value, String), Failure>) -> u8 {
    match outcome {
        Ok((data, human)) => {
            if json_mode {
                println!("{}", json!({ "ok": true, "data": data }));
            } else {
                println!("{human}");
            }
            crate::cli::exit::OK
        }
        Err(failure) => {
            if json_mode {
                let mut error = json!({
                    "code": failure.code,
                    "message": failure.message,
                    "next_steps": failure.next_steps,
                });
                // Added only when there is something to add, so the envelope a
                // caller parses is byte-identical for every failure that has
                // nothing extra to say.
                if let Some(data) = failure.data {
                    error["data"] = *data;
                }
                println!("{}", json!({ "ok": false, "error": error }));
            } else {
                eprintln!("error: {}", failure.message);
                for step in &failure.next_steps {
                    eprintln!("Next step: {step}");
                }
            }
            failure.exit
        }
    }
}
