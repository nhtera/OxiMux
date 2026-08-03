//! Whether macOS is currently withholding keyboard events from every event tap.
//!
//! While any process holds **secure event input** — a password field, the lock
//! screen, some terminals — the window server stops delivering key events to
//! taps. That is the feature working correctly: it is what stops a keylogger
//! reading a password out of another app.
//!
//! # Why this has to be asked rather than inferred
//!
//! It is invisible from the tap's own side. `CGEventTapCreate` succeeds,
//! `CGEventTapEnable` succeeds, and `CGEventTapIsEnabled` reports true. The
//! keys simply never arrive. So a screen-control kill switch built on a tap
//! cannot tell from the tap whether it works, and "Press Esc to stop" is a
//! promise it would go on making while being unable to keep it.
//!
//! The state lives in the IORegistry, on the `IOConsoleUsers` property: a
//! non-zero `kCGSSessionSecureInputPID` names the process holding it. The same
//! answer by hand:
//!
//! ```text
//! ioreg -l -d 1 -k IOConsoleUsers | grep kCGSSessionSecureInputPID
//! ```

/// Is anything holding secure input right now?
///
/// The question callers actually have. Answering it costs an IORegistry search,
/// which measures well under a millisecond — cheap enough to ask on every tick,
/// which it has to be, because a password field can take focus at any moment.
pub fn active() -> bool {
    holder_pid().is_some()
}

/// The pid recorded as holding secure input, or `None` when nothing is.
///
/// # The pid is an attribution, not the caller
///
/// Measured, not assumed: a process that takes a hold via
/// `EnableSecureEventInput` is recorded here under a *different* pid — its
/// responsible parent. So this identifies roughly where a hold came from and is
/// useful in a log line; it is not an identity to compare against.
/// [`active`] is what decisions should be made on.
///
/// # Why failure reads as "nothing holds it"
///
/// An unreadable registry also produces `None`. That direction is deliberate
/// but not free: it means a failure here reads as "the kill switch works",
/// which is the optimistic answer. The alternative — declaring the kill switch
/// broken because a registry read failed — would train the user to ignore the
/// warning, and the warning is the whole point.
#[cfg(target_os = "macos")]
pub fn holder_pid() -> Option<u32> {
    imp::holder_pid()
}

/// `None` everywhere else — but on Windows that means **unknown**, not clear.
///
/// It used to be true that there was nothing to withhold from off macOS. Once
/// [`crate::platform::escape_tap`] grew a `WH_KEYBOARD_LL` implementation it
/// stopped being true: a low-level hook can be starved of the very key it
/// exists to catch, by the secure desktop (UAC consent, Ctrl+Alt+Del, the lock
/// screen) and by UIPI when the foreground window outranks OxiMux.
///
/// What Windows does not offer is a way to *ask*. There is no `IOConsoleUsers`
/// equivalent to read, so this cannot distinguish "nothing is withholding keys"
/// from "something is and we cannot see it".
///
/// The `None` is therefore load-bearing in the wrong direction, and the
/// asymmetry with macOS is worth stating plainly: there, a `None` is a real
/// answer that is occasionally wrong after a failed read; here it is not an
/// answer at all. Callers that render "Press Esc to stop an agent" must not
/// treat this as confirmation on Windows — the honest string is that Escape
/// *should* stop it, not that it will.
///
/// Open decision #6 in `plans/260801-0157-windows-computer-use/` — the UI half
/// belongs with the settings pane in Phase 7. This is the note that keeps the
/// next reader from concluding the question was already settled.
#[cfg(not(target_os = "macos"))]
pub fn holder_pid() -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

    /// `io_registry_entry_t`, which is a `mach_port_t`.
    type IoRegistryEntry = u32;

    /// `kIOServicePlane`, the plane the console-users property hangs off.
    const SERVICE_PLANE: &[u8] = b"IOService\0";
    /// `kIORegistryIterateRecursively` — the property is not on the root itself.
    const ITERATE_RECURSIVELY: u32 = 1;
    /// `kIOMainPortDefault`. Documented as accepting `MACH_PORT_NULL` for the
    /// default port, which avoids linking a symbol that was renamed from
    /// `kIOMasterPortDefault` and would tie us to an SDK version.
    const MAIN_PORT_DEFAULT: IoRegistryEntry = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegistryGetRootEntry(main_port: IoRegistryEntry) -> IoRegistryEntry;
        fn IORegistryEntrySearchCFProperty(
            entry: IoRegistryEntry,
            plane: *const c_char,
            key: *const c_void,
            allocator: *const c_void,
            options: u32,
        ) -> *const c_void;
        fn IOObjectRelease(object: IoRegistryEntry) -> i32;
    }

    pub(super) fn holder_pid() -> Option<u32> {
        // SAFETY: the root entry is released below; a zero handle simply yields
        // a null property and no iteration.
        let root = unsafe { IORegistryGetRootEntry(MAIN_PORT_DEFAULT) };
        if root == 0 {
            return None;
        }
        // NSString is toll-free bridged to CFString, so the retained pointer
        // doubles as the CFStringRef key for the duration of the call.
        let key = NSString::from_str("IOConsoleUsers");
        // SAFETY: `root` is live for this call and `key` outlives it. The
        // returned property is owned by us (a "Create" rule call), which is
        // what `from_raw` below then takes responsibility for.
        let property = unsafe {
            IORegistryEntrySearchCFProperty(
                root,
                SERVICE_PLANE.as_ptr() as *const c_char,
                Retained::as_ptr(&key) as *const c_void,
                std::ptr::null(),
                ITERATE_RECURSIVELY,
            )
        };
        // SAFETY: releasing the entry we obtained above, exactly once.
        unsafe { IOObjectRelease(root) };

        if property.is_null() {
            return None;
        }
        // SAFETY: `IOConsoleUsers` is a CFArray of CFDictionary, both toll-free
        // bridged, and the pointer carries the +1 this now owns.
        let sessions: Retained<NSArray<NSDictionary<NSString, AnyObject>>> =
            unsafe { Retained::from_raw(property as *mut _)? };

        sessions.iter().find_map(|session| pid_in(&session))
    }

    /// The secure-input pid recorded on one console session, if it holds one.
    ///
    /// Zero means "recorded, but nobody holds it" — the key is present on an
    /// ordinary session too, so treating its mere presence as a yes would report
    /// secure input permanently on.
    fn pid_in(session: &NSDictionary<NSString, AnyObject>) -> Option<u32> {
        let value = session.objectForKey(&NSString::from_str("kCGSSessionSecureInputPID"))?;
        let number = value.downcast_ref::<NSNumber>()?;
        let pid = number.as_i32();
        (pid > 0).then_some(pid as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::serialize_input_state;

    // The public Carbon calls that hold and release secure input. Using the
    // real mechanism is the point: a test that faked the state would prove the
    // fake readable, not the registry.
    #[cfg(target_os = "macos")]
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn EnableSecureEventInput() -> i32;
        fn DisableSecureEventInput() -> i32;
    }

    /// Holds secure input for as long as it lives. A guard rather than a pair
    /// of calls because a panic between them would leave the machine with
    /// secure input stuck on until this process exited.
    #[cfg(target_os = "macos")]
    struct SecureInputHeld;

    #[cfg(target_os = "macos")]
    impl SecureInputHeld {
        fn take() -> Option<Self> {
            // SAFETY: a thin Carbon call taking no arguments; the guard's Drop
            // releases exactly what this acquired.
            (unsafe { EnableSecureEventInput() } == 0).then_some(Self)
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for SecureInputHeld {
        fn drop(&mut self) {
            // SAFETY: releasing the hold taken in `take`, exactly once.
            unsafe { DisableSecureEventInput() };
        }
    }

    /// The claim that matters, exercised against the real mechanism: a genuine
    /// hold is seen, and releasing it is seen too.
    ///
    /// Without this the whole module could return `None` unconditionally and
    /// every other test here would still pass — precisely the shape of a safety
    /// check that silently never fires.
    ///
    /// It asserts *that* a hold is visible, not *who* holds it, because the pid
    /// is not the caller: this process took the hold and the registry named a
    /// different one (its responsible parent). Which is fine for what this is
    /// for — the question is only ever "is anything withholding keys".
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_hold_is_seen_and_so_is_letting_go() {
        let _serial = serialize_input_state();
        let before = active();
        let Some(held) = SecureInputHeld::take() else {
            // Refused — it is only granted to an active app in some contexts.
            // Skipping beats asserting something this cannot arrange.
            return;
        };
        assert!(active(), "a real hold must be visible");
        drop(held);
        assert_eq!(
            active(),
            before,
            "releasing must return the machine to how it was found"
        );
    }

    /// Reads the real registry, so it asserts the contract rather than an
    /// answer: whichever way this machine happens to be right now, the call
    /// must be cheap, safe, and self-consistent.
    ///
    /// It is a real check despite that — a mis-declared FFI signature, a
    /// wrong ownership rule, or a bad downcast would crash here rather than in
    /// front of a user with an agent mid-drive.
    #[test]
    fn asking_is_safe_and_repeatable() {
        let _serial = serialize_input_state();
        let first = holder_pid();
        let second = holder_pid();
        assert_eq!(
            first, second,
            "two reads a moment apart must agree; a differing answer means the \
             property is being misread rather than that secure input toggled"
        );
        if let Some(pid) = first {
            assert!(pid > 0, "zero means nobody holds it and must read as None");
        }
    }

    /// The read happens once per second while an agent can drive, so a slow
    /// registry walk would be a per-second cost on the user's machine.
    #[test]
    fn asking_is_cheap_enough_to_poll() {
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let _ = holder_pid();
        }
        let each = start.elapsed() / 10;
        assert!(
            each < std::time::Duration::from_millis(50),
            "a registry read took {each:?}, too slow to run on every tick"
        );
    }
}
