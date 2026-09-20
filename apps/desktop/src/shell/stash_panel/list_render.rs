//! Pure render-data helpers for the stash list. Tests live alongside in
//! `tests/stash_panel_list.rs`.
//!
//! # Why the row no longer says `stash@{N}`
//!
//! The address costs ~18 characters of a 340px row and drives no decision the
//! user makes at a glance — two stashes are told apart by their message, not
//! by their position in a stack that shifts under every drop. It moves to the
//! tooltip (here) and, in Phase 6, to the context menu.
//!
//! # Separator
//!
//! `·` between metadata fields, matching the graph rows above. A field that is
//! empty is omitted rather than rendered blank: an entry written by
//! `git stash store` legitimately carries no branch (verified), so
//! `“msg” — , 3 months ago` would read as a bug in our parser.

use oximux_core::StashEntry;

/// Joins metadata fields. Matches the graph row's separator.
const SEP: &str = " · ";

/// Shown in place of a message a stash does not carry.
const NO_MESSAGE: &str = "(no message)";

/// Longest message the tooltip will show, in characters.
///
/// GPUI lays a tooltip out at its min size, which for text means "do not
/// wrap", and the popup carries no max width — so an uncapped message makes a
/// tooltip wider than the window, which is then repositioned to x=0 and
/// clipped. Capping the string is layout-independent and testable, where a
/// `max_w` on the element would only move the overflow. 120 characters is
/// still several times what the row itself can show.
const TOOLTIP_MESSAGE_CHARS: usize = 120;

/// The row's primary text: the stash's own message, never its address.
pub fn row_message(entry: &StashEntry) -> String {
    let msg = entry.message.trim();
    if msg.is_empty() {
        NO_MESSAGE.to_string()
    } else {
        msg.to_string()
    }
}

/// The row's secondary text: `branch · relative`, with either half dropped
/// when git gave us nothing for it.
pub fn row_meta(entry: &StashEntry) -> String {
    join(&[entry.branch.trim(), entry.relative.trim()])
}

/// Hover text. Carries what the row deliberately drops — the `stash@{N}`
/// address and the absolute date — plus the full message, which is the whole
/// reason a user hovers a row that truncates.
///
/// `file_count` is `Option` because the file list is fetched lazily on expand
/// (Phase 5): a row that has never been expanded shows no count rather than a
/// confident, wrong `0`.
pub fn row_tooltip(entry: &StashEntry, file_count: Option<usize>) -> String {
    let address = format!("stash@{{{}}}", entry.stash_ref.index);
    let files = file_count.map(|n| {
        if n == 1 {
            "1 file".to_string()
        } else {
            format!("{n} files")
        }
    });
    let meta = join(&[
        &address,
        entry.branch.trim(),
        &absolute_date(entry.created_at),
        files.as_deref().unwrap_or_default(),
    ]);
    format!("{}\n{meta}", clip(&row_message(entry), TOOLTIP_MESSAGE_CHARS))
}

/// First `max` characters of `s`, with an ellipsis when anything was cut.
/// Counts characters, not bytes: a stash message is arbitrary UTF-8.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

/// `3 Aug 2026` in the viewer's own timezone — the tooltip's counterpart to
/// the row's relative age, for when "7 weeks ago" is not precise enough.
pub fn absolute_date(created_at: i64) -> String {
    absolute_date_in(created_at, &chrono::Local)
}

/// The timezone-explicit form. Production passes `chrono::Local`; tests pass a
/// fixed offset, because a formatter that reads the machine's clock settings
/// can only be asserted against itself.
pub fn absolute_date_in<Tz>(created_at: i64, tz: &Tz) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    chrono::DateTime::from_timestamp(created_at, 0)
        .map(|dt| dt.with_timezone(tz).format("%-d %b %Y").to_string())
        .unwrap_or_default()
}

/// Join the non-empty fragments with [`SEP`].
fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(SEP)
}
