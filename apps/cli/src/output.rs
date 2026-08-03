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
    pub exit: u8,
}

impl Failure {
    pub fn new(code: &'static str, exit: u8, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), next_steps: Vec::new(), exit }
    }

    pub fn with_steps(mut self, steps: impl IntoIterator<Item = String>) -> Self {
        self.next_steps = steps.into_iter().collect();
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
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "error": {
                            "code": failure.code,
                            "message": failure.message,
                            "next_steps": failure.next_steps,
                        }
                    })
                );
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
