//! Platform glue — macOS/OS-level integration and window lifecycle.
//!
//! `app_nap` (App-Nap suppression), `single_instance` (flock guard),
//! `window_factory` + `window_registry` (window creation/tracking), and
//! `menu` (native menu bar). Grouped for traversal; re-exported at the crate
//! root so existing `crate::app_nap::…` paths keep resolving.

/// Serialises every test that touches the machine's *global* input state.
///
/// Two of them do: `escape_tap` arms a real event tap and posts a real key,
/// and `secure_input` really takes a secure-input hold. Run at the same time
/// they break each other — a hold taken mid-pump starves the tap's key, and the
/// tap test then fails saying the tap is broken about a machine where another
/// test was muzzling it. One lock across both modules, because per-module locks
/// are exactly what let that through.
#[cfg(test)]
pub(crate) static INPUT_STATE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`INPUT_STATE_SERIAL`], ignoring poisoning — a panicking test has
/// already failed, and refusing to run every later one adds noise, not signal.
#[cfg(test)]
pub(crate) fn serialize_input_state() -> std::sync::MutexGuard<'static, ()> {
    INPUT_STATE_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

pub mod app_nap;
pub mod claude_session_env;
// Unconditional rather than `#[cfg(windows)]`: the entry point calls it on
// every platform and the function is the no-op, so the one Windows quirk
// stays inside the module that explains it instead of spreading a cfg into
// `main`.
pub mod direct_composition;
pub mod escape_tap;
// Every line assumes POSIX: `:`-separated PATH, a `-lc` login shell, launchd's
// four-directory stub. Windows inherits a real PATH from the registry, so
// there is nothing here to port.
#[cfg(unix)]
pub mod login_path;
pub mod menu;
pub mod mic_permission;
// Waits out the old pid in a detached `sh`, then `open -n`s the bundle. Paired
// with the macOS updater's quit-time swap, and macOS-shaped throughout.
#[cfg(target_os = "macos")]
pub mod relaunch;
#[cfg(any(target_os = "macos", windows))]
pub mod screen_control_indicator;
pub mod secure_input;
pub mod single_instance;
pub mod window_factory;
pub mod window_registry;
