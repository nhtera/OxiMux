//! Ref-counted prevent-idle-sleep assertion, held for three independent reasons.
//!
//! **Agents:** each agent-tab status watcher acquires an [`AwakeHold`] when its
//! agent enters `Running` and drops it on the way out (or when the tab closes —
//! RAII makes the release unconditional), so an overnight run keeps the machine
//! from idle-sleeping while costing nothing once everything is parked.
//!
//! **Remote access:** the host holds one for as long as it is bound. A phone can
//! only reach a machine that is awake, so without it remote control quietly
//! stops working a few minutes after the user walks away — the failure it exists
//! to prevent looks exactly like the feature being broken.
//!
//! **Scheduling:** a schedule armed to fire later holds one until it does. Note
//! this only prevents *idle sleep while the app is running* — it cannot wake a
//! sleeping machine or relaunch a quit app, so an overnight schedule still needs
//! the app left open.
//!
//! Each reason has its own user toggle (Notifications → "Keep this computer
//! awake while agents run", Remote → "Keep this computer awake while on"), and
//! they are tracked separately so neither setting silently governs the other.
//! Flipping one takes effect immediately on a live assertion in either
//! direction, while keeping the hold count — so re-enabling mid-run re-asserts.
//!
//! The process-global [`AgentAwake`] creates **one** OS assertion when the
//! first reason wants it and releases it when none do; the OS refcounts nothing
//! for us, so a second assertion would only be another thing to leak.
//!
//! Both shipping platforms have a real hold behind [`SleepAssertionBackend`] —
//! an IOKit assertion on macOS, a power request on Windows — so the toggles mean
//! the same thing on each. The seam also keeps the refcount/toggle logic
//! unit-testable without touching power management.

use std::sync::{Arc, Mutex, OnceLock};

/// Seam over the OS's prevent-idle-sleep call: `IOPMAssertionCreateWithName` /
/// `IOPMAssertionRelease` on macOS, a power request on Windows.
pub trait SleepAssertionBackend: Send + Sync {
    /// Create the assertion under `name` — what `pmset -g assertions` shows on
    /// macOS and `powercfg /requests` on Windows. `None` when the OS call fails
    /// (logged by the impl).
    ///
    /// The token is opaque to the caller and only ever handed back to the
    /// backend that minted it. It is 64 bits wide because the two platforms
    /// hand back different things — macOS a 32-bit assertion id, Windows a
    /// `HANDLE` — and narrowing a handle to fit the smaller of the two would
    /// truncate the pointer and leak the request.
    fn create(&self, name: &str) -> Option<u64>;
    fn release(&self, id: u64);
}

/// One independent justification for staying awake: how many things currently
/// want it, and whether the user has allowed this particular reason.
///
/// These share a single OS assertion. They are kept separate rather than summed
/// into one count and one flag because the flags are different user settings —
/// turning off "keep awake while agents run" must not also let the machine sleep
/// on a paired phone, or decide that scheduled runs may be missed.
#[derive(Default)]
struct Reason {
    holds: usize,
    enabled: bool,
}

impl Reason {
    fn wants(&self) -> bool {
        self.holds > 0 && self.enabled
    }
}

struct State {
    agent: Reason,
    remote: Reason,
    scheduling: Reason,
    assertion: Option<u64>,
}

pub struct AgentAwake {
    backend: Arc<dyn SleepAssertionBackend>,
    state: Mutex<State>,
}

impl AgentAwake {
    pub fn with_backend(backend: Arc<dyn SleepAssertionBackend>, enabled: bool) -> Self {
        Self {
            backend,
            state: Mutex::new(State {
                agent: Reason { holds: 0, enabled },
                remote: Reason { holds: 0, enabled },
                // Always enabled: scheduling keep-awake is not a user preference.
                // Creating a schedule already expresses the intent, and a
                // separate switch that silently caused scheduled runs to be
                // missed would be a footgun. So an armed schedule holds the
                // machine awake unconditionally — there is no toggle to gate it.
                scheduling: Reason { holds: 0, enabled: true },
                assertion: None,
            }),
        }
    }

    /// Register one running agent. The returned guard releases on drop.
    pub fn acquire(self: &Arc<Self>) -> AwakeHold {
        self.acquire_for(Source::Agent)
    }

    /// Keep the machine reachable while remote access is bound. Same guard, a
    /// separate reason: a phone can only reach a machine that is awake, so the host
    /// being up is its own justification independent of any agent running.
    pub fn acquire_remote(self: &Arc<Self>) -> AwakeHold {
        self.acquire_for(Source::Remote)
    }

    /// Keep the machine awake while a schedule is armed to fire.
    ///
    /// Its own reason for the same purpose the other two are separate: a user
    /// who turned off "keep awake while agents run" has not thereby said their
    /// scheduled runs should be missed.
    ///
    /// This only prevents **idle sleep while the app is running**. It cannot
    /// wake a machine that is already asleep, and it cannot relaunch a quit app —
    /// so an overnight schedule still needs the app left open. Every surface
    /// that creates a schedule is expected to say so.
    pub fn acquire_scheduling(self: &Arc<Self>) -> AwakeHold {
        self.acquire_for(Source::Scheduling)
    }

    fn acquire_for(self: &Arc<Self>, source: Source) -> AwakeHold {
        {
            let mut state = self.lock_state();
            source.reason_mut(&mut state).holds += 1;
            self.reevaluate(&mut state);
        }
        AwakeHold {
            owner: Arc::clone(self),
            source,
        }
    }

    /// Flip the user preference. Takes effect immediately on a live
    /// assertion in either direction.
    pub fn set_enabled(&self, enabled: bool) {
        self.set_enabled_for(Source::Agent, enabled);
    }

    /// The remote pane's counterpart of [`set_enabled`](Self::set_enabled).
    pub fn set_remote_enabled(&self, enabled: bool) {
        self.set_enabled_for(Source::Remote, enabled);
    }

    /// The remote preference as currently in force, so the toggle renders the
    /// live value rather than a copy that could drift from it.
    pub fn remote_enabled(&self) -> bool {
        self.lock_state().remote.enabled
    }

    fn set_enabled_for(&self, source: Source, enabled: bool) {
        let mut state = self.lock_state();
        source.reason_mut(&mut state).enabled = enabled;
        self.reevaluate(&mut state);
    }

    fn release_one(&self, source: Source) {
        let mut state = self.lock_state();
        let reason = source.reason_mut(&mut state);
        reason.holds = reason.holds.saturating_sub(1);
        self.reevaluate(&mut state);
    }

    /// Single source of the invariant: one assertion exists iff **any** reason
    /// wants it. One is enough — the OS refcounts nothing for us, and a second
    /// would just be another thing to leak.
    ///
    /// Note this does not re-create the assertion when the set of active reasons
    /// changes while it stays non-empty, so its name reflects whichever reasons
    /// were live when it was taken. The name is diagnostic only (it shows in
    /// `pmset -g assertions`); churning the assertion to keep the label current
    /// would trade a real resource for a cosmetic one.
    fn reevaluate(&self, state: &mut State) {
        let want = state.agent.wants() || state.remote.wants() || state.scheduling.wants();
        match (want, state.assertion) {
            (true, None) => {
                state.assertion = self.backend.create(assertion_name(state));
            }
            (false, Some(id)) => {
                self.backend.release(id);
                state.assertion = None;
            }
            _ => {}
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> (usize, bool) {
        let s = self.lock_state();
        (s.agent.holds, s.assertion.is_some())
    }

    /// Whether the OS assertion is currently held. Test-only, and `pub(crate)` so
    /// sibling modules that take holds (the scheduler) can assert on the effect.
    #[cfg(test)]
    pub(crate) fn asserted(&self) -> bool {
        self.lock_state().assertion.is_some()
    }
}

/// Which justification a hold belongs to, so releasing one can't decrement the
/// other's count.
#[derive(Clone, Copy)]
enum Source {
    Agent,
    Remote,
    Scheduling,
}

impl Source {
    fn reason_mut(self, state: &mut State) -> &mut Reason {
        match self {
            Source::Agent => &mut state.agent,
            Source::Remote => &mut state.remote,
            Source::Scheduling => &mut state.scheduling,
        }
    }
}

/// What `pmset -g assertions` (macOS) and `powercfg /requests` (Windows) will
/// show as the reason this machine is awake. Names
/// the actual cause so someone hunting a machine that won't sleep is not sent
/// looking for an agent that isn't running.
fn assertion_name(state: &State) -> &'static str {
    match (state.agent.wants(), state.remote.wants(), state.scheduling.wants()) {
        (true, true, _) => "OxiMux agent running, remote access on",
        (false, true, _) => "OxiMux remote access on",
        // Named last so an agent actually running still reads as the cause; a
        // schedule merely armed is the weakest of the three explanations for a
        // machine that will not sleep.
        (false, false, true) => "OxiMux schedule armed",
        _ => "OxiMux agent running",
    }
}

/// RAII guard for one stake in the assertion.
pub struct AwakeHold {
    owner: Arc<AgentAwake>,
    source: Source,
}

impl Drop for AwakeHold {
    fn drop(&mut self) {
        self.owner.release_one(self.source);
    }
}

/// Process-global instance over the real IOKit backend. Defaults to
/// enabled (matching `NotifyPrefValues::default`); boot hydration and the
/// settings toggle adjust it via `set_enabled`.
pub fn global() -> &'static Arc<AgentAwake> {
    static GLOBAL: OnceLock<Arc<AgentAwake>> = OnceLock::new();
    GLOBAL.get_or_init(|| Arc::new(AgentAwake::with_backend(platform_backend(), true)))
}

#[cfg(target_os = "macos")]
fn platform_backend() -> Arc<dyn SleepAssertionBackend> {
    Arc::new(iopm::IoPmBackend)
}

#[cfg(target_os = "windows")]
fn platform_backend() -> Arc<dyn SleepAssertionBackend> {
    Arc::new(power_request::PowerRequestBackend)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_backend() -> Arc<dyn SleepAssertionBackend> {
    /// No sleep-assertion path wired on this platform; holds are tracked
    /// but never materialize.
    struct NoopBackend;
    impl SleepAssertionBackend for NoopBackend {
        fn create(&self, _name: &str) -> Option<u64> {
            None
        }
        fn release(&self, _id: u64) {}
    }
    Arc::new(NoopBackend)
}

#[cfg(target_os = "macos")]
mod iopm {
    use std::ffi::c_void;

    use objc2_foundation::NSString;

    use super::SleepAssertionBackend;

    type CFStringRef = *const c_void;
    type IOPMAssertionID = u32;

    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;
    const KERN_SUCCESS: i32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionID,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> i32;
    }

    pub(super) struct IoPmBackend;

    impl SleepAssertionBackend for IoPmBackend {
        fn create(&self, name: &str) -> Option<u64> {
            // NSString is toll-free bridged to CFString, so the retained
            // pointers double as CFStringRef for the duration of the call
            // (both values outlive it — they drop at end of scope).
            let assertion_type = NSString::from_str("PreventUserIdleSystemSleep");
            let name = NSString::from_str(name);
            let mut id: IOPMAssertionID = 0;
            let rc = unsafe {
                IOPMAssertionCreateWithName(
                    objc2::rc::Retained::as_ptr(&assertion_type) as CFStringRef,
                    LEVEL_ON,
                    objc2::rc::Retained::as_ptr(&name) as CFStringRef,
                    &mut id,
                )
            };
            if rc == KERN_SUCCESS {
                Some(u64::from(id))
            } else {
                tracing::warn!(rc, "IOPMAssertionCreateWithName failed");
                None
            }
        }

        fn release(&self, id: u64) {
            let _ = unsafe { IOPMAssertionRelease(id as IOPMAssertionID) };
        }
    }
}

#[cfg(target_os = "windows")]
mod power_request {
    use widestring::U16CString;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    use super::SleepAssertionBackend;

    /// `POWER_REQUEST_CONTEXT_VERSION`. Not exported by `windows-sys`; the SDK
    /// defines it as 0 and has since the API shipped in Windows 7.
    const CONTEXT_VERSION: u32 = 0;

    /// The Windows counterpart of the IOKit assertion: a **power request**
    /// rather than `SetThreadExecutionState`.
    ///
    /// `SetThreadExecutionState` is the older and better-known call, and it is
    /// the wrong one here for a reason that has nothing to do with taste: its
    /// effect belongs to the *calling thread* and lasts only as long as that
    /// thread does. Holds here are taken and dropped from wherever an agent,
    /// the remote host or the scheduler happens to be running, so the thread
    /// that acquires is routinely not the thread that releases — and a thread
    /// that exits while holding would silently drop the assertion.
    ///
    /// A power request is process-wide and handle-based, so it survives the
    /// thread that made it and is released by whoever holds the handle. It also
    /// carries the reason string, which is what makes the hold accountable:
    /// `powercfg /requests` names OxiMux and says *why*, the same way
    /// `pmset -g assertions` does on macOS.
    pub(super) struct PowerRequestBackend;

    impl SleepAssertionBackend for PowerRequestBackend {
        fn create(&self, name: &str) -> Option<u64> {
            // The reason string must outlive the `PowerCreateRequest` call —
            // Windows copies it into the request — so it is held in a binding
            // rather than materialized inside the struct literal.
            let Ok(reason) = U16CString::from_str(name) else {
                // Only reachable if the name contains an interior NUL. Ours are
                // string literals, so this is a guard, not a case.
                tracing::warn!("keep-awake reason string was not representable");
                return None;
            };
            let context = REASON_CONTEXT {
                Version: CONTEXT_VERSION,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: reason.as_ptr() as *mut u16,
                },
            };
            // SAFETY: `context` is fully initialized and its string outlives
            // the call; the union arm matches the `SIMPLE_STRING` flag.
            let handle: HANDLE = unsafe { PowerCreateRequest(&context) };
            if handle.is_null() {
                tracing::warn!("PowerCreateRequest failed");
                return None;
            }
            // `SystemRequired` prevents *idle sleep* while leaving the display
            // free to blank — the same scope as `PreventUserIdleSystemSleep` on
            // macOS. Keeping the screen on is a different promise and not one
            // any of these three reasons made.
            // SAFETY: `handle` is a live power request from the call above.
            if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) } == 0 {
                tracing::warn!("PowerSetRequest failed");
                // SAFETY: closing a handle we own and are about to forget.
                unsafe { CloseHandle(handle) };
                return None;
            }
            Some(handle as u64)
        }

        fn release(&self, id: u64) {
            let handle = id as HANDLE;
            // SAFETY: `id` came from this backend's `create`, which only ever
            // returns a live request handle, and `reevaluate` hands each token
            // back exactly once.
            //
            // Logged rather than ignored because a clear that does not land is
            // invisible from inside the app and permanent from outside it: the
            // request outlives every reason for it and the machine simply
            // stops idle-sleeping, with nothing on screen to say why.
            if unsafe { PowerClearRequest(handle, PowerRequestSystemRequired) } == 0 {
                tracing::warn!("PowerClearRequest failed; the keep-awake hold may outlive the app");
            }
            // SAFETY: closing a handle this backend owns, exactly once.
            unsafe { CloseHandle(handle) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Against the real kernel32, not a mock: the point of this one is that
        /// Windows *accepted* the request. A malformed `REASON_CONTEXT` — wrong
        /// version, a flag that disagrees with the union arm, a reason string
        /// that stopped existing before the call — fails right here, and it is
        /// the only way to find out short of an elevated `powercfg /requests`.
        #[test]
        fn windows_grants_and_releases_a_real_power_request() {
            let backend = PowerRequestBackend;
            let id = backend
                .create("OxiMux test assertion")
                .expect("PowerCreateRequest + PowerSetRequest should succeed");
            assert_ne!(id, 0, "a granted request has a real handle");

            // Clearing through the round-tripped token is the half worth
            // asserting. `release` does exactly this and can only log when it
            // fails, so if widening the handle to `u64` and back ever stopped
            // naming the same request, the release would quietly clear nothing
            // and the machine would never idle-sleep again. Here it is an
            // assertion instead.
            let handle = id as HANDLE;
            // SAFETY: `handle` is the live request `create` just returned.
            assert_ne!(
                unsafe { PowerClearRequest(handle, PowerRequestSystemRequired) },
                0,
                "the round-tripped handle must still name the request"
            );
            // SAFETY: closing the handle this test owns, exactly once.
            assert_ne!(unsafe { CloseHandle(handle) }, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct MockBackend {
        creates: AtomicUsize,
        releases: AtomicUsize,
        next_id: AtomicU32,
        last_name: Mutex<String>,
    }

    impl SleepAssertionBackend for MockBackend {
        fn create(&self, name: &str) -> Option<u64> {
            *self.last_name.lock().unwrap() = name.to_string();
            self.creates.fetch_add(1, Ordering::Relaxed);
            Some(u64::from(self.next_id.fetch_add(1, Ordering::Relaxed)))
        }
        fn release(&self, _id: u64) {
            self.releases.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fixture(enabled: bool) -> (Arc<AgentAwake>, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), enabled));
        (awake, backend)
    }

    #[test]
    fn assertion_spans_first_to_last_hold() {
        let (awake, backend) = fixture(true);
        let h1 = awake.acquire();
        let h2 = awake.acquire();
        assert_eq!(backend.creates.load(Ordering::Relaxed), 1);
        drop(h1);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 0);
        drop(h2);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
        assert_eq!(awake.snapshot(), (0, false));
    }

    #[test]
    fn disabled_never_creates() {
        let (awake, backend) = fixture(false);
        let _h = awake.acquire();
        assert_eq!(backend.creates.load(Ordering::Relaxed), 0);
        assert_eq!(awake.snapshot(), (1, false));
    }

    #[test]
    fn toggle_off_releases_live_assertion_and_on_reasserts() {
        let (awake, backend) = fixture(true);
        let _h = awake.acquire();
        awake.set_enabled(false);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
        awake.set_enabled(true);
        assert_eq!(backend.creates.load(Ordering::Relaxed), 2);
        assert_eq!(awake.snapshot(), (1, true));
    }

    #[test]
    fn reacquire_after_drain_creates_again() {
        let (awake, backend) = fixture(true);
        drop(awake.acquire());
        let _h = awake.acquire();
        assert_eq!(backend.creates.load(Ordering::Relaxed), 2);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
    }

    /// The whole point of two reasons: the settings that gate them are different
    /// questions. Turning off "keep awake while agents run" must not let the machine
    /// sleep on a paired phone, and turning off the remote one must not undo the
    /// agent behaviour either.
    #[test]
    fn each_reason_is_gated_by_its_own_setting() {
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));

        let _agent = awake.acquire();
        let _remote = awake.acquire_remote();
        assert!(awake.asserted(), "both reasons want it");

        awake.set_enabled(false);
        assert!(awake.asserted(), "remote alone still holds the machine awake");

        awake.set_remote_enabled(false);
        assert!(!awake.asserted(), "with neither reason allowed, it is released");

        awake.set_enabled(true);
        assert!(awake.asserted(), "and re-allowing either one re-asserts");
    }

    /// Releasing one reason's guard must not decrement the other's count — a
    /// shared counter would let a closing agent tab drop the assertion out from
    /// under a live remote host.
    #[test]
    fn releasing_one_reason_leaves_the_other_holding() {
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));

        let agent = awake.acquire();
        let _remote = awake.acquire_remote();
        drop(agent);

        assert!(awake.asserted(), "the remote hold survives the agent's release");
        assert_eq!(backend.releases.load(Ordering::Relaxed), 0, "nothing was released yet");
    }

    /// `pmset -g assertions` should name the actual cause, so someone hunting a
    /// machine that will not sleep is not sent looking for an agent that is not
    /// running.
    #[test]
    fn the_assertion_names_the_reason_that_took_it() {
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));

        let remote = awake.acquire_remote();
        assert_eq!(*backend.last_name.lock().unwrap(), "OxiMux remote access on");
        drop(remote);

        let _agent = awake.acquire();
        assert_eq!(*backend.last_name.lock().unwrap(), "OxiMux agent running");
    }

    #[test]
    fn create_failure_leaves_no_assertion_but_keeps_holds() {
        struct FailBackend;
        impl SleepAssertionBackend for FailBackend {
            fn create(&self, _name: &str) -> Option<u64> {
                None
            }
            fn release(&self, _id: u64) {}
        }
        let awake = Arc::new(AgentAwake::with_backend(Arc::new(FailBackend), true));
        let _h = awake.acquire();
        assert_eq!(awake.snapshot(), (1, false));
    }

    /// An armed schedule holds the assertion on its own — with no agent running
    /// and remote off. Without this a scheduled run set for later would sit
    /// behind an idle-sleeping machine and silently never fire.
    #[test]
    fn a_scheduling_hold_asserts_by_itself() {
        let backend = Arc::new(MockBackend::default());
        let awake = Arc::new(AgentAwake::with_backend(backend.clone(), true));
        assert!(!awake.asserted(), "nothing wants it yet");

        let hold = awake.acquire_scheduling();
        assert!(awake.asserted(), "an armed schedule is its own reason to stay awake");
        assert_eq!(*backend.last_name.lock().unwrap(), "OxiMux schedule armed");

        drop(hold);
        assert!(!awake.asserted(), "released when no schedule is armed");
    }

    /// The three reasons are refcounted independently: releasing one must not
    /// drop an assertion another still wants.
    #[test]
    fn scheduling_and_agent_holds_do_not_release_each_other() {
        let awake = Arc::new(AgentAwake::with_backend(Arc::new(MockBackend::default()), true));
        let agent = awake.acquire();
        let scheduling = awake.acquire_scheduling();
        assert!(awake.asserted());

        drop(agent);
        assert!(awake.asserted(), "the schedule still wants it");

        drop(scheduling);
        assert!(!awake.asserted(), "now nothing does");
    }

    /// Scheduling is not gated on the agent preference. Turning off "keep awake
    /// while agents run" must not also decide that scheduled runs may be missed —
    /// so a scheduling hold keeps asserting after the agent reason is disabled,
    /// and only releasing the hold (no schedule armed) drops it.
    #[test]
    fn disabling_the_agent_reason_leaves_scheduling_holding() {
        let awake = Arc::new(AgentAwake::with_backend(Arc::new(MockBackend::default()), true));
        let _agent = awake.acquire();
        let scheduling = awake.acquire_scheduling();

        awake.set_enabled(false);
        assert!(awake.asserted(), "scheduling is not gated on the agent preference");

        drop(scheduling);
        assert!(!awake.asserted(), "nothing armed, nothing holds");
    }

    /// Scheduling keep-awake has no user toggle and defaults on regardless of the
    /// agent preference passed at construction — an armed schedule holds the
    /// machine awake even when the app was built with keep-awake off.
    #[test]
    fn scheduling_holds_even_when_constructed_disabled() {
        let awake = Arc::new(AgentAwake::with_backend(Arc::new(MockBackend::default()), false));
        let _scheduling = awake.acquire_scheduling();
        assert!(awake.asserted(), "scheduling is always enabled, not gated on the seed");
    }
}
