//! The note that tells an agent what a restart cut short.
//!
//! After OxiMux restarts in the middle of a turn, the agent's process and its
//! background tasks are gone, but the agent does not know it: resumed, it would
//! carry on from a state that no longer exists. The note says so, once. A root
//! session gets it on the user's next message (never a billed turn of its own);
//! a child may get it as its own recovery turn. Shared by every host, so the
//! wording is the same wherever the agent reads it.

/// The note for a restart at `restarted_at` (a human timestamp) that stopped
/// the turn and the `stopped` background tasks (their descriptions).
pub fn build(restarted_at: &str, stopped: &[String]) -> String {
    let mut note = format!("OxiMux restarted at {restarted_at}. Your previous turn was interrupted.");
    let stopped: Vec<&str> = stopped.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !stopped.is_empty() {
        note.push_str(&format!(" These background tasks were stopped: {}.", stopped.join("; ")));
    }
    note.push_str(" Check the current state (git status, files) before continuing.");
    note
}

/// `text` with the note ahead of it, as one message.
pub fn attach(note: &str, text: &str) -> String {
    if text.trim().is_empty() { note.to_string() } else { format!("{note}\n\n{text}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_names_the_restart_and_what_it_stopped() {
        let note = build("2026-11-03 09:12", &["npm test".into(), " ".into(), "review the diff".into()]);
        assert_eq!(
            note,
            "OxiMux restarted at 2026-11-03 09:12. Your previous turn was interrupted. \
             These background tasks were stopped: npm test; review the diff. \
             Check the current state (git status, files) before continuing."
        );
    }

    #[test]
    fn with_nothing_stopped_the_list_is_left_out() {
        assert!(!build("t", &[]).contains("background tasks"));
    }

    #[test]
    fn attach_puts_the_note_first() {
        assert_eq!(attach("N", "do it"), "N\n\ndo it");
        assert_eq!(attach("N", "  "), "N");
    }
}
