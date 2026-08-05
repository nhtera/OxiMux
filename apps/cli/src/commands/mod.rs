//! One module per verb family. Each returns `(json data, human text)` so the
//! output layer owns the envelope and the verbs own only their content.

use crate::cli::exit;
use crate::output::Failure;

/// A correlation id for a prompt: unique per send, within and across processes.
///
/// The pid seeds the high bits so two concurrent CLIs never collide; a counter
/// fills the low bits so two sends from ONE process do not either. The previous
/// `pid << 16` was constant for a process's whole life despite a comment
/// claiming per-send uniqueness — inert today, because nothing host-side reads
/// the field yet, and a trap the moment something does: a loop of sends would
/// hand every turn the same id.
///
/// Wrapping past 65_535 sends in one process is accepted. That is far beyond
/// any real invocation, and the alternative — widening or erroring — buys
/// nothing for a field whose only job is telling concurrent sends apart.
pub fn next_corr_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed) & 0xffff;
    (u64::from(std::process::id()) << 16) | n
}

/// Resolve a prompt argument, reading stdin when it is exactly `-`.
///
/// The convention every unix filter uses, and this CLI wants it for three
/// reasons an argv-only prompt cannot serve:
///
/// - **Size.** `ARG_MAX` is 1 MB on macOS and ~2 MB on Linux (measured, not
///   assumed: `execve` here starts failing between 900 KB and 1 MB of argv).
///   A prompt built from a large diff or a pasted transcript reaches that, and
///   `Argument list too long` reads as a shell bug rather than a limit of this
///   command. The ceiling is high enough that this is the least of the three
///   reasons — the two below apply at every size.
/// - **Quoting.** A heredoc keeps the text exactly as written; embedding the
///   same thing in an argv string means escaping whatever the shell would eat.
/// - **Disclosure.** An argv prompt is visible in `ps` to every account on the
///   box and lands in shell history. On a shared server that is at odds with
///   how carefully the rest of this codebase treats what it writes down.
///
/// Only the exact string `-` is special. A prompt that legitimately *is* a
/// single hyphen can be sent as `./-` or with a trailing space; anything longer
/// that merely starts with `-` is already claimed by the argument parser.
pub fn resolve_prompt(prompt: String) -> Result<String, Failure> {
    if prompt != "-" {
        return Ok(prompt);
    }
    use std::io::Read as _;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).map_err(|e| {
        Failure::new("stdin", exit::USAGE, format!("could not read the prompt from stdin: {e}"))
            .with_steps(["`-` reads the prompt from stdin; pipe or redirect something into it".into()])
    })?;
    // A prompt of pure whitespace is a caller mistake — usually an empty pipe,
    // or `-` given with nothing redirected, where the command would otherwise
    // hang on a terminal waiting for input nobody knows to type. Better to say
    // so than to spend an agent turn on it.
    if text.trim().is_empty() {
        return Err(Failure::new("empty-prompt", exit::USAGE, "the prompt read from stdin is empty")
            .with_steps([
                "pipe the prompt in: `… | oximux send <SESSION> -`".into(),
                "or pass it as an argument instead of `-`".into(),
            ]));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything that is not exactly `-` is the prompt itself, untouched.
    ///
    /// The boundary matters: a prompt merely *starting* with `-` is already the
    /// argument parser's business, and one that merely contains a `-` is
    /// ordinary text. Only the whole-string match may reach for stdin, or a
    /// prompt like "-" inside a sentence would block on a terminal.
    #[test]
    fn only_a_bare_hyphen_means_stdin() {
        for prompt in ["fix the -v flag", "--", "- ", " -", "a - b", ""] {
            assert_eq!(
                resolve_prompt(prompt.to_string()).expect("passed through"),
                prompt,
                "{prompt:?} must be taken literally"
            );
        }
    }

    /// Two sends from ONE process get different ids.
    ///
    /// The regression this guards: the previous `pid << 16` was constant for a
    /// process's whole life while its comment claimed per-send uniqueness.
    /// Inert while nothing reads the field, and a silent collision the moment
    /// something does.
    #[test]
    fn each_send_gets_its_own_correlation_id() {
        let ids: Vec<u64> = (0..8).map(|_| next_corr_id()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids repeated within one process: {ids:?}");

        // And every one still carries this process's seed, which is what keeps
        // two concurrent CLIs apart.
        let seed = u64::from(std::process::id()) << 16;
        for id in ids {
            assert_eq!(id & !0xffff, seed, "id {id} lost its pid seed");
        }
    }
}

pub mod agent_context;
pub mod attach;
pub mod git;
pub mod heartbeat;
pub mod hosts;
pub mod model;
pub mod pair;
pub mod permit;
pub mod run;
pub mod schedule;
pub mod send;
pub mod session_ctl;
pub mod sessions;
pub mod state;
pub mod status;
pub mod team;
pub mod term;
pub mod transcript;
pub mod update;
pub mod wait;
pub mod worktree;
