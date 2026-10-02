//! The index schema. The index is derived data: a version change deletes the
//! file and rebuilds rather than migrating.

/// Bump on any change below.
pub const SCHEMA_VERSION: &str = "1";

pub const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL); -- schema_version

CREATE TABLE sessions(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent TEXT NOT NULL,
  session_id TEXT NOT NULL,
  file_path TEXT NOT NULL,
  custom_title TEXT,
  ai_title TEXT,
  last_prompt TEXT,
  first_prompt TEXT,
  cwd TEXT,
  cwd_key TEXT,
  branch TEXT,
  created_ms INTEGER,
  updated_ms INTEGER,
  message_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX sessions_updated ON sessions(updated_ms);
CREATE INDEX sessions_cwd_key ON sessions(cwd_key);

CREATE TABLE files(
  path TEXT PRIMARY KEY,
  agent TEXT NOT NULL,
  byte_offset INTEGER NOT NULL,
  mtime_ms INTEGER NOT NULL,
  size_bytes INTEGER NOT NULL,
  session_row_id INTEGER,
  fail_count INTEGER NOT NULL DEFAULT 0,
  failed_mtime_ms INTEGER
);

CREATE TABLE messages(
  id INTEGER PRIMARY KEY,
  session_row_id INTEGER NOT NULL,
  role TEXT NOT NULL,
  ts_ms INTEGER
);
CREATE INDEX messages_session ON messages(session_row_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
  user_text, assistant_text, tool_text, identifiers,
  tokenize = "unicode61 tokenchars '_.-/+'",
  detail = full
);
"#;

/// `messages_fts` column index per role (`identifiers` is column 3).
pub fn text_column(role: super::turn::Role) -> usize {
    match role {
        super::turn::Role::User => 0,
        super::turn::Role::Assistant => 1,
        super::turn::Role::Tool => 2,
    }
}
