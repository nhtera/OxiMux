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
/// marker, the recovered cwd, and the child pid are needed here;
/// unknown fields are ignored.
#[derive(Deserialize)]
struct CheckpointMeta {
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
    ended_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
}

/// When the ring's byte cap cuts mid-line, replay starts with a torn
/// escape sequence or half a line. Advance the cut to the next line
/// boundary — but only if one appears this close to the cut, so a
/// pathological no-newline stream loses at most this much vs. losing
/// the garbled-first-line trade.
const LINE_BOUNDARY_SCAN_BYTES: usize = 4096;

/// What a dead PTY left behind, ready to apply to its replacement.
pub struct ColdRestore {
    /// Composed prefill payload: clear screen + recovered scrollback +
    /// restored marker + mode reset. Empty when the checkpoint had no
    /// replayable scrollback (e.g. the session lived entirely in the
    /// alternate screen) but still carried a usable cwd — the caller
    /// then skips the grid prefill and only spawns at the recovered
    /// directory.
    pub bytes: Vec<u8>,
    /// The shell child's last known working directory (live-resolved by
    /// the daemon at checkpoint time). `None` when the meta carried no
    /// cwd or the directory no longer exists — the caller then spawns
    /// at its own fallback cwd.
    pub cwd: Option<PathBuf>,
    /// The dead PTY's (cols, rows) at its last checkpoint. The
    /// replacement spawns at these dims so its first paint wraps for
    /// the size the restored content used; the pane's normal resize
    /// takes over right after adopt. `None` when the meta predates the
    /// fields or carried degenerate values.
    pub dims: Option<(u16, u16)>,
}

/// Where the daemon keeps its checkpoints: `<runtime dir>/checkpoints`,
/// the same runtime dir the relay supervisor passes as the socket's
/// parent (the daemon derives this exact path from its socket flag).
pub fn default_checkpoints_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dev.nhtera.oximux").join("checkpoints"))
}

/// Read and compose the cold restore for a dead PTY id. Returns `None`
/// when there is nothing (or nothing safe) to restore — the pane then
/// comes up as a plain fresh spawn, exactly like today.
pub fn read_cold_restore(checkpoints_dir: &Path, pty_id: &str) -> Option<ColdRestore> {
    if !is_safe_pty_id(pty_id) {
        return None;
    }
    let dir = checkpoints_dir.join(pty_id);
    let meta: CheckpointMeta = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
    if meta.ended_at_epoch_secs.is_some() {
        return None; // marked cleanly ended — not restorable
    }
    // The recovered cwd must still exist (the directory may have been a
    // worktree or temp path deleted since the crash) — a fresh shell
    // spawned into a missing dir fails outright on some shells.
    let cwd = (!meta.cwd.is_empty())
        .then(|| PathBuf::from(meta.cwd))
        .filter(|p| p.is_absolute() && p.is_dir());
    let dims = (meta.cols > 0 && meta.rows > 0).then_some((meta.cols, meta.rows));
    // Tail first (bounds the parse work), aligned to a line boundary
    // when the cap actually cut, then strip alternate-screen content
    // (the part that scrambles on replay).
    let scrollback = std::fs::read(dir.join("scrollback.bin")).unwrap_or_default();
    let usable = truncate_alt_screen(tail_at_line_boundary(&scrollback));
    if usable.is_empty() {
        // No replayable scrollback, but a live cwd is still worth the
        // restore: the replacement shell lands where the user was, with
        // a blank grid instead of a lone "restored" marker.
        return cwd.map(|cwd| ColdRestore {
            bytes: Vec::new(),
            cwd: Some(cwd),
            dims,
        });
    }
    let mut out =
        Vec::with_capacity(CLEAR_SCREEN.len() + usable.len() + RESTORED_MARKER.len() + MODE_RESET.len());
    out.extend_from_slice(CLEAR_SCREEN);
    out.extend_from_slice(usable);
    out.extend_from_slice(RESTORED_MARKER);
    out.extend_from_slice(MODE_RESET);
    Some(ColdRestore {
        bytes: out,
        cwd,
        dims,
    })
}

/// The byte-cap tail of the scrollback, advanced past the first `\n`
/// when the cap cut into the stream — replay then starts on a whole
/// line instead of a torn escape sequence. If no newline shows up
/// within the scan window (one enormous line), keep the raw cut: a
/// garbled first line beats dropping the window's worth of content.
fn tail_at_line_boundary(scrollback: &[u8]) -> &[u8] {
    let start = scrollback.len().saturating_sub(COLD_RESTORE_MAX_BYTES);
    if start == 0 {
        return scrollback;
    }
    let tail = &scrollback[start..];
    let scan = &tail[..tail.len().min(LINE_BOUNDARY_SCAN_BYTES)];
    match scan.iter().position(|&b| b == b'\n') {
        Some(nl) => &tail[nl + 1..],
        None => tail,
    }
}

/// The shell child's OS pid for a LIVE daemon PTY, read from the same
/// checkpoint meta (the daemon seeds it at spawn). The daemon and its
/// children run on the same host, so the caller can feed this straight
/// into the kernel cwd lookup — giving daemon-backed panes the same
/// split-cwd inheritance as in-process ones, with no wire round-trip.
///
/// Caveat: if the shell died with the daemon and the pane stayed frozen
/// long enough for the OS to recycle the pid, the value can point at an
/// unrelated process — a downstream cwd lookup would then yield a wrong
/// (but harmless) directory. Dead, un-recycled pids resolve to `None`
/// downstream, which is the common case.
pub fn read_checkpoint_pid(checkpoints_dir: &Path, pty_id: &str) -> Option<u32> {
    if !is_safe_pty_id(pty_id) {
        return None;
    }
    let meta_path = checkpoints_dir.join(pty_id).join("meta.json");
    let meta: CheckpointMeta = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
    meta.pid
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

        let restore = read_cold_restore(&base, "pty-1").expect("restorable");
        let s = String::from_utf8_lossy(&restore.bytes);
        assert!(s.contains("recovered output"));
        assert!(s.contains("--- session restored ---"));
        assert!(s.starts_with("\x1b[2J"), "clears before replaying");
        assert_eq!(restore.cwd.as_deref(), Some(Path::new("/")));
        assert_eq!(restore.dims, Some((80, 24)));

        consume_checkpoint(&base, "pty-1");
        assert!(!dir.exists(), "consumed checkpoint removed");
        assert!(
            read_cold_restore(&base, "pty-1").is_none(),
            "second read finds nothing"
        );
    }

    #[test]
    fn recovered_cwd_dropped_when_missing_or_relative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");
        for (id, cwd) in [
            ("pty-gone", "/nonexistent-dir-for-cold-restore-test"),
            ("pty-rel", "relative/dir"),
            ("pty-none", ""),
        ] {
            let dir = base.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("meta.json"),
                format!(
                    r#"{{"cwd":"{cwd}","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}}"#
                ),
            )
            .unwrap();
            std::fs::write(dir.join("scrollback.bin"), b"bytes").unwrap();
            let restore = read_cold_restore(&base, id).expect("scrollback still restorable");
            assert_eq!(restore.cwd, None, "cwd must be dropped for {id}");
        }
    }

    #[test]
    fn checkpoint_pid_read_and_backward_compat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");

        let with_pid = base.join("pty-pid");
        std::fs::create_dir_all(&with_pid).unwrap();
        std::fs::write(
            with_pid.join("meta.json"),
            br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null,"pid":777}"#,
        )
        .unwrap();
        assert_eq!(read_checkpoint_pid(&base, "pty-pid"), Some(777));

        // Meta written by a pre-pid daemon build: field absent → None.
        let legacy = base.join("pty-legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("meta.json"),
            br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#,
        )
        .unwrap();
        assert_eq!(read_checkpoint_pid(&base, "pty-legacy"), None);

        assert_eq!(read_checkpoint_pid(&base, "../escape"), None);
        assert_eq!(read_checkpoint_pid(&base, "pty-missing"), None);
    }

    #[test]
    fn cleanly_ended_not_restorable() {
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
        assert!(read_cold_restore(&base, "pty-ended").is_none());
    }

    #[test]
    fn empty_scrollback_still_recovers_cwd_without_marker() {
        // An alt-screen-only or just-spawned session leaves no replayable
        // scrollback, but its kernel-resolved cwd is still the best spawn
        // dir — and a blank pane beats a lone "restored" marker.
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");
        for (id, scrollback) in [
            ("pty-empty", Some(&b""[..])),
            ("pty-alt-only", Some(&b"\x1b[?1049hTUI ONLY"[..])),
            ("pty-meta-only", None), // crash before the first checkpoint tick
        ] {
            let dir = base.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("meta.json"),
                br#"{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#,
            )
            .unwrap();
            if let Some(bytes) = scrollback {
                std::fs::write(dir.join("scrollback.bin"), bytes).unwrap();
            }
            let restore = read_cold_restore(&base, id).expect("cwd-only restore");
            assert!(restore.bytes.is_empty(), "{id}: no prefill bytes");
            assert_eq!(restore.cwd.as_deref(), Some(Path::new("/")), "{id}");
        }

        // Nothing replayable AND no usable cwd → truly nothing to restore.
        let bare = base.join("pty-bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(
            bare.join("meta.json"),
            br#"{"cwd":"","cols":80,"rows":24,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#,
        )
        .unwrap();
        std::fs::write(bare.join("scrollback.bin"), b"").unwrap();
        assert!(read_cold_restore(&base, "pty-bare").is_none());
    }

    #[test]
    fn degenerate_or_missing_dims_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("checkpoints");
        // Meta written before the cols/rows fields existed (absent →
        // serde default 0) and a zero-dim meta both yield dims = None.
        for (id, meta) in [
            (
                "pty-no-dims",
                &br#"{"cwd":"/","started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#[..],
            ),
            (
                "pty-zero-dims",
                &br#"{"cwd":"/","cols":0,"rows":0,"started_at_epoch_secs":1,"ended_at_epoch_secs":null}"#[..],
            ),
        ] {
            let dir = base.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("meta.json"), meta).unwrap();
            std::fs::write(dir.join("scrollback.bin"), b"bytes").unwrap();
            let restore = read_cold_restore(&base, id).expect("restorable");
            assert_eq!(restore.dims, None, "{id}");
        }
    }

    #[test]
    fn capped_tail_starts_on_a_line_boundary() {
        let mut sb = Vec::new();
        sb.extend_from_slice(b"old\n");
        sb.extend_from_slice(b"torn-line-part");
        sb.extend_from_slice(b"\nkeep-me\r\n");
        sb.resize(sb.len() + COLD_RESTORE_MAX_BYTES - 20, b'z');
        // The cap cut lands inside "torn-line-part"; the trim must skip
        // the rest of that torn line and start at "keep-me".
        let tail = tail_at_line_boundary(&sb);
        assert!(tail.starts_with(b"keep-me\r\n"));
        assert!(!tail.windows(4).any(|w| w == b"torn"));
    }

    #[test]
    fn capped_tail_without_nearby_newline_keeps_raw_cut() {
        // One enormous line: dropping to the next newline would discard
        // everything, so the raw (possibly torn) cut is kept.
        let sb = vec![b'y'; COLD_RESTORE_MAX_BYTES + 100];
        let tail = tail_at_line_boundary(&sb);
        assert_eq!(tail.len(), COLD_RESTORE_MAX_BYTES);
    }

    #[test]
    fn uncapped_scrollback_not_trimmed() {
        // No cut happened — even a leading partial line is real content.
        let sb = b"no-newline-here".to_vec();
        assert_eq!(tail_at_line_boundary(&sb), sb.as_slice());
    }
}
