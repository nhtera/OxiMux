//! Cookie SQLite readers (Chromium + Firefox).
//!
//! Both copy the source DB (plus its `-wal`/`-shm` sidecars) into a scratch dir
//! before opening it read-only — a running browser holds a WAL lock on the live
//! file, and copying sidesteps it without disturbing the source. Chromium values
//! are decrypted via [`super::decrypt`]; Firefox values are plaintext.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::{ParsedCookie, SameSite};

/// Outcome of reading one cookie DB: the importable cookies plus a count of
/// rows skipped (e.g. encrypted values we couldn't unlock).
pub struct ReadOutcome {
    pub cookies: Vec<ParsedCookie>,
    pub skipped: usize,
}

/// Microseconds between the Windows/WebKit epoch (1601-01-01) and the Unix
/// epoch — Chromium stores `expires_utc` in WebKit microseconds.
const WEBKIT_EPOCH_OFFSET_S: i64 = 11_644_473_600;

/// Copy `db` (+ `-wal`/`-shm` sidecars) into a temp dir; returns the temp dir
/// guard (kept alive by the caller) and the copied DB path.
fn copy_to_temp(db: &Path) -> std::io::Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let name = db.file_name().unwrap_or_else(|| std::ffi::OsStr::new("Cookies"));
    let dest = dir.path().join(name);
    std::fs::copy(db, &dest)?;
    // Sidecars are best-effort: a missing one just means the latest uncommitted
    // WAL pages aren't visible, never a fatal error.
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!("{}{suffix}", name.to_string_lossy()));
        if side.exists() {
            let _ = std::fs::copy(&side, dir.path().join(side.file_name().unwrap()));
        }
    }
    Ok((dir, dest))
}

fn open_readonly(db: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// Chromium `samesite`: -1 unspecified, 0 none, 1 lax, 2 strict.
fn chromium_samesite(v: i64) -> SameSite {
    match v {
        0 => SameSite::None,
        1 => SameSite::Lax,
        2 => SameSite::Strict,
        _ => SameSite::Unspecified,
    }
}

/// Firefox `sameSite`: 0 none, 1 lax, 2 strict.
fn firefox_samesite(v: i64) -> SameSite {
    match v {
        1 => SameSite::Lax,
        2 => SameSite::Strict,
        _ => SameSite::None,
    }
}

/// Read a Chromium cookie DB. `key` decrypts `encrypted_value`; when it's
/// `None` (Keychain denied) any encrypted row is counted as skipped.
pub fn read_chromium(db: &Path, key: Option<[u8; 16]>) -> Result<ReadOutcome, String> {
    let (_guard, copy) = copy_to_temp(db).map_err(|e| format!("copy cookie DB: {e}"))?;
    let conn = open_readonly(&copy).map_err(|e| format!("open cookie DB: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT host_key, name, value, path, is_secure, \
             samesite, expires_utc, encrypted_value FROM cookies",
        )
        .map_err(|e| format!("query cookies: {e}"))?;

    let mut cookies = Vec::new();
    let mut skipped = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,   // host_key
                r.get::<_, String>(1)?,   // name
                r.get::<_, String>(2)?,   // value (plaintext)
                r.get::<_, String>(3)?,   // path
                r.get::<_, i64>(4)? != 0, // is_secure
                r.get::<_, i64>(5)?,      // samesite
                r.get::<_, i64>(6)?,      // expires_utc (WebKit µs)
                r.get::<_, Vec<u8>>(7)?,  // encrypted_value
            ))
        })
        .map_err(|e| format!("read cookies: {e}"))?;

    for row in rows.flatten() {
        let (host, name, value, path, secure, ss, expires_utc, encrypted) = row;
        let resolved = if encrypted.is_empty() {
            Some(value)
        } else {
            key.and_then(|k| super::decrypt::decrypt_value(&encrypted, &k))
        };
        let Some(value) = resolved else {
            skipped += 1;
            continue;
        };
        cookies.push(ParsedCookie {
            host,
            name,
            value,
            path,
            secure,
            same_site: chromium_samesite(ss),
            expires_unix_s: (expires_utc != 0)
                .then(|| expires_utc / 1_000_000 - WEBKIT_EPOCH_OFFSET_S),
        });
    }
    Ok(ReadOutcome { cookies, skipped })
}

/// Read a Firefox cookie DB (plaintext values); expired cookies are dropped.
pub fn read_firefox(db: &Path, now_unix_s: i64) -> Result<ReadOutcome, String> {
    let (_guard, copy) = copy_to_temp(db).map_err(|e| format!("copy cookie DB: {e}"))?;
    let conn = open_readonly(&copy).map_err(|e| format!("open cookie DB: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT name, value, host, path, expiry, isSecure, \
             sameSite FROM moz_cookies",
        )
        .map_err(|e| format!("query cookies: {e}"))?;

    let mut cookies = Vec::new();
    let mut skipped = 0usize;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,   // name
                r.get::<_, String>(1)?,   // value
                r.get::<_, String>(2)?,   // host
                r.get::<_, String>(3)?,   // path
                r.get::<_, i64>(4)?,      // expiry (unix s, 0 = session)
                r.get::<_, i64>(5)? != 0, // isSecure
                r.get::<_, i64>(6)?,      // sameSite
            ))
        })
        .map_err(|e| format!("read cookies: {e}"))?;

    for row in rows.flatten() {
        let (name, value, host, path, expiry, secure, ss) = row;
        if expiry > 0 && expiry < now_unix_s {
            skipped += 1;
            continue;
        }
        cookies.push(ParsedCookie {
            host,
            name,
            value,
            path,
            secure,
            same_site: firefox_samesite(ss),
            expires_unix_s: (expiry > 0).then_some(expiry),
        });
    }
    Ok(ReadOutcome { cookies, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_firefox_plaintext_and_drops_expired() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("cookies.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_cookies(name TEXT, value TEXT, host TEXT, path TEXT, \
             expiry INTEGER, isSecure INTEGER, isHttpOnly INTEGER, sameSite INTEGER);
             INSERT INTO moz_cookies VALUES('sid','live','.example.com','/',4102444800,1,1,1);
             INSERT INTO moz_cookies VALUES('old','dead','.example.com','/',100,0,0,0);
             INSERT INTO moz_cookies VALUES('sess','keep','.example.com','/',0,0,0,2);",
        )
        .unwrap();
        drop(conn);

        let out = read_firefox(&db, 1_000_000_000).unwrap();
        assert_eq!(out.skipped, 1); // the expired one
        let names: Vec<_> = out.cookies.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"sid"));
        assert!(names.contains(&"sess"));
        assert!(!names.contains(&"old"));
        let sid = out.cookies.iter().find(|c| c.name == "sid").unwrap();
        assert!(sid.secure);
        assert!(matches!(sid.same_site, SameSite::Lax));
        assert_eq!(sid.expires_unix_s, Some(4102444800));
        let sess = out.cookies.iter().find(|c| c.name == "sess").unwrap();
        assert_eq!(sess.expires_unix_s, None); // session cookie
    }

    #[test]
    fn reads_chromium_plaintext_values_without_key() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("Cookies");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies(host_key TEXT, name TEXT, value TEXT, path TEXT, \
             is_secure INTEGER, is_httponly INTEGER, samesite INTEGER, \
             expires_utc INTEGER, encrypted_value BLOB);
             INSERT INTO cookies VALUES('.example.com','a','plain','/',1,0,1,0,X'');",
        )
        .unwrap();
        drop(conn);

        let out = read_chromium(&db, None).unwrap();
        assert_eq!(out.cookies.len(), 1);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.cookies[0].value, "plain");
        assert_eq!(out.cookies[0].expires_unix_s, None);
    }

    #[test]
    fn chromium_encrypted_without_key_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("Cookies");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies(host_key TEXT, name TEXT, value TEXT, path TEXT, \
             is_secure INTEGER, is_httponly INTEGER, samesite INTEGER, \
             expires_utc INTEGER, encrypted_value BLOB);
             INSERT INTO cookies VALUES('.x.com','a','','/',1,0,1,0,X'763130AABB');",
        )
        .unwrap();
        drop(conn);

        let out = read_chromium(&db, None).unwrap();
        assert!(out.cookies.is_empty());
        assert_eq!(out.skipped, 1);
    }
}
