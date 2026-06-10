//! Cold restore from the relay daemon's disk scrollback checkpoints.
//!
//! The daemon checkpoints each PTY's replay ring to disk every few
//! seconds and removes the checkpoint on every clean end — so a
//! checkpoint that still exists when its PTY is gone means the daemon
//! died (crash, SIGKILL, host reboot). When the restore reconcile
//! cold-spawns a replacement PTY for a dead attach hint, this module
//! recovers that scrollback and composes the bytes to prefill the new
//! pane's grid: clear screen, recovered scrollback (alternate-screen
//! content stripped), a dim "session restored" marker, and a terminal
//! mode reset.
//!
//! This is deliberately DISTINCT from routine restore, which stays
//! replay-free (a surviving PTY's content comes only from the daemon's
//! byte-for-byte reattach replay). Cold restore accepts reflow
//! imperfection for the rare crash path because the alternative is an
//! empty pane — and the marker makes clear the content is history, not
//! live state.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Upper bound on restored bytes, matching the established pane-buffer
/// prefill budget. The checkpoint ring is at most 1 MiB; taking the
/// tail keeps grid-prefill parsing cheap on the main thread.
const COLD_RESTORE_MAX_BYTES: usize = 512 * 1024;

const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[3J\x1b[H";
const RESTORED_MARKER: &[u8] = b"\r\n\x1b[2m--- session restored ---\x1b[0m\r\n\r\n";
// The recovered scrollback can carry mode-setting bytes from a TUI that
// died with the daemon (cursor style, progressive keyboard enhancement
// stack, mouse tracking, focus reporting, bracketed paste). No live
// process is around to unset them, so reset to what the fresh shell
// expects.
const MODE_RESET: &[u8] =
    b"\x1b[0 q\x1b[<99u\x1b[=0u\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l";

const ALT_SCREEN_ON: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";

/// Mirror of the daemon's checkpoint meta. Only the restorability
/// marker is needed here; unknown fields are ignored.
#[derive(Deserialize)]
struct CheckpointMeta {
    ended_at_epoch_secs: Option<u64>,
}

/// Where the daemon keeps its checkpoints: `<runtime dir>/checkpoints`,
/// the same runtime dir the relay supervisor passes as the socket's
/// parent (the daemon derives this exact path from its socket flag).
pub fn default_checkpoints_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dev.nhtera.oximux").join("checkpoints"))
}

/// Read and compose the cold-restore prefill bytes for a dead PTY id.
/// Returns `None` when there is nothing (or nothing safe) to restore —
/// the pane then comes up as a plain fresh spawn, exactly like today.
pub fn read_cold_restore_bytes(checkpoints_dir: &Path, pty_id: &str) -> Option<Vec<u8>> {
    if !is_safe_pty_id(pty_id) {
        return None;
    }
    let dir = checkpoints_dir.join(pty_id);
    let meta: CheckpointMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
    if meta.ended_at_epoch_secs.is_some() {
        return None; // marked cleanly ended — not restorable
    }
    let scrollback = std::fs::read(dir.join("scrollback.bin")).ok()?;
    if scrollback.is_empty() {
        return None;
    }
    // Tail first (bounds the parse work — mid-sequence cuts are the
    // same trade the daemon's ring buffer already makes), then strip
    // alternate-screen content (the part that scrambles on replay).
    let start = scrollback.len().saturating_sub(COLD_RESTORE_MAX_BYTES);
    let usable = truncate_alt_screen(&scrollback[start..]);
    if usable.is_empty() {
        return None;
    }
    let mut out =
        Vec::with_capacity(CLEAR_SCREEN.len() + usable.len() + RESTORED_MARKER.len() + MODE_RESET.len());
    out.extend_from_slice(CLEAR_SCREEN);
    out.extend_from_slice(usable);
    out.extend_from_slice(RESTORED_MARKER);
    out.extend_from_slice(MODE_RESET);
    Some(out)
}

/// Delete a consumed checkpoint so the same crash isn't restored twice.
/// Best-effort: a leftover is reaped by the daemon's age GC eventually.
pub fn consume_checkpoint(checkpoints_dir: &Path, pty_id: &str) {
    if !is_safe_pty_id(pty_id) {
        return;
    }
    let _ = std::fs::remove_dir_all(checkpoints_dir.join(pty_id));
}

// PTY ids are daemon-minted UUIDs, but they round-trip through
// persisted rows — refuse anything that could traverse out of the
// checkpoints dir before joining it onto a path we read or delete.
fn is_safe_pty_id(pty_id: &str) -> bool {
    !pty_id.is_empty()
        && pty_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Raw scrollback from a session that ran a full-screen TUI contains
/// alternate-screen switches whose content replays as garbage (it was
/// painted with absolute positioning for a specific grid). Track
/// on/off nesting and cut before the outermost UNMATCHED on — content
/// up to there is normal-buffer output that replays fine; a matched
/// on/off pair means the TUI exited and the buffer was restored, so
/// it's safe to keep scanning past it.
fn truncate_alt_screen(data: &[u8]) -> &[u8] {
    let mut depth = 0usize;
    let mut outermost_unmatched_on = None;
    let mut search_from = 0usize;
    while search_from < data.len() {
        let on = find_subslice(data, ALT_SCREEN_ON, search_from);
        let off = find_subslice(data, ALT_SCREEN_OFF, search_from);
        // Whichever switch comes next in the stream wins; the two
        // sequences differ only in their final byte so they can never
        // overlap.
        let (is_on, idx) = match (on, off) {
            (None, None) => break,
            (Some(on_idx), None) => (true, on_idx),
            (None, Some(off_idx)) => (false, off_idx),
            (Some(on_idx), Some(off_idx)) => {
                if on_idx < off_idx {
                    (true, on_idx)
                } else {
                    (false, off_idx)
                }
            }
        };
        if is_on {
            if depth == 0 {
                outermost_unmatched_on = Some(idx);
            }
            depth += 1;
            search_from = idx + ALT_SCREEN_ON.len();
        } else {
            depth = depth.saturating_sub(1);
            search_from = idx + ALT_SCREEN_OFF.len();
        }
    }
    match (depth > 0, outermost_unmatched_on) {
        (true, Some(idx)) => &data[..idx],
        _ => data,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_plain_output() {
        assert_eq!(truncate_alt_screen(b"hello\r\nworld"), b"hello\r\nworld");
    }

    #[test]
    fn truncate_cuts_before_unmatched_alt_screen_on() {
        let data = b"before\x1b[?1049hTUI CONTENT".to_vec();
        assert_eq!(truncate_alt_screen(&data), b"before");
    }

    #[test]
    fn truncate_keeps_matched_alt_screen_round_trip() {
        // TUI entered AND exited: the buffer was restored, everything
        // after the off is normal output again — keep all of it.
        let data = b"a\x1b[?1049hTUI\x1b[?1049lb".to_vec();
        assert_eq!(truncate_alt_screen(&data), data.as_slice());
    }

    #[test]
    fn truncate_cuts_at_outermost_of_nested_on() {
        let data = b"x\x1b[?1049h inner \x1b[?1049h deeper".to_vec();
        assert_eq!(truncate_alt_screen(&data), b"x");
    }

    #[test]
    fn truncate_handles_unmatched_off_then_on() {
        // A leading off (entered before the ring's window) is ignored;
        // the later unmatched on still cuts.
        let data = b"\x1b[?1049lvisible\x1b[?1049hTUI".to_vec();
        assert_eq!(truncate_alt_screen(&data), b"\x1b[?1049lvisible");
    }

    #[test]
    fn unsafe_pty_ids_rejected() {
        assert!(!is_safe_pty_id("../../etc"));
        assert!(!is_safe_pty_id("a/b"));
        assert!(!is_safe_pty_id(""));
        assert!(is_safe_pty_id("0a1b2c3d-4e5f-6789-abcd-ef0123456789"));
    }

    #[test]
    fn cold_restore_roundtrip_and_consume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");
        let dir = base.join("pty-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#,
        )
        .unwrap();
        std::fs::write(dir.join("scrollback.bin"), b"recovered output").unwrap();

        let bytes = read_cold_restore_bytes(&base, "pty-1").expect("restorable");
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("recovered output"));
        assert!(s.contains("--- session restored ---"));
        assert!(s.starts_with("\x1b[2J"), "clears before replaying");

        consume_checkpoint(&base, "pty-1");
        assert!(!dir.exists(), "consumed checkpoint removed");
        assert!(
            read_cold_restore_bytes(&base, "pty-1").is_none(),
            "second read finds nothing"
        );
    }

    #[test]
    fn cleanly_ended_or_empty_not_restorable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");
        let ended = base.join("pty-ended");
        std::fs::create_dir_all(&ended).unwrap();
        std::fs::write(
            ended.join("meta.json"),
            br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":2}"#,
        )
        .unwrap();
        std::fs::write(ended.join("scrollback.bin"), b"bytes").unwrap();
        assert!(read_cold_restore_bytes(&base, "pty-ended").is_none());

        let empty = base.join("pty-empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(
            empty.join("meta.json"),
            br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#,
        )
        .unwrap();
        std::fs::write(empty.join("scrollback.bin"), b"").unwrap();
        assert!(read_cold_restore_bytes(&base, "pty-empty").is_none());
    }
}
