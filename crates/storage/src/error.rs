//! Typed errors for the storage layer.
//!
//! Anything that crosses the `oximux-storage` public boundary maps into
//! `StorageError`. Internally the runner uses `?` over `rusqlite::Result` and
//! converts at the boundary. `anyhow::Error: From<StorageError>` is derived
//! by `thiserror`, so call sites that already use `anyhow::Result` keep
//! working.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("failed to open SQLite database: {0}")]
    Open(#[source] rusqlite::Error),

    #[error("failed to set PRAGMA: {0}")]
    Pragma(#[source] rusqlite::Error),

    #[error("migration v{version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "schema downgrade detected: db is at version {db_version} but binary only knows version {code_version}"
    )]
    SchemaMigrationDowngrade { db_version: u32, code_version: u32 },

    #[error("query failed: {0}")]
    Query(#[source] rusqlite::Error),
}
