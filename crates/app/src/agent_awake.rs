//! Ref-counted prevent-idle-sleep assertion held while any agent runs.
//!
//! Each agent-tab status watcher acquires an [`AwakeHold`] when its agent
//! enters `Running` and drops it on the way out (or when the tab closes —
//! RAII makes the release unconditional). The process-global [`AgentAwake`]
//! creates one IOKit power assertion at the 0→1 hold transition and
//! releases it at 1→0, so an overnight agent run keeps the machine from
//! idle-sleeping while costing nothing once everything is parked.
//!
//! The user toggle (settings → Notifications → "Keep Mac awake…") flips
//! `set_enabled`; turning it off releases a live assertion immediately
//! while keeping the hold count, so re-enabling mid-run re-asserts.
//!
//! IOKit is behind [`SleepAssertionBackend`] so the refcount/toggle logic
//! is unit-testable without touching power management.

use std::sync::{Arc, Mutex, OnceLock};

/// Seam over `IOPMAssertionCreateWithName` / `IOPMAssertionRelease`.
pub trait SleepAssertionBackend: Send + Sync {
    /// Create the assertion; `None` when the OS call fails (logged by the
    /// impl). The returned token is opaque to the caller.
    fn create(&self) -> Option<u32>;
    fn release(&self, id: u32);
}

struct State {
    holds: usize,
    enabled: bool,
    assertion: Option<u32>,
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
                holds: 0,
                enabled,
                assertion: None,
            }),
        }
    }

    /// Register one running agent. The returned guard releases on drop.
    pub fn acquire(self: &Arc<Self>) -> AwakeHold {
        {
            let mut state = self.lock_state();
            state.holds += 1;
            self.reevaluate(&mut state);
        }
        AwakeHold {
            owner: Arc::clone(self),
        }
    }

    /// Flip the user preference. Takes effect immediately on a live
    /// assertion in either direction.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.lock_state();
        state.enabled = enabled;
        self.reevaluate(&mut state);
    }

    fn release_one(&self) {
        let mut state = self.lock_state();
        state.holds = state.holds.saturating_sub(1);
        self.reevaluate(&mut state);
    }

    /// Single source of the invariant: an assertion exists iff
    /// (holds > 0 && enabled).
    fn reevaluate(&self, state: &mut State) {
        let want = state.holds > 0 && state.enabled;
        match (want, state.assertion) {
            (true, None) => state.assertion = self.backend.create(),
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
        (s.holds, s.assertion.is_some())
    }
}

/// RAII guard for one running agent's stake in the assertion.
pub struct AwakeHold {
    owner: Arc<AgentAwake>,
}

impl Drop for AwakeHold {
    fn drop(&mut self) {
        self.owner.release_one();
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

#[cfg(not(target_os = "macos"))]
fn platform_backend() -> Arc<dyn SleepAssertionBackend> {
    /// No sleep-assertion path wired on this platform; holds are tracked
    /// but never materialize.
    struct NoopBackend;
    impl SleepAssertionBackend for NoopBackend {
        fn create(&self) -> Option<u32> {
            None
        }
        fn release(&self, _id: u32) {}
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
        fn create(&self) -> Option<u32> {
            // NSString is toll-free bridged to CFString, so the retained
            // pointers double as CFStringRef for the duration of the call
            // (both values outlive it — they drop at end of scope).
            let assertion_type = NSString::from_str("PreventUserIdleSystemSleep");
            let name = NSString::from_str("OxiMux agent running");
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
                Some(id)
            } else {
                tracing::warn!(rc, "IOPMAssertionCreateWithName failed");
                None
            }
        }

        fn release(&self, id: u32) {
            let _ = unsafe { IOPMAssertionRelease(id) };
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
    }

    impl SleepAssertionBackend for MockBackend {
        fn create(&self) -> Option<u32> {
            self.creates.fetch_add(1, Ordering::Relaxed);
            Some(self.next_id.fetch_add(1, Ordering::Relaxed))
        }
        fn release(&self, _id: u32) {
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

    #[test]
    fn create_failure_leaves_no_assertion_but_keeps_holds() {
        struct FailBackend;
        impl SleepAssertionBackend for FailBackend {
            fn create(&self) -> Option<u32> {
                None
            }
            fn release(&self, _id: u32) {}
        }
        let awake = Arc::new(AgentAwake::with_backend(Arc::new(FailBackend), true));
        let _h = awake.acquire();
        assert_eq!(awake.snapshot(), (1, false));
    }
}
