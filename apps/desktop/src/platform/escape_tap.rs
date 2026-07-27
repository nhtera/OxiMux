//! A global Escape key that stops an agent driving the screen — and does not
//! reach the app being driven.
//!
//! # Why a `CGEventTap` and not a global monitor
//!
//! `NSEvent addGlobalMonitorForEventsMatchingMask:` is observe-only by Apple's
//! design; it physically cannot swallow an event. Built on that, Escape would
//! fire the abort **and still reach the frontmost, agent-controlled app** —
//! leaving open the exact hole this closes: an agent that puts a dialog on
//! screen would have the user's panic key dismissing its own dialog.
//!
//! A `CGEventTap` created with `kCGEventTapOptionDefault` is *active*: the
//! callback's return value decides whether the event continues. Returning null
//! consumes it.
//!
//! # Live only while an agent is driving
//!
//! The tap swallows **every** Escape on the machine while it is armed, so it is
//! armed only for the duration of a screen-control session and torn down the
//! moment the last one ends. A tap left armed would break Escape in every app
//! the user owns, including dismissing an input method's candidate window.
//! [`arm`] returning a guard rather than installing something permanent is what
//! makes that hard to get wrong.
//!
//! # Two ways it can fail, both reported rather than hidden
//!
//! Creating an active tap needs Accessibility permission. Without it
//! `CGEventTapCreate` returns null, [`arm`] fails, and the caller must say so —
//! a UI claiming Escape will stop an agent when it will not is worse than one
//! that admits it cannot.
//!
//! macOS also *disables* a tap whose callback is too slow
//! (`kCGEventTapDisabledByTimeout`). That arrives as a callback of its own and
//! must be answered by re-enabling, or the tap goes quiet and stays quiet. It
//! is the classic way this API fails in the field, which is why the callback
//! does nothing but set a flag and post a wake-up.
//!
//! # A third way, which this module cannot currently detect
//!
//! While any process holds **secure event input** — a password field, the lock
//! screen, some terminals — macOS delivers no keyboard events to any tap at
//! all. That is the feature working as designed; it is what stops a keylogger.
//!
//! What makes it dangerous here is that it is invisible from this side:
//! `CGEventTapCreate` still succeeds, [`arm`] still returns a guard, and
//! `CGEventTapIsEnabled` still reports true. Escape simply never arrives. So a
//! successful [`arm`] is **not** proof that Escape can stop an agent, and any
//! UI built on that assumption is making the promise this module's whole
//! premise says must not be made.
//!
//! Detecting it means reading `kCGSSessionSecureInputPID` out of IOKit's
//! `IOConsoleUsers`; a non-zero pid there means no tap on the machine is
//! receiving keys. Until that lands, the gap is real and known:
//!
//! ```text
//! ioreg -l -d 1 -k IOConsoleUsers | grep kCGSSessionSecureInputPID
//! ```

/// Serialises the tests that arm a real tap.
///
/// The abort flag and the live-port slot are process-wide, so two tests holding
/// taps at once would read each other's state: the live-key test's Escape would
/// land in the other's assertion that nothing is pending. That flake would look
/// exactly like a bug in the tap.
#[cfg(test)]
static TAP_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// `kCGEventKeyDown`.
    const CG_EVENT_KEY_DOWN: u32 = 10;
    /// `kCGEventTapDisabledByTimeout` — the tap was too slow and macOS switched
    /// it off. Answered by re-enabling; ignoring it silently loses the key.
    const CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    /// `kCGEventTapDisabledByUserInput`.
    const CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    /// `kCGKeyboardEventKeycode`.
    const KEYCODE_FIELD: u32 = 9;
    /// Virtual key code for Escape. A hardware code, so it is the same whatever
    /// layout or input method is active — which matters, because the point is to
    /// work when the user is panicking.
    const ESCAPE_KEYCODE: i64 = 53;

    /// `kCGSessionEventTap`: this login session, ahead of the apps in it.
    const SESSION_EVENT_TAP: u32 = 1;
    /// `kCGHeadInsertEventTap`: ahead of taps installed later.
    const HEAD_INSERT: u32 = 0;
    /// `kCGEventTapOptionDefault`: *active*, so the callback's return value can
    /// consume the event. The listen-only option cannot, and choosing it would
    /// silently reproduce the global-monitor bug this module exists to avoid.
    const TAP_OPTION_DEFAULT: u32 = 0;

    type CFMachPortRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CGEventRef = *mut c_void;
    type CGEventTapProxy = *mut c_void;

    type CGEventTapCallBack = extern "C" fn(
        proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: CFStringRef;
        fn CFMachPortCreateRunLoopSource(
            allocator: CFAllocatorRef,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopGetMain() -> CFRunLoopRef;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRelease(cf: *const c_void);
    }

    /// Set by the tap callback, drained by the owner.
    ///
    /// A flag rather than a callback invoked in place: the callback runs on the
    /// event-tap's own thread inside the window macOS measures for the timeout,
    /// so it must do as close to nothing as possible. Anything that took a lock
    /// held by the UI thread, or repainted, would be exactly how a tap gets
    /// disabled for being slow.
    static ABORT_REQUESTED: AtomicBool = AtomicBool::new(false);

    /// The live tap's port, so the timeout handler can re-enable it.
    ///
    /// A pointer behind a mutex rather than a plain static because the callback
    /// and the owner touch it from different threads. Only ever one tap: the
    /// abort is app-wide, not per chat.
    static LIVE_PORT: OnceLock<Mutex<usize>> = OnceLock::new();

    fn live_port() -> &'static Mutex<usize> {
        LIVE_PORT.get_or_init(|| Mutex::new(0))
    }

    /// What the tap should do with an event.
    ///
    /// Split out from the `extern "C"` callback so the branch that matters can
    /// be tested. Returning "pass" for Escape would silently reproduce the
    /// global-monitor bug — the abort would fire *and* the key would reach the
    /// app an agent is driving — and that is invisible to inspection.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Handling {
        /// Let the event through untouched.
        Pass,
        /// Escape: record the abort and swallow the key.
        AbortAndConsume,
        /// macOS switched the tap off; turn it back on and pass the event.
        Reenable,
    }

    pub(super) fn handling(event_type: u32, keycode: impl FnOnce() -> i64) -> Handling {
        if event_type == CG_EVENT_TAP_DISABLED_BY_TIMEOUT
            || event_type == CG_EVENT_TAP_DISABLED_BY_USER_INPUT
        {
            return Handling::Reenable;
        }
        if event_type != CG_EVENT_KEY_DOWN {
            return Handling::Pass;
        }
        if keycode() == ESCAPE_KEYCODE {
            Handling::AbortAndConsume
        } else {
            Handling::Pass
        }
    }

    extern "C" fn on_event(
        _proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        _user_info: *mut c_void,
    ) -> CGEventRef {
        // SAFETY: a key-down event is guaranteed to carry a keycode field, and
        // the closure is only called for one.
        match handling(event_type, || unsafe {
            CGEventGetIntegerValueField(event, KEYCODE_FIELD)
        }) {
            Handling::Pass => event,
            // Turning it back on is the entire correct response; passing the
            // event keeps the user's input flowing in the meantime.
            Handling::Reenable => {
                let port = *live_port().lock().expect("escape tap port poisoned") as CFMachPortRef;
                if !port.is_null() {
                    // SAFETY: `port` is the CFMachPort this callback belongs
                    // to, still retained by the armed guard that installed it.
                    unsafe { CGEventTapEnable(port, true) };
                }
                event
            }
            Handling::AbortAndConsume => {
                ABORT_REQUESTED.store(true, Ordering::SeqCst);
                // Null consumes it. This is the whole point: the app an agent
                // is driving must not see the key meant to stop the agent.
                std::ptr::null_mut()
            }
        }
    }

    /// The option the tap is created with. A function so `arm` and the test
    /// that pins it read the same thing — a test asserting against its own copy
    /// of the constant would pass while `arm` passed something else.
    const fn tap_option() -> u32 {
        TAP_OPTION_DEFAULT
    }

    /// A live Escape tap. Dropping it restores ordinary Escape everywhere.
    pub struct EscapeTap {
        port: CFMachPortRef,
        source: CFRunLoopSourceRef,
        /// The loop the source was added to, so `Drop` removes it from the same
        /// one it joined.
        run_loop: CFRunLoopRef,
    }

    // SAFETY: the two pointers are CoreFoundation objects this type owns a
    // reference to. They are only dereferenced through the CF calls below,
    // which are thread-safe, and `Drop` runs exactly once.
    unsafe impl Send for EscapeTap {}

    impl EscapeTap {
        /// Has Escape been pressed since the last check? Clears the flag, so a
        /// single press aborts once.
        pub fn abort_requested(&self) -> bool {
            ABORT_REQUESTED.swap(false, Ordering::SeqCst)
        }
    }

    impl Drop for EscapeTap {
        fn drop(&mut self) {
            // SAFETY: both pointers were created in `arm` and are still owned by
            // this guard; each CF object is released exactly once.
            unsafe {
                CGEventTapEnable(self.port, false);
                CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopCommonModes);
                CFRelease(self.source);
                CFRelease(self.port);
            }
            *live_port().lock().expect("escape tap port poisoned") = 0;
            // A press that landed between the last check and teardown is not an
            // abort for the *next* session.
            ABORT_REQUESTED.store(false, Ordering::SeqCst);
        }
    }

    /// Install the tap. `Err` means Escape cannot be intercepted — almost always
    /// because OxiMux has no Accessibility permission.
    ///
    /// Must be called from the main thread: the run-loop source is added to the
    /// main run loop, which is where GPUI's event loop lives.
    pub fn arm() -> Result<EscapeTap, super::EscapeTapError> {
        // SAFETY: returns the process's main run loop, which is where GPUI's
        // event loop lives and therefore the only one guaranteed to be running.
        arm_on(unsafe { CFRunLoopGetMain() })
    }

    /// [`arm`], against a caller-chosen run loop.
    ///
    /// Exists so a test can attach the tap to a loop it controls and pump it —
    /// a `cargo test` binary never runs the main run loop, so a tap installed
    /// there would be armed but permanently silent, and every assertion about
    /// what it does with a real key press would be vacuous.
    fn arm_on(run_loop: CFRunLoopRef) -> Result<EscapeTap, super::EscapeTapError> {
        ABORT_REQUESTED.store(false, Ordering::SeqCst);
        // SAFETY: a null `user_info` is valid — the callback ignores it and
        // reads process statics instead, because an `extern "C"` fn cannot
        // capture and a raw pointer into a moved guard would dangle.
        let port = unsafe {
            CGEventTapCreate(
                SESSION_EVENT_TAP,
                HEAD_INSERT,
                tap_option(),
                1u64 << CG_EVENT_KEY_DOWN,
                on_event,
                std::ptr::null_mut(),
            )
        };
        if port.is_null() {
            return Err(super::EscapeTapError::NotPermitted);
        }

        // SAFETY: `port` is a live CFMachPort from the call above.
        let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0) };
        if source.is_null() {
            // SAFETY: releasing the port we just created and are abandoning.
            unsafe { CFRelease(port) };
            return Err(super::EscapeTapError::NoRunLoopSource);
        }

        // SAFETY: adding a live source to a live run loop in the common modes.
        unsafe {
            CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
        }
        *live_port().lock().expect("escape tap port poisoned") = port as usize;
        Ok(EscapeTap {
            port,
            source,
            run_loop,
        })
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        use crate::platform::escape_tap::TAP_SERIAL;

        /// `kCGHIDEventTap` — below the session tap, so an event posted here is
        /// seen by one exactly as the user's own key press would be. (Posting
        /// at the session level instead lands *above* the tap and slips past
        /// it, which is why AppleScript's `key code` cannot exercise this.)
        const HID_EVENT_TAP: u32 = 0;
        /// `kCGEventSourceStateHIDSystemState`.
        const HID_SYSTEM_STATE: i32 = 1;

        type CGEventSourceRef = *mut c_void;

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
            fn CGEventCreateKeyboardEvent(
                source: CGEventSourceRef,
                keycode: u16,
                key_down: bool,
            ) -> CGEventRef;
            fn CGEventPost(tap: u32, event: CGEventRef);
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            static kCFRunLoopDefaultMode: CFStringRef;
            fn CFRunLoopGetCurrent() -> CFRunLoopRef;
            fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: bool)
                -> i32;
        }

        fn post_escape() {
            // SAFETY: a source and two events created here, posted, and released
            // exactly once each.
            unsafe {
                let source = CGEventSourceCreate(HID_SYSTEM_STATE);
                for down in [true, false] {
                    let event =
                        CGEventCreateKeyboardEvent(source, ESCAPE_KEYCODE as u16, down);
                    if !event.is_null() {
                        CGEventPost(HID_EVENT_TAP, event);
                        CFRelease(event);
                    }
                }
                if !source.is_null() {
                    CFRelease(source);
                }
            }
        }

        /// The claim the module makes, exercised against a real key press
        /// rather than against [`handling`] alone: an armed tap sees Escape and
        /// records the abort.
        ///
        /// Attached to this thread's run loop and pumped here, because a
        /// `cargo test` binary never runs the main one — a tap installed there
        /// would be armed and permanently silent, and this assertion would pass
        /// only by never having been tested.
        ///
        /// # Why it is ignored by default
        ///
        /// It needs **secure input to be off**, and that is not something a test
        /// can arrange. While any process holds secure event input, macOS
        /// delivers no keyboard events to any tap — by design, since that is
        /// what stops keyloggers — yet `CGEventTapCreate` still succeeds and the
        /// tap still reports itself enabled. Check with:
        ///
        /// ```text
        /// ioreg -l -d 1 -k IOConsoleUsers | grep kCGSSessionSecureInputPID
        /// ```
        ///
        /// A non-zero pid there means this test cannot pass, and neither can the
        /// feature — see the module docs. Run it with `--ignored` when that
        /// reads zero.
        #[test]
        #[ignore = "needs secure input off; see the doc comment"]
        fn a_real_escape_press_reaches_an_armed_tap() {
            let _serial = TAP_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: this thread's run loop, which the pump below drives.
            let Ok(tap) = arm_on(unsafe { CFRunLoopGetCurrent() }) else {
                return;
            };
            post_escape();
            // The callback runs on the run loop, so it has to be given a turn.
            // Bounded: a tap that never fires must fail the assertion, not hang.
            // SAFETY: pumping the current thread's run loop.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, false) };
            assert!(
                tap.abort_requested(),
                "an armed tap must see a real Escape press"
            );
            // And one press aborts once — a second read must come back empty,
            // or a single panic key would keep stopping later sessions.
            assert!(!tap.abort_requested());
        }

        /// Escape must be consumed, not merely observed. This is the assertion
        /// that stands in for the whole module's reason to exist.
        #[test]
        fn escape_aborts_and_is_swallowed() {
            assert_eq!(
                handling(CG_EVENT_KEY_DOWN, || ESCAPE_KEYCODE),
                Handling::AbortAndConsume
            );
        }

        #[test]
        fn every_other_key_passes_straight_through() {
            // Over-consuming would make the machine unusable while an agent
            // runs — the failure mode opposite to the one above, and just as
            // bad.
            for keycode in [0, 36, 49, 53 + 1, 126] {
                assert_eq!(
                    handling(CG_EVENT_KEY_DOWN, || keycode),
                    Handling::Pass,
                    "keycode {keycode}"
                );
            }
        }

        #[test]
        fn a_disabled_tap_is_re_enabled_rather_than_left_dead() {
            // The classic field failure: macOS switches off a slow tap, the
            // callback ignores the notice, and Escape stops working with no
            // sign that anything is wrong.
            for event_type in [
                CG_EVENT_TAP_DISABLED_BY_TIMEOUT,
                CG_EVENT_TAP_DISABLED_BY_USER_INPUT,
            ] {
                assert_eq!(
                    handling(event_type, || panic!("must not read a keycode")),
                    Handling::Reenable
                );
            }
        }

        #[test]
        fn a_non_key_event_never_reads_a_keycode() {
            // Reading the keycode field off, say, a mouse event is meaningless;
            // the closure exists so it is never evaluated for one.
            assert_eq!(
                handling(1 /* kCGEventLeftMouseDown */, || panic!("must not read")),
                Handling::Pass
            );
        }

        #[test]
        fn the_tap_is_created_active_so_it_can_consume() {
            // `kCGEventTapOptionListenOnly` (1) cannot swallow anything.
            // Switching to it would compile, run, fire the abort, and quietly
            // let Escape through to the agent-controlled app.
            assert_eq!(tap_option(), 0, "the tap must be active, not listen-only");
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// Stand-in so the rest of the app compiles off macOS. Screen control is
    /// macOS-only, so nothing ever arms this.
    pub struct EscapeTap;

    impl EscapeTap {
        pub fn abort_requested(&self) -> bool {
            false
        }
    }

    pub fn arm() -> Result<EscapeTap, super::EscapeTapError> {
        Err(super::EscapeTapError::Unsupported)
    }
}

pub use imp::{EscapeTap, arm};

/// Why Escape could not be intercepted.
///
/// Distinguished rather than collapsed because the UI has to say something
/// true: "grant Accessibility" is actionable, "not supported here" is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EscapeTapError {
    #[error(
        "OxiMux needs Accessibility permission to stop an agent with Escape — grant it in System Settings › Privacy & Security › Accessibility"
    )]
    NotPermitted,
    #[error("could not attach the Escape tap to the run loop")]
    NoRunLoopSource,
    #[error("stopping an agent with Escape is only available on macOS")]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_says_what_the_user_can_do_about_it() {
        // The permission case is the one that actually happens, and it is
        // useless unless it names the switch to flip.
        let denied = EscapeTapError::NotPermitted.to_string();
        assert!(denied.contains("Accessibility"), "{denied}");
        assert!(denied.contains("System Settings"), "{denied}");
        for err in [
            EscapeTapError::NotPermitted,
            EscapeTapError::NoRunLoopSource,
            EscapeTapError::Unsupported,
        ] {
            assert!(!err.to_string().is_empty(), "{err:?}");
        }
    }

    /// Arming needs Accessibility permission, which CI does not have and a
    /// developer machine may not either — so this asserts the *contract* holds
    /// either way rather than that the tap succeeds.
    ///
    /// It is a real check despite that: a null-pointer bug in `arm`, a
    /// mis-declared FFI signature, or a double-release in `Drop` would crash
    /// here rather than in front of a user with an agent mid-drive.
    #[test]
    fn arming_either_works_cleanly_or_fails_cleanly() {
        let _serial = TAP_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        match arm() {
            Ok(tap) => {
                // Nothing has been pressed, so nothing is pending.
                assert!(!tap.abort_requested());
                drop(tap);
                // And a second arm after a clean teardown must still work —
                // the case a leaked run-loop source would break.
                let again = arm().expect("re-arming after teardown");
                assert!(!again.abort_requested());
            }
            Err(err) => {
                assert!(matches!(
                    err,
                    EscapeTapError::NotPermitted
                        | EscapeTapError::NoRunLoopSource
                        | EscapeTapError::Unsupported
                ));
            }
        }
    }
}
