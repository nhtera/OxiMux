//! The core-owned Tokio runtime for the long-lived connection tasks.
//!
//! The demux pump and the event dispatcher must keep running for the connection's
//! whole life, independent of any single FFI call. They run on this shared
//! multi-thread runtime; the async FFI methods themselves are driven by uniffi's
//! own tokio bridge and only await channels these tasks service.

use std::sync::OnceLock;

use tokio::runtime::Runtime;

/// The process-wide runtime that hosts the pump + dispatcher tasks.
pub(crate) fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("build the mobile-core tokio runtime"))
}

/// Current wall-clock seconds — the registration proof binds to it, and the host
/// checks it against its own clock within a bounded window.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
