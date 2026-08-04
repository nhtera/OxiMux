//! Single-instance guard for the GUI process.
//!
//! Two OxiMux GUI processes pointed at the same on-disk data directory race on
//! the per-window layout store (`terminal_tabs:*`, `pane_relay_ids`,
//! `open_windows`) and both attach the same relay PTYs, so the second launch
//! silently clobbers the first's persisted session — a terminal saved by one
//! instance vanishes when the other overwrites the snapshot. This guard makes a
//! second launch a no-op: it takes an exclusive advisory lock on a file in the
//! data directory and, if a live instance already holds it, brings that
//! instance to the foreground and bows out instead of booting a second racing
//! window set.
//!
//! The lock mechanics live in [`oximux_single_instance`], shared with the
//! headless host's own role locks; what stays here is the GUI-only half —
//! which file names the GUI role, and how a contender raises the holder's
//! window on each platform. The companion only short-circuits the GUI boot
//! path; the `notify` / `agent-status` helper CLIs and the dev spikes run
//! before this check, so they never contend with a live window.

use std::path::{Path, PathBuf};

pub use oximux_single_instance::{AcquireOutcome, SingleInstanceGuard, try_acquire};

/// Lock file name under the app data directory. Sits alongside the relay
/// `relay-v8.{sock,pid,token}` files — same placement convention.
pub const LOCK_FILENAME: &str = "oximux-gui.lock";

/// Resolve the GUI lock path inside a given data directory.
pub fn lock_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILENAME)
}

/// Bring the already-running instance to the foreground so the user sees their
/// existing window instead of a launch that appears to do nothing. Best-effort:
/// a no-op if the PID is gone or not a GUI app.
#[cfg(target_os = "macos")]
pub fn activate_existing_instance(pid: u32) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: `NSRunningApplication` is part of AppKit, already linked into the
    // process. `runningApplicationWithProcessIdentifier:` returns a (possibly
    // nil) autoreleased instance; we only message it when non-nil.
    unsafe {
        let app: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid as i32
        ];
        if app.is_null() {
            return;
        }
        // NSApplicationActivateAllWindows | NSApplicationActivateIgnoringOtherApps:
        // raise every window of the holder and steal focus from the launcher.
        let options: u64 = (1 << 0) | (1 << 1);
        let _: bool = msg_send![app, activateWithOptions: options];
    }
}

/// Name of the broadcast message a second launch uses to ask the live instance
/// to come forward. `RegisterWindowMessage` maps this string to the same
/// message id for every process on the desktop, which is what lets two
/// unrelated instances agree on one without sharing anything else.
#[cfg(windows)]
pub const ACTIVATE_MESSAGE: &str = "OxiMuxActivate";

/// Ask the running instance to raise its windows.
///
/// Deliberately not a socket, a pipe, or any other channel: the only capability
/// being conveyed is "come to the front", and a broadcast window message can
/// carry nothing else. Anything richer would be new attack surface — reachable
/// by every process on the desktop — in exchange for a feature nobody asked
/// for.
///
/// Windows normally refuses focus changes from a process the user is not
/// interacting with. `AllowSetForegroundWindow` is how the launching process
/// hands its own (just-granted, since the user launched us) foreground right to
/// the holder, so the holder's own `SetForegroundWindow` is honoured rather
/// than flashing its taskbar button.
#[cfg(windows)]
pub fn activate_existing_instance(pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, HWND_BROADCAST, PostMessageW, RegisterWindowMessageW,
    };

    let Ok(name) = widestring::U16CString::from_str(ACTIVATE_MESSAGE) else {
        return;
    };
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let msg = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if msg == 0 {
        return;
    }

    // Best-effort, and failure is survivable: the worst case is the holder
    // blinks in the taskbar instead of coming forward.
    // SAFETY: plain Win32 calls with no pointer arguments.
    unsafe {
        AllowSetForegroundWindow(pid);
        // Broadcast rather than target a window: the holder's HWND is not
        // knowable from here, and every process filters on the registered id,
        // which no other application has registered.
        PostMessageW(HWND_BROADCAST, msg, 0, 0);
    }
}

/// No-op activation on platforms with neither AppKit nor Win32.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn activate_existing_instance(_pid: u32) {}
