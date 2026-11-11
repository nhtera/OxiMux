//! What a person has already seen of a session, and how loudly it may speak
//! (the V034 `agent_session_prefs`).
//!
//! Whether a session still *needs you* is not a fact about the agent. A
//! failure you have looked at is no longer an alert; a session you want to
//! come back to is unread however healthy it is; a chatty child can be muted.
//! None of that can be derived from the agent's own state, so it is persisted
//! here — and it has to be, or a relaunch would replay every old failure into
//! the badge.
//!
//! **One timestamp, not two.** Viewing a session and dismissing one of its
//! alerts both write `last_viewed_at`: an alert whose edge is older than that
//! has been seen, a newer one has not. A second `dismissed_at` would only
//! create two orders to reconcile.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, params};

/// How loudly one session's alerts may speak. The Settings rows that let a
/// person choose come with the notification-modes phase; until then every
/// session is [`NotifyMode::Default`] and behaves as today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyMode {
    /// Follow the global notification settings.
    #[default]
    Default,
    /// Every alert, including the informational ones.
    All,
    /// Only what needs a person.
    NeedsYou,
    /// Nothing: no banner, no push. The rail still shows it.
    Muted,
}

impl NotifyMode {
    pub fn as_text(self) -> &'static str {
        match self {
            NotifyMode::Default => "default",
            NotifyMode::All => "all",
            NotifyMode::NeedsYou => "needs_you",
            NotifyMode::Muted => "muted",
        }
    }

    /// An unrecognised value reads as [`NotifyMode::Default`]: a row written
    /// by a newer build never silences a session on an older one.
    pub fn from_text(text: &str) -> Self {
        match text {
            "all" => NotifyMode::All,
            "needs_you" => NotifyMode::NeedsYou,
            "muted" => NotifyMode::Muted,
            _ => NotifyMode::Default,
        }
    }
}

/// One session's prefs. A session with no row reads as the default, so nothing
/// has to be written before a session can be looked at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionPrefs {
    /// When the person last had this session in front of them, in Unix
    /// milliseconds — the clock `SessionHealth.last_activity_at` uses, which is
    /// the only thing this is ever compared against. The column itself holds
    /// RFC-3339, for a database a person can read.
    pub last_viewed_at: Option<i64>,
    /// "Mark as unread": keep standing out although it has been viewed.
    pub unread_override: bool,
    pub notify_mode: NotifyMode,
    /// The session's subtree is folded shut in the rail (read by the subagent
    /// tree phase; stored here because V034 is immutable once merged).
    pub collapsed: bool,
}

/// The prefs key of a chat session. Chat ids come from providers and run roots
/// are named `team:<run>` / `plan:<run>`, so the prefix is what keeps a chat
/// whose provider named it `plan:7` out of a run's row.
pub fn chat_key(session_id: &str) -> String {
    format!("{CHAT}{session_id}")
}

/// The one spelling of a chat's prefix, so minting a key and reading one back
/// cannot drift apart.
const CHAT: &str = "chat:";

/// The session id a chat's key names, or `None` when the key is not a chat's.
pub fn chat_id(key: &str) -> Option<&str> {
    key.strip_prefix(CHAT)
}

/// The prefs key of a terminal (cockpit) agent, by its `agent_sessions.id`.
pub fn cockpit_key(db_id: &str) -> String {
    format!("cockpit:{db_id}")
}

/// The prefs table, backed by the app's SQLite database. Cloning shares the
/// connection.
#[derive(Clone)]
pub struct SessionPrefsRepo {
    conn: Arc<Mutex<Connection>>,
}

impl SessionPrefsRepo {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// One session's prefs, or the default when it has no row.
    pub fn get(&self, session_key: &str) -> Result<SessionPrefs> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT last_viewed_at, unread_override, notify_mode, collapsed \
                 FROM agent_session_prefs WHERE session_key = ?1",
                params![session_key],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .context("read session prefs")?;
        Ok(match row {
            None => SessionPrefs::default(),
            Some((viewed, unread, mode, collapsed)) => SessionPrefs {
                last_viewed_at: viewed.as_deref().and_then(parse_ms),
                unread_override: unread != 0,
                notify_mode: NotifyMode::from_text(&mode),
                collapsed: collapsed != 0,
            },
        })
    }

    /// Every session that has a row, for a host that wants them all at once
    /// rather than one query per session on every health change.
    pub fn all(&self) -> Result<Vec<(String, SessionPrefs)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_key, last_viewed_at, unread_override, notify_mode, collapsed \
                 FROM agent_session_prefs",
            )
            .context("prepare session prefs scan")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    SessionPrefs {
                        last_viewed_at: r.get::<_, Option<String>>(1)?.as_deref().and_then(parse_ms),
                        unread_override: r.get::<_, i64>(2)? != 0,
                        notify_mode: NotifyMode::from_text(&r.get::<_, String>(3)?),
                        collapsed: r.get::<_, i64>(4)? != 0,
                    },
                ))
            })
            .context("scan session prefs")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("read session prefs row")?);
        }
        Ok(out)
    }

    /// The person has this session in front of them (or dismissed one of its
    /// alerts): every edge older than `now` has been seen, and the explicit
    /// unread mark is spent.
    ///
    /// Nothing here moves backwards. `last_viewed_at` only ever advances, so a
    /// stale call — a tick that raced a newer view — cannot un-see something;
    /// and a view older than the row's last write does not clear an unread
    /// mark made after it, which would silently undo what the person asked
    /// for.
    pub fn mark_viewed(&self, session_key: &str, now: DateTime<Local>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_session_prefs (session_key, last_viewed_at, unread_override, updated_at) \
             VALUES (?1, ?2, 0, ?2) \
             ON CONFLICT(session_key) DO UPDATE SET \
                 last_viewed_at = MAX(COALESCE(last_viewed_at, ?2), ?2), \
                 unread_override = CASE WHEN ?2 >= updated_at THEN 0 ELSE unread_override END, \
                 updated_at = MAX(updated_at, ?2)",
            params![session_key, stamp(now)],
        )
        .context("mark session viewed")?;
        Ok(())
    }

    /// "Mark as unread": the person wants it to keep standing out.
    pub fn mark_unread(&self, session_key: &str, now: DateTime<Local>) -> Result<()> {
        self.set_flag(session_key, "unread_override", true, now)
    }

    pub fn set_notify_mode(&self, session_key: &str, mode: NotifyMode, now: DateTime<Local>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_session_prefs (session_key, notify_mode, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(session_key) DO UPDATE SET notify_mode = ?2, updated_at = ?3",
            params![session_key, mode.as_text(), stamp(now)],
        )
        .context("set session notify mode")?;
        Ok(())
    }

    pub fn set_collapsed(&self, session_key: &str, collapsed: bool, now: DateTime<Local>) -> Result<()> {
        self.set_flag(session_key, "collapsed", collapsed, now)
    }

    fn set_flag(&self, session_key: &str, column: &str, value: bool, now: DateTime<Local>) -> Result<()> {
        // `column` is one of this module's own literals, never caller input.
        let sql = format!(
            "INSERT INTO agent_session_prefs (session_key, {column}, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(session_key) DO UPDATE SET {column} = ?2, updated_at = ?3"
        );
        let conn = self.conn.lock().unwrap();
        conn.execute(&sql, params![session_key, i64::from(value), stamp(now)])
            .with_context(|| format!("set session {column}"))?;
        Ok(())
    }

    /// Forget one session's prefs (its row is gone for good: a cancelled
    /// agent's tree was collected).
    pub fn forget(&self, session_key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM agent_session_prefs WHERE session_key = ?1", params![session_key])
            .context("forget session prefs")?;
        Ok(())
    }
}

/// A chat whose session id changed (a provider that names its own session
/// after launch) keeps its prefs: without this, the new id reads as never
/// viewed and every alert the person already dealt with comes back.
///
/// Runs inside the caller's transaction, beside the lineage, receipt and retry
/// rows that follow a session the same way.
///
/// Both ids having a row is rare (an id a provider reuses) and the two are
/// records of the same session, so they are **merged** rather than one
/// dropped: the later view wins, so nothing already seen is un-seen; an unread
/// mark on either side survives, because swallowing an alert is worse than
/// showing one twice; and the mode and fold state come from the old id, the
/// row whose tab the person has actually been using.
pub(crate) fn rekey_in(tx: &rusqlite::Transaction<'_>, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    let Some(from) = row_in(tx, old)? else { return Ok(()) };
    tx.execute("DELETE FROM agent_session_prefs WHERE session_key = ?1", params![old])
        .context("drop rekeyed session prefs")?;
    let merged = match row_in(tx, new)? {
        None => from,
        Some(into) => Row {
            // ISO-8601 in UTC, which is what `stamp` writes, orders lexically.
            last_viewed_at: [from.last_viewed_at, into.last_viewed_at].into_iter().flatten().max(),
            unread_override: from.unread_override || into.unread_override,
            notify_mode: from.notify_mode,
            collapsed: from.collapsed,
        },
    };
    tx.execute(
        "INSERT INTO agent_session_prefs              (session_key, last_viewed_at, unread_override, notify_mode, collapsed, updated_at)          VALUES (?1, ?2, ?3, ?4, ?5, ?6)          ON CONFLICT(session_key) DO UPDATE SET              last_viewed_at = ?2, unread_override = ?3, notify_mode = ?4, collapsed = ?5, updated_at = ?6",
        params![
            new,
            merged.last_viewed_at,
            i64::from(merged.unread_override),
            merged.notify_mode,
            i64::from(merged.collapsed),
            stamp(chrono::Local::now()),
        ],
    )
    .context("rekey session prefs")?;
    Ok(())
}

/// One row as it is stored, so a rekey can merge two of them without going
/// through the millisecond form and back.
struct Row {
    last_viewed_at: Option<String>,
    unread_override: bool,
    notify_mode: String,
    collapsed: bool,
}

fn row_in(tx: &rusqlite::Transaction<'_>, key: &str) -> Result<Option<Row>> {
    tx.query_row(
        "SELECT last_viewed_at, unread_override, notify_mode, collapsed          FROM agent_session_prefs WHERE session_key = ?1",
        params![key],
        |r| {
            Ok(Row {
                last_viewed_at: r.get(0)?,
                unread_override: r.get::<_, i64>(1)? != 0,
                notify_mode: r.get(2)?,
                collapsed: r.get::<_, i64>(3)? != 0,
            })
        },
    )
    .optional()
    .context("read session prefs for rekey")
}

fn stamp(at: DateTime<Local>) -> String {
    at.with_timezone(&chrono::Utc).to_rfc3339()
}

/// Unparseable timestamps read as "never viewed": an alert is shown rather
/// than silently swallowed by a corrupt row.
fn parse_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis())
}

#[cfg(test)]
#[path = "session_prefs_tests.rs"]
mod tests;
