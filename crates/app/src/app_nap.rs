//! macOS App Nap suppression as a scoped RAII guard.
//!
//! App Nap throttles an app the system thinks is idle, and the RunningBoard
//! assertion macOS acquires on our behalf can wedge the main thread inside a
//! `_dispatch_async_and_wait` — which freezes the GPUI foreground executor.
//! The danger window is whenever the main thread is doing a synchronous
//! daemon round-trip (session restore, spawn, attach, list): a napped app
//! parked in `block_on` is exactly the state that deadlocks.
//!
//! Rather than disable App Nap for the whole process, we hold a user-initiated
//! `NSProcessInfo` activity only for the duration of those operations and
//! release it on drop. The guard is scoped to the latency-sensitive daemon
//! round-trips, so when the app is genuinely idle it can still nap normally.
//!
//! Usage: `let _nap = app_nap::prevent("relay spawn");` before a blocking
//! relay call; the guard lifts when it leaves scope.

#[cfg(target_os = "macos")]
mod imp {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CString;

    // NSActivityUserInitiatedAllowingIdleSystemSleep: opt out of App Nap and
    // sudden/automatic termination, but still let the display and system
    // idle-sleep normally. Equivalent to
    // `NSActivityUserInitiated & ~NSActivityIdleSystemSleepDisabled`.
    const NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP: u64 = 0x00EF_FFFF;

    /// Holds an `NSProcessInfo` activity for its lifetime; ends it on drop.
    pub struct PreventAppNapGuard {
        activity: *mut Object,
    }

    // The activity token returned by NSProcessInfo is documented as safe to
    // pass across threads, and the guard only ever calls `endActivity:` on it.
    unsafe impl Send for PreventAppNapGuard {}

    impl PreventAppNapGuard {
        pub fn new(reason: &str) -> Self {
            unsafe {
                let c_reason = CString::new(reason)
                    .unwrap_or_else(|_| CString::new("relay request").expect("static fallback"));
                let process_info: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
                let ns_reason: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: c_reason.as_ptr()];
                let activity: *mut Object = msg_send![
                    process_info,
                    beginActivityWithOptions: NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP
                    reason: ns_reason
                ];
                // Retain so an autorelease-pool drain can't free it before drop.
                let activity: *mut Object = msg_send![activity, retain];
                Self { activity }
            }
        }
    }

    impl Drop for PreventAppNapGuard {
        fn drop(&mut self) {
            unsafe {
                // `processInfo` is a never-deallocated singleton; fetch it fresh.
                let process_info: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
                let _: () = msg_send![process_info, endActivity: self.activity];
                let _: () = msg_send![self.activity, release];
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::PreventAppNapGuard;

/// Acquire an App-Nap-suppressing activity for the duration of the returned
/// guard. `None` on non-macOS (and the guard is a no-op there).
#[cfg(target_os = "macos")]
pub fn prevent(reason: &str) -> Option<PreventAppNapGuard> {
    Some(PreventAppNapGuard::new(reason))
}

/// No-op guard type on platforms without App Nap.
#[cfg(not(target_os = "macos"))]
pub struct PreventAppNapGuard;

#[cfg(not(target_os = "macos"))]
pub fn prevent(_reason: &str) -> Option<PreventAppNapGuard> {
    None
}
