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
//! Creating this tap needs **Input Monitoring**, which is a different switch
//! from Accessibility and the one usually missing: macOS gates keyboard taps on
//! it, so a build that already holds Accessibility still fails here. Without it
//! `CGEventTapCreate` returns null, [`arm`] fails, and the caller must say so —
//! a UI claiming Escape will stop an agent when it will not is worse than one
//! that admits it cannot.
//!
//! The null says nothing about *which* switch it wanted, so anything shown to
//! the user has to name both rather than pick one. Naming Accessibility alone
//! sends someone who has already granted it back to a pane that is correct,
//! where they will find nothing to change and conclude the feature is broken.
//!
//! macOS also *disables* a tap whose callback is too slow
//! (`kCGEventTapDisabledByTimeout`). That arrives as a callback of its own and
//! must be answered by re-enabling, or the tap goes quiet and stays quiet. It
//! is the classic way this API fails in the field, which is why the callback
//! does nothing but set a flag and post a wake-up.
//!
//! # A third way, which this module cannot see for itself
//!
//! While any process holds **secure event input** — a password field, the lock
//! screen, some terminals — macOS delivers no keyboard events to any tap at
//! all. That is the feature working as designed; it is what stops a keylogger.
//!
//! It is invisible from this side: `CGEventTapCreate` succeeds, [`arm`] returns
//! a guard, and `CGEventTapIsEnabled` reports true. Escape simply never
//! arrives. So **a successful [`arm`] is not proof that Escape can stop an
//! agent**, and a UI built on that assumption makes exactly the promise this
//! module's premise says must not be made.
//!
//! Nothing here can detect it, because the tap is not where the answer lives.
//! [`crate::platform::secure_input`] reads it out of the IORegistry, and
//! callers must ask *both* before claiming the kill switch works.
//!
//! # Windows: a different API with the same property, and the same caveat
//!
//! `docs/windows-port-exclusions.md` listed this feature as having no Windows
//! equivalent. That is wrong, and the reason matters: the requirement is not
//! "a global hotkey" but **consume, don't observe**, and Windows has exactly
//! one API that qualifies.
//!
//! A `WH_KEYBOARD_LL` hook installed with `SetWindowsHookExW` sees every key
//! before the focused application does, and returning non-zero *without*
//! calling `CallNextHookEx` swallows it. That is the same active/passive
//! distinction as `kCGEventTapOptionDefault` versus a global monitor, and it is
//! the whole reason this module exists rather than using `RegisterHotKey` —
//! which would also work, and would also let the key through.
//!
//! Two things are *better* than on macOS: no permission is required, so
//! [`EscapeTapError::NotPermitted`] cannot occur, and there is no
//! Input-Monitoring pane to send anyone to.
//!
//! Three constraints carry over almost unchanged:
//!
//! - **A message pump is required** on the installing thread. The system
//!   delivers low-level hook callbacks by posting to that thread's queue, so a
//!   hook armed on a thread that never pumps is a hook that never fires. GPUI's
//!   main thread pumps, which is where [`arm`] already had to be called.
//! - **A slow callback is silently unhooked.** Windows drops a low-level hook
//!   that exceeds `LowLevelHooksTimeout` (default 300 ms) — the direct analogue
//!   of `kCGEventTapDisabledByTimeout`, and worse, because there is no
//!   notification to answer. The same answer applies and matters more: the
//!   callback sets an atomic and returns, and does nothing else, ever.
//! - **A successful [`arm`] is still not proof Escape can stop an agent.** The
//!   causes differ but the conclusion does not. The hook does not run on the
//!   secure desktop (UAC consent, Ctrl+Alt+Del, the lock screen), and UIPI
//!   stops an unelevated OxiMux from intercepting keys headed for a
//!   higher-integrity foreground window. `SetWindowsHookExW` succeeds in both
//!   cases and Escape simply never arrives.
//!
//! That last point has no [`crate::platform::secure_input`] counterpart to
//! consult: there is no equivalent of reading `IOConsoleUsers` to find out. So
//! on Windows the honest answer to "does the kill switch work right now" is
//! *cannot confirm*, and any UI must not upgrade that to *yes*.

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

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        /// Whether macOS considers *this process* Accessibility-trusted.
        ///
        /// The plain (non-`WithOptions`) form, so it answers without ever
        /// raising a prompt — this is called on a timer while an agent runs, and
        /// a dialog per tick would be its own bug.
        fn AXIsProcessTrusted() -> bool;
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
    /// because OxiMux has no Input Monitoring permission.
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
            // Which of the two permissions is missing is not recoverable from
            // the null itself, and the System Settings panes have been observed
            // to show a grant that the API does not honour. Asking macOS what it
            // believes about this process separates "the pane is lying" from
            // "the tap was refused for some other reason" — without it, both
            // look identical from here and cost an afternoon each.
            //
            // SAFETY: no arguments, and the non-prompting form, so this is safe
            // to call on any thread and cannot raise a dialog on a timer tick.
            let trusted = unsafe { AXIsProcessTrusted() };
            tracing::warn!(
                accessibility_trusted = trusted,
                "CGEventTapCreate returned null; the Escape tap cannot be installed"
            );
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

        use crate::platform::serialize_input_state;

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
        /// # Two preconditions it checks rather than assumes
        ///
        /// Input Monitoring permission, without which no tap exists; and **secure
        /// input off**, without which macOS delivers no keys to any tap while
        /// `CGEventTapCreate` still succeeds and the tap still reports itself
        /// enabled. Neither is a test's to arrange, so each is detected and
        /// skipped on — which is why the skip is a real branch here and not an
        /// `#[ignore]`: on a machine in the ordinary state, this runs.
        #[test]
        fn a_real_escape_press_reaches_an_armed_tap() {
            let _serial = serialize_input_state();
            if crate::platform::secure_input::active() {
                // Nothing would arrive, and the failure would say "the tap is
                // broken" about a machine where it is merely muzzled.
                return;
            }
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

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    /// `HC_ACTION` — the hook may process this event. Any other code means the
    /// callback must pass it along without inspecting it.
    const HC_ACTION: i32 = 0;
    /// `VK_ESCAPE`. A virtual-key code, so it is layout-independent — which
    /// matters, because the point is to work when the user is panicking.
    const VK_ESCAPE: u32 = 0x1B;

    /// Set by the hook callback, read and cleared by the owner.
    ///
    /// The only thing the callback touches. Windows silently unhooks a
    /// low-level hook that outruns `LowLevelHooksTimeout`, and unlike macOS
    /// there is no disabled-by-timeout notification to recover from — so the
    /// callback's budget is one atomic store.
    static ABORT_REQUESTED: AtomicBool = AtomicBool::new(false);

    /// What the hook should do with a key event.
    ///
    /// Split out from the `extern "system"` callback for the same reason as the
    /// macOS `Handling`: the branch that matters is the one that must *consume*
    /// rather than pass, and a mistake there is invisible to inspection —
    /// Escape would abort the agent and still reach the app it was driving.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Handling {
        /// Not ours: hand it to the next hook untouched.
        Pass,
        /// Escape going down: record the abort and swallow the key.
        AbortAndConsume,
    }

    pub(super) fn handling(code: i32, message: u32, vk_code: impl FnOnce() -> u32) -> Handling {
        if code != HC_ACTION {
            return Handling::Pass;
        }
        // Key *down* only. Consuming the matching key-up as well would be
        // tidier in theory and wrong in practice: an application that saw the
        // down and not the up can be left believing the key is still held.
        if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
            return Handling::Pass;
        }
        if vk_code() == VK_ESCAPE {
            Handling::AbortAndConsume
        } else {
            Handling::Pass
        }
    }

    unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: for `WH_KEYBOARD_LL` with `code == HC_ACTION`, Windows
        // guarantees `lparam` points to a `KBDLLHOOKSTRUCT` that outlives the
        // call. The closure is only invoked once `handling` has checked `code`.
        let verdict = handling(code, wparam as u32, || unsafe {
            (*(lparam as *const KBDLLHOOKSTRUCT)).vkCode
        });

        match verdict {
            Handling::AbortAndConsume => {
                ABORT_REQUESTED.store(true, Ordering::SeqCst);
                // Non-zero *and* not calling `CallNextHookEx` is what swallows
                // the key. Either one alone leaks it: returning zero passes it
                // on, and calling the next hook delivers it regardless of what
                // is returned. This is the whole point of the module.
                1
            }
            // SAFETY: forwarding the parameters we were handed, unmodified.
            // `None` for the hook handle is the documented form for
            // `WH_KEYBOARD_LL`.
            Handling::Pass => unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) },
        }
    }

    /// A live Escape hook. Dropping it restores ordinary Escape everywhere.
    pub struct EscapeTap {
        hook: HHOOK,
    }

    // SAFETY: `HHOOK` is an opaque kernel handle, not a pointer this code
    // dereferences. `UnhookWindowsHookEx` is documented without a
    // calling-thread restriction, so the guard may be dropped anywhere.
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
            // SAFETY: `hook` came from the `SetWindowsHookExW` in `arm` and is
            // unhooked exactly once, here.
            unsafe { UnhookWindowsHookEx(self.hook) };
        }
    }

    /// Install the hook.
    ///
    /// Must be called from a thread that pumps messages — the system delivers
    /// low-level hook callbacks through the installing thread's message queue,
    /// so a hook armed anywhere else is installed successfully and never fires.
    /// GPUI's main thread qualifies, which is the same requirement the macOS
    /// implementation has for the main run loop.
    pub fn arm() -> Result<EscapeTap, super::EscapeTapError> {
        // A `WH_KEYBOARD_LL` hook is global and its procedure lives in this
        // module, so the module handle is unused and the thread id must be 0.
        // SAFETY: `on_key` has the signature the hook type requires and is
        // valid for the lifetime of the process.
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), std::ptr::null_mut(), 0)
        };

        if hook.is_null() {
            // No permission gates this on Windows, so a failure here is not a
            // grant that can be handed over — it is a resource or
            // desktop-isolation problem the user cannot act on.
            return Err(super::EscapeTapError::NoRunLoopSource);
        }
        Ok(EscapeTap { hook })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn escape_going_down_is_aborted_and_swallowed() {
            // The branch the whole module exists for. `Pass` here would mean
            // the agent stops *and* its dialog receives the user's Escape.
            assert_eq!(
                handling(HC_ACTION, WM_KEYDOWN, || VK_ESCAPE),
                Handling::AbortAndConsume
            );
            assert_eq!(
                handling(HC_ACTION, WM_SYSKEYDOWN, || VK_ESCAPE),
                Handling::AbortAndConsume
            );
        }

        #[test]
        fn every_other_key_passes_straight_through() {
            // The tap is armed while an agent drives, and the user keeps
            // typing. Swallowing anything else would make the machine feel
            // broken in a way nothing would explain.
            for vk in [0x41_u32, 0x0D, 0x09, 0x20] {
                assert_eq!(handling(HC_ACTION, WM_KEYDOWN, || vk), Handling::Pass);
            }
        }

        #[test]
        fn a_key_up_is_not_consumed() {
            // Only the down stroke aborts. Eating the up stroke would leave an
            // application believing Escape is still held.
            const WM_KEYUP: u32 = 0x0101;
            assert_eq!(handling(HC_ACTION, WM_KEYUP, || VK_ESCAPE), Handling::Pass);
        }

        #[test]
        fn a_code_below_action_is_passed_without_being_inspected() {
            // Windows requires it, and the payload may not be readable — the
            // closure must not run at all.
            assert_eq!(
                handling(-1, WM_KEYDOWN, || panic!("must not read the payload")),
                Handling::Pass
            );
        }

        /// The end-to-end proof, and the only thing that shows the hook is
        /// really installed ahead of the focused application.
        ///
        /// Synthesizes a real Escape and pumps the message loop, because a
        /// low-level hook fires through the installing thread's queue and
        /// nothing arrives without one.
        #[test]
        fn a_real_escape_press_reaches_an_armed_hook() {
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
                VIRTUAL_KEY,
            };
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                PeekMessageW, MSG, PM_REMOVE,
            };

            // `ABORT_REQUESTED` is process-global, so this and
            // `arming_either_works_cleanly_or_fails_cleanly` cannot run at the
            // same time: one synthesizes a press, the other asserts nothing is
            // pending, and the harness runs tests on parallel threads. Same
            // lock the other test takes.
            let _serial = crate::platform::serialize_input_state();
            // Start from a known state rather than whatever ran before.
            ABORT_REQUESTED.store(false, Ordering::SeqCst);

            let Ok(tap) = arm() else {
                // Hook installation can fail on a locked or isolated desktop.
                // Skipping beats failing on something the code did not do.
                eprintln!("skipping: could not install the keyboard hook");
                return;
            };

            // A control hook, installed *after* `arm` so it runs ahead of it and
            // sees the key whatever the real hook does with it.
            //
            // It exists to separate two failures this test otherwise reports
            // identically: "the abort hook is broken" and "this desktop will not
            // deliver injected input at all". The second is not hypothetical —
            // UIPI silently drops injected input whenever the foreground window
            // outranks this process, `SendInput` still returns success, and a
            // test runner cannot control what has focus. Without this probe that
            // environment reads as a code regression.
            let _probe = ControlProbe::install();

            let key = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_ESCAPE as VIRTUAL_KEY,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let mut events = [key(0), key(KEYEVENTF_KEYUP)];

            // SAFETY: a well-formed INPUT array whose length matches, with the
            // size the API expects.
            let sent = unsafe {
                SendInput(
                    events.len() as u32,
                    events.as_mut_ptr(),
                    std::mem::size_of::<INPUT>() as i32,
                )
            };
            assert_eq!(sent, 2, "SendInput did not deliver the key");

            // Pump until the hook has run. Bounded so a desktop that never
            // delivers the event fails the assertion rather than hanging.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            while std::time::Instant::now() < deadline {
                // SAFETY: `message` is a valid MSG this thread owns.
                while unsafe {
                    PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE)
                } != 0
                {}
                if ABORT_REQUESTED.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            if !PROBE_SAW_INPUT.load(Ordering::SeqCst) {
                // Nothing reached *any* hook, so this says nothing about ours.
                eprintln!(
                    "skipping: this desktop did not deliver injected input \
                     (UIPI, or a higher-integrity foreground window)"
                );
                return;
            }

            assert!(
                tap.abort_requested(),
                "the control hook saw the injected Escape and the armed tap did \
                 not — the hook is installed but not acting on it"
            );
            // And the flag clears, so one press aborts once.
            assert!(!tap.abort_requested());
        }

        /// Set by [`ControlProbe`] the moment any key reaches a hook.
        static PROBE_SAW_INPUT: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        /// A pass-through low-level keyboard hook that only records that it ran.
        struct ControlProbe(windows_sys::Win32::UI::WindowsAndMessaging::HHOOK);

        impl ControlProbe {
            fn install() -> Option<Self> {
                PROBE_SAW_INPUT.store(false, Ordering::SeqCst);
                // SAFETY: a well-formed hook procedure, installed for this
                // thread's queue and removed on drop.
                let hook = unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::SetWindowsHookExW(
                        windows_sys::Win32::UI::WindowsAndMessaging::WH_KEYBOARD_LL,
                        Some(Self::callback),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                (!hook.is_null()).then_some(Self(hook))
            }

            /// Always chains, so installing this cannot change what the tap
            /// under test observes.
            unsafe extern "system" fn callback(
                code: i32,
                w: windows_sys::Win32::Foundation::WPARAM,
                l: windows_sys::Win32::Foundation::LPARAM,
            ) -> windows_sys::Win32::Foundation::LRESULT {
                PROBE_SAW_INPUT.store(true, Ordering::SeqCst);
                // SAFETY: forwarding the parameters this hook was handed.
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::CallNextHookEx(
                        std::ptr::null_mut(),
                        code,
                        w,
                        l,
                    )
                }
            }
        }

        impl Drop for ControlProbe {
            fn drop(&mut self) {
                // SAFETY: unhooking a handle this type owns, exactly once.
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(self.0);
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    /// Stand-in so the rest of the app compiles on platforms with neither
    /// implementation. Nothing ever arms this.
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
/// true: a named permission is actionable, "not supported here" is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EscapeTapError {
    /// Names both panes because the failure cannot tell them apart, and the
    /// less obvious one — Input Monitoring — is the one that is usually off.
    ///
    /// macOS-only in practice: a `WH_KEYBOARD_LL` hook needs no permission, so
    /// there is no switch to send a Windows user to. The variant stays on the
    /// type rather than being `cfg`-ed so callers keep one `match`.
    #[error(
        "OxiMux needs Input Monitoring permission to stop an agent with Escape — grant it in System Settings › Privacy & Security › Input Monitoring, and check Accessibility there too"
    )]
    NotPermitted,
    /// The hook or run-loop source could not be installed.
    ///
    /// Not actionable by the user on either platform, which is why it says so
    /// rather than sending them somewhere to change a setting that is already
    /// correct.
    #[cfg(not(windows))]
    #[error("could not attach the Escape tap to the run loop")]
    NoRunLoopSource,
    #[cfg(windows)]
    #[error("could not install the Escape keyboard hook")]
    NoRunLoopSource,
    #[cfg(not(windows))]
    #[error("stopping an agent with Escape is only available on macOS")]
    Unsupported,
    #[cfg(windows)]
    #[error("stopping an agent with Escape is not available on this system")]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::serialize_input_state;

    #[test]
    fn every_failure_says_what_the_user_can_do_about_it() {
        // The permission case is the one that actually happens, and it is
        // useless unless it names the switch to flip. Input Monitoring is the
        // one that gates a keyboard tap, so naming only Accessibility strands
        // anyone who has already granted that — which is the common case, since
        // it is the permission every other part of screen control asks for.
        let denied = EscapeTapError::NotPermitted.to_string();
        assert!(denied.contains("Input Monitoring"), "{denied}");
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

    /// Arming needs Input Monitoring permission, which CI does not have and a
    /// developer machine may not either — so this asserts the *contract* holds
    /// either way rather than that the tap succeeds.
    ///
    /// It is a real check despite that: a null-pointer bug in `arm`, a
    /// mis-declared FFI signature, or a double-release in `Drop` would crash
    /// here rather than in front of a user with an agent mid-drive.
    #[test]
    fn arming_either_works_cleanly_or_fails_cleanly() {
        let _serial = serialize_input_state();
        match arm() {
            Ok(tap) => {
                // Nothing has been pressed, so nothing is pending.
                assert!(!tap.abort_requested());
                // Tear down before re-arming. `Drop` releases the run-loop
                // source on macOS and unhooks on Windows, and releasing it here
                // is the whole setup for the re-arm below. Written as a scope
                // rather than `drop(tap)` because the fallback stub for other
                // platforms has no `Drop` impl, which makes `drop()` on it a
                // `clippy::drop_non_drop` error — the call would be a no-op
                // there, but the scope says what is meant everywhere.
                {
                    let _armed = tap;
                }
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
