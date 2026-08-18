//! Closing inline markers that a half-streamed block has left hanging.
//!
//! While a reply streams, `**bold` with no closer yet is not bold — it is the
//! literal characters `*`, `*`, `b`, … So the reader watches plain text arrive,
//! and then the closing `**` lands, two characters vanish, the run restyles, and
//! every wrap point after it shifts. On a paragraph of any length that is a
//! visible twitch on almost every token.
//!
//! The repair: append synthetic closers and parse *that*, for display only. The
//! canonical tree is never touched, so a marker that genuinely never closes
//! settles honestly with a single flip at completion rather than jittering the
//! whole way there.
//!
//! **Deliberately approximate.** This is not a CommonMark delimiter-run
//! implementation and should not become one; it is a guess about what the next
//! few tokens will make true, and it is re-guessed on every append. Known and
//! accepted:
//!
//! - `2**3` briefly bolds the `3` — intraword `**` is treated as an opener.
//! - An opener followed only by more markers stays literal for a chunk.
//! - Reference links (`[text][label]`) are mended as inline links.
//!
//! Every one of those is repaired by the next append or by the settle, which is
//! the property that makes approximation acceptable here. If a specific repair
//! ever proves unstable, delete that repair — partial mending is strictly better
//! than none, and much better than a wrong guess held confidently.

/// Append whatever closers `src` is missing, or `None` if it is already whole.
///
/// `None` is the common case and costs one scan; callers rely on that to skip
/// the reparse entirely.
pub(crate) fn close_hanging(src: &str) -> Option<String> {
    let mut closers = Vec::new();
    let masked = mask_code_spans(src);

    // Unclosed inline code first, because everything else is measured against
    // the masked copy — a `**` inside a code span is not an opener.
    if let Some(run) = unclosed_backtick_run(src) {
        closers.push((run.at, "`".repeat(run.len)));
    }

    if let Some(at) = unclosed_link(&masked) {
        closers.push(at);
    }

    for (marker, closer) in [("~~", "~~"), ("**", "**"), ("__", "__")] {
        if let Some(at) = unpaired_marker(&masked, marker) {
            closers.push((at, closer.to_string()));
        }
    }
    for marker in ["*", "_"] {
        if let Some(at) = unpaired_single(&masked, marker) {
            closers.push((at, marker.to_string()));
        }
    }

    // A trailing line of only `-` or `=` is a setext underline, which would
    // silently promote the paragraph above it to a heading — a whole-block
    // restyle mid-stream, and the most jarring flicker of the lot. A zero-width
    // space is enough to stop it being an underline while showing nothing.
    let setext_guard = trailing_setext_line(src);

    if closers.is_empty() && !setext_guard {
        return None;
    }

    // Innermost first: the marker opened last must be closed first, or
    // `**a *b` would close as `**a *b**` + `*` and invert the nesting.
    closers.sort_by_key(|(at, _)| std::cmp::Reverse(*at));

    let mut out = String::with_capacity(src.len() + 8);
    out.push_str(src);
    if setext_guard {
        out.push('\u{200b}');
    }
    for (_, closer) in closers {
        out.push_str(&closer);
    }
    Some(out)
}

struct BacktickRun {
    at: usize,
    len: usize,
}

/// Replace the contents of closed code spans with spaces.
///
/// Positions are preserved so every later scan can report an offset into the
/// original string. Only *closed* spans are masked; an unclosed one is the
/// hanging marker we are here to fix.
fn mask_code_spans(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let len = run_len(b, i, b'`');
        match find_run(b, i + len, b'`', len) {
            Some(close) => {
                for slot in out.iter_mut().take(close + len).skip(i) {
                    *slot = b' ';
                }
                i = close + len;
            }
            // Unclosed: nothing after it is markup either, so stop masking and
            // let `unclosed_backtick_run` own the rest.
            None => break,
        }
    }
    // Masking only ever writes ASCII spaces over whole byte runs that began at
    // a `\``, so the result is still valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

fn unclosed_backtick_run(src: &str) -> Option<BacktickRun> {
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let len = run_len(b, i, b'`');
        match find_run(b, i + len, b'`', len) {
            Some(close) => i = close + len,
            None => return Some(BacktickRun { at: i, len }),
        }
    }
    None
}

/// An unclosed `[text](url` or a bare `[text`.
///
/// Both mend to something that parses as a link so the *text* styles
/// immediately. A bare `[text` gets a sentinel destination: without one the
/// bracket stays literal until the URL arrives, and then the whole `](…)` tail
/// collapses at once — the largest reflow of any marker class.
fn unclosed_link(masked: &str) -> Option<(usize, String)> {
    let b = masked.as_bytes();
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'[' => open = Some(i),
            b']' => {
                if let Some(at) = open {
                    // `](` starts a destination; is it closed?
                    if b.get(i + 1) == Some(&b'(') {
                        match find_byte(b, i + 2, b')') {
                            Some(close) => {
                                open = None;
                                i = close;
                            }
                            None => return Some((at, ")".to_string())),
                        }
                    } else {
                        open = None;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    open.map(|at| (at, "](#)".to_string()))
}

/// Can a delimiter run at `at` open or close emphasis at all?
///
/// The CommonMark rule in miniature: a run followed by whitespace cannot open,
/// and one preceded by whitespace cannot close. A run that can do neither —
/// `a * b`, where the asterisk is spaced on both sides — is literal text, and
/// counting it was making the mender append a closer to a source that had no
/// hanging marker.
///
/// Only the whitespace half of the rule; the punctuation clauses are what make
/// the real thing intricate, and they do not change the outcome for text an
/// agent writes.
fn can_flank(b: &[u8], at: usize, len: usize) -> bool {
    let before = b[..at].last().copied();
    let after = b.get(at + len).copied();
    let opens = after.is_some_and(|c| !c.is_ascii_whitespace());
    let closes = before.is_some_and(|c| !c.is_ascii_whitespace());
    opens || closes
}

/// The position of an unpaired two-character marker, if the count is odd.
fn unpaired_marker(masked: &str, marker: &str) -> Option<usize> {
    let positions = marker_positions(masked, marker);
    (positions.len() % 2 == 1).then(|| *positions.last().expect("odd implies non-empty"))
}

/// The position of an unpaired one-character emphasis marker.
///
/// Runs of two or more are the two-character marker's business, and a `*` used
/// as a bullet (`* item` at a line start) is not emphasis at all.
fn unpaired_single(masked: &str, marker: &str) -> Option<usize> {
    let b = masked.as_bytes();
    let m = marker.as_bytes()[0];
    let mut positions = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] != m {
            i += 1;
            continue;
        }
        let len = run_len(b, i, m);
        if len == 1 && !is_bullet_position(b, i) && can_flank(b, i, 1) {
            positions.push(i);
        }
        i += len;
    }
    (positions.len() % 2 == 1).then(|| *positions.last().expect("odd implies non-empty"))
}

/// Is this `*` a list bullet — line-start (modulo indent) and followed by a
/// space?
fn is_bullet_position(b: &[u8], at: usize) -> bool {
    if b.get(at + 1) != Some(&b' ') {
        return false;
    }
    b[..at].iter().rev().take_while(|c| **c != b'\n').all(|c| *c == b' ')
}

fn marker_positions(masked: &str, marker: &str) -> Vec<usize> {
    let b = masked.as_bytes();
    let m = marker.as_bytes()[0];
    let width = marker.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] != m {
            i += 1;
            continue;
        }
        let len = run_len(b, i, m);
        if len >= width && can_flank(b, i, len) {
            out.push(i);
        }
        i += len;
    }
    out
}

/// Does the source end with a line of only `-` or `=`?
fn trailing_setext_line(src: &str) -> bool {
    let Some(last) = src.lines().next_back() else {
        return false;
    };
    // A source ending in a newline has already committed to the next block, so
    // the underline (if any) is settled and not ours to guard.
    if src.ends_with('\n') {
        return false;
    }
    let t = last.trim();
    t.len() >= 2 && (t.bytes().all(|c| c == b'-') || t.bytes().all(|c| c == b'='))
}

fn run_len(b: &[u8], at: usize, byte: u8) -> usize {
    b[at..].iter().take_while(|c| **c == byte).count()
}

/// The start of the next run of exactly `len` `byte`s at or after `from`.
fn find_run(b: &[u8], from: usize, byte: u8, len: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] != byte {
            i += 1;
            continue;
        }
        let run = run_len(b, i, byte);
        if run == len {
            return Some(i);
        }
        i += run;
    }
    None
}

fn find_byte(b: &[u8], from: usize, byte: u8) -> Option<usize> {
    b[from..].iter().position(|c| *c == byte).map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_text_needs_no_repair() {
        for src in [
            "plain text",
            "**bold** and *italic*",
            "`code` here",
            "[link](http://x)",
            "~~struck~~",
            "a * b", // a lone asterisk with spaces is not emphasis in CommonMark
        ] {
            assert_eq!(close_hanging(src), None, "should need no mend: {src}");
        }
    }

    #[test]
    fn hanging_markers_are_closed() {
        assert_eq!(close_hanging("**bold").as_deref(), Some("**bold**"));
        assert_eq!(close_hanging("*it").as_deref(), Some("*it*"));
        assert_eq!(close_hanging("~~str").as_deref(), Some("~~str~~"));
        assert_eq!(close_hanging("`co").as_deref(), Some("`co`"));
        assert_eq!(close_hanging("``co").as_deref(), Some("``co``"));
    }

    #[test]
    fn links_mend_so_the_text_styles_immediately() {
        assert_eq!(close_hanging("[text](http://partial").as_deref(), Some("[text](http://partial)"));
        // Bare `[text` gets a sentinel: waiting for the real URL means the whole
        // `](...)` tail collapses in one frame when it lands.
        assert_eq!(close_hanging("see [text").as_deref(), Some("see [text](#)"));
    }

    /// Nesting order is the one thing an approximate mender still has to get
    /// right — closing outermost-first inverts the emphasis.
    #[test]
    fn closers_are_emitted_innermost_first() {
        assert_eq!(close_hanging("**bold and *italic").as_deref(), Some("**bold and *italic***"));
    }

    #[test]
    fn markers_inside_closed_code_spans_are_not_openers() {
        assert_eq!(close_hanging("`a ** b` done"), None);
        assert_eq!(close_hanging("`**` and `*`"), None);
    }

    #[test]
    fn escaped_markers_are_not_openers() {
        assert_eq!(close_hanging(r"a \* b"), None);
        assert_eq!(close_hanging(r"\*not emphasis\*"), None);
        // Both asterisks escaped, then a real unclosed one: the escape must not
        // swallow the marker that genuinely hangs.
        assert_eq!(close_hanging(r"\*lit\* **real").as_deref(), Some(r"\*lit\* **real**"));
    }

    /// A `*` bullet is list syntax, not a hanging emphasis marker. Without this
    /// every streaming bullet list would grow a spurious `*`.
    #[test]
    fn list_bullets_are_not_emphasis() {
        assert_eq!(close_hanging("* one\n* two"), None);
        assert_eq!(close_hanging("  * indented"), None);
    }

    /// The largest flicker of all: a trailing `---` promotes the paragraph above
    /// it to a heading, restyling the whole block.
    #[test]
    fn a_trailing_underline_is_defused() {
        let mended = close_hanging("Title\n---").expect("needs the guard");
        assert!(mended.ends_with('\u{200b}'), "got {mended:?}");
        // Once the line is committed the underline is real and not ours to
        // second-guess.
        assert_eq!(close_hanging("Title\n---\n"), None);
    }

    #[test]
    fn multibyte_text_does_not_panic_or_corrupt() {
        for src in ["héllo **wörld", "日本語 `コード", "emoji 🎉 *ital"] {
            let mended = close_hanging(src).expect("has a hanging marker");
            assert!(mended.starts_with(src), "the original must be a prefix: {mended:?}");
        }
    }
}
