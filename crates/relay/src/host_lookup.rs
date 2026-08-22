//! Is this process still able to ask the OS who anything is?
//!
//! The daemon is spawned detached and outlives whatever started it. On macOS
//! it also inherits that spawner's Mach bootstrap port, and a bootstrap port
//! is a task special port: inherited on fork, preserved across exec. So every
//! PTY the daemon spawns gets a copy of it, and if the daemon's copy ever goes
//! dead, so does every terminal in the app — permanently, and without a word
//! in any log.
//!
//! That is not hypothetical. A daemon left running for eight days was measured
//! in exactly that state: inside its PTYs, `getpwuid` returned NULL (zsh's
//! `%n` rendered empty and `whoami` printed the bare uid `501`), `getaddrinfo`
//! could not resolve `github.com`, `dscacheutil` answered nothing, and `gh`
//! silently fell back from the keychain to its plaintext config token. A relay
//! spawned fresh the same minute, same binary, was clean on every one of those.
//!
//! What ties those failures together is that all of them are XPC lookups —
//! libinfo reaching `opendirectoryd`, `mDNSResponder`, `securityd` — and all of
//! them start with a `bootstrap_look_up` on the inherited port. `nslookup` kept
//! working throughout, because it builds DNS packets and talks to the
//! nameserver directly instead of going through libinfo. That contradiction
//! (raw sockets fine, every name lookup dead) is the fingerprint, and it is why
//! probing DNS would be the wrong test: the network is not what breaks.
//!
//! `getpwuid` is the probe instead. It is the cheapest thing on that path, it
//! needs no network, and it is the call that was observed failing first.
//!
//! The trade this makes, stated here so it is chosen rather than discovered: a
//! machine-wide lookup outage lasting longer than `failures_before_fatal` ×
//! the caller's probe interval now costs the user every open pane once, where
//! before it passed unnoticed. The blast radius is one cycle rather than a
//! loop — the replacement daemon probes broken at boot, is classified
//! born-degraded, and rides the outage out instead of exiting again. Widening
//! that margin means raising either knob and being slower to catch a real
//! death.
//!
//! Unix-only. Windows has no libinfo, no bootstrap port, and nothing that
//! inherits this way — there is no failure here to watch for.

use nix::unistd::{Uid, User};

/// One probe of the OS user-identity lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// `getpwuid()` answered. The lookup path is alive.
    Resolved(String),
    /// `getpwuid()` returned NULL, or errored. libinfo cannot reach the
    /// system lookup daemons, and neither will anything this process spawns.
    Unresolvable,
}

/// Ask the OS for the name behind our own uid.
///
/// Blocking, though only just — a healthy lookup is a round trip to
/// `opendirectoryd`. Callers on an async runtime should still hand it to
/// `spawn_blocking` rather than assume it stays fast in the broken state, and
/// should bound the wait: nothing promises a dead lookup path fails rather
/// than hangs.
pub fn probe() -> Identity {
    match User::from_uid(Uid::current()) {
        Ok(Some(user)) => Identity::Resolved(user.name),
        // `Ok(None)` is the interesting one: the call completed and the OS had
        // no answer for a uid that unquestionably exists. `Err` is folded in
        // with it because the consequence for anything we spawn is identical.
        Ok(None) | Err(_) => Identity::Unresolvable,
    }
}

/// The same question as [`probe`], for callers that only want the answer.
///
/// The string allocation inside `probe` is nix's — `User` owns its fields —
/// and is not worth dodging at one call per probe interval. What this buys is
/// a call site that asks for what it needs instead of matching a payload it
/// immediately drops; the name is only ever wanted by the boot-time log.
pub fn is_healthy() -> bool {
    matches!(probe(), Identity::Resolved(_))
}

/// What the watcher should do about the probe it just took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Healthy, or a degradation already reported recently. Say nothing — this
    /// fires on every tick of a working daemon and must stay silent.
    Quiet,
    /// Came back after at least one failed probe.
    Recovered,
    /// A probe failed, but not enough of them in a row to act on. Worth a
    /// warning, not worth killing a daemon with live terminals under it.
    Flaky { consecutive: u32 },
    /// Broken since boot, so there is no healthy state to return to. Reported
    /// on the first failure and periodically after — see [`LookupWatch`] for
    /// why this deliberately never escalates, and why it repeats.
    BornDegraded,
    /// Was healthy, now persistently is not. The daemon should stop serving so
    /// the host respawns one from its own context.
    Died { consecutive: u32 },
}

impl Verdict {
    /// Does this verdict mean the daemon has to go?
    ///
    /// The classification lives here rather than at the call site so a test can
    /// pin it. Exactly one verdict ends the daemon, and a new variant added
    /// without deciding its fatality trips `only_died_is_fatal`.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Verdict::Died { .. })
    }
}

/// Turns a stream of probe results into a decision.
///
/// Two rules carry the whole design, and both exist to avoid making things
/// worse than the bug being watched for:
///
/// 1. **Only a healthy→broken transition is fatal.** A daemon whose *first*
///    probe fails was born into a broken context, which means the process that
///    spawned it is broken too — exiting would just hand the host an identical
///    replacement, and every respawn costs the user every terminal pane. So a
///    daemon born degraded keeps serving; PTYs still run, they just cannot
///    resolve names. Recovery there needs a human, not a restart. It does say
///    so repeatedly, on `degraded_rereport_every`: the daemon can outlive many
///    daily log rotations, and a single boot-time line would leave the live log
///    empty for whoever eventually goes looking — the exact blindness this
///    module exists to remove.
///
/// 2. **A single failed probe is never fatal.** `opendirectoryd` restarts, and
///    a probe that lands mid-restart fails without anything being wrong. Only
///    `failures_before_fatal` consecutive failures count, so the cost of a
///    false positive is a warning line rather than the user's session.
#[derive(Debug)]
pub struct LookupWatch {
    seen_healthy: bool,
    consecutive_failures: u32,
    failures_before_fatal: u32,
    degraded_rereport_every: u32,
}

impl LookupWatch {
    /// `failures_before_fatal` is clamped to at least 1: a threshold of zero
    /// would make the first tick fatal, which is the opposite of rule 2.
    /// `degraded_rereport_every` — how many further failed probes a
    /// born-degraded daemon takes before repeating itself — is clamped because
    /// zero would silently disable the repeat entirely: nothing is a multiple
    /// of zero except zero, so every later probe would fall through to Quiet
    /// and the daemon would go dark after its first line.
    pub fn new(failures_before_fatal: u32, degraded_rereport_every: u32) -> Self {
        Self {
            seen_healthy: false,
            consecutive_failures: 0,
            failures_before_fatal: failures_before_fatal.max(1),
            degraded_rereport_every: degraded_rereport_every.max(1),
        }
    }

    /// Fold one probe result into the watch and say what it means.
    pub fn observe(&mut self, healthy: bool) -> Verdict {
        if healthy {
            let recovered = self.consecutive_failures > 0;
            self.consecutive_failures = 0;
            self.seen_healthy = true;
            return if recovered {
                Verdict::Recovered
            } else {
                Verdict::Quiet
            };
        }

        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        if !self.seen_healthy {
            // Rule 1: report the first failure, then every `rereport_every`
            // after it. Often enough that the live log always carries the
            // reason, rare enough that it never becomes noise.
            return if self.consecutive_failures == 1
                || self
                    .consecutive_failures
                    .is_multiple_of(self.degraded_rereport_every)
            {
                Verdict::BornDegraded
            } else {
                Verdict::Quiet
            };
        }

        // Rule 2.
        if self.consecutive_failures >= self.failures_before_fatal {
            Verdict::Died {
                consecutive: self.consecutive_failures,
            }
        } else {
            Verdict::Flaky {
                consecutive: self.consecutive_failures,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts against the host's passwd database rather than against anything
    /// in this module, which makes it the one test here that can prove `probe`
    /// ever returns true — and the one that is not hermetic. It fails wherever
    /// the running uid has no passwd entry: a container started with
    /// `docker run --user $(id -u)`, a nix build sandbox, a minimal CI image.
    /// This repo's CI runs on plain GitHub-hosted runners with no `container:`
    /// key, so the assumption holds today; a move to containerised runners
    /// would need this gated rather than debugged.
    #[test]
    fn probe_resolves_on_a_healthy_host() {
        assert!(
            matches!(probe(), Identity::Resolved(name) if !name.is_empty()),
            "getpwuid failed on the test host — either the host itself is in \
             the broken state this module detects, or the uid running the \
             tests has no passwd entry (see this test's doc comment)"
        );
    }

    #[test]
    fn is_healthy_agrees_with_probe() {
        assert_eq!(is_healthy(), matches!(probe(), Identity::Resolved(_)));
    }

    /// The daemon's shutdown hangs off this one classification, and nothing
    /// else in the suite can catch a variant being reclassified.
    #[test]
    fn only_died_is_fatal() {
        assert!(Verdict::Died { consecutive: 3 }.is_fatal());
        for benign in [
            Verdict::Quiet,
            Verdict::Recovered,
            Verdict::BornDegraded,
            Verdict::Flaky { consecutive: 2 },
        ] {
            assert!(!benign.is_fatal(), "{benign:?} must not end the daemon");
        }
    }

    #[test]
    fn a_working_daemon_stays_silent() {
        let mut watch = LookupWatch::new(3, 120);
        for _ in 0..10 {
            assert_eq!(watch.observe(true), Verdict::Quiet);
        }
    }

    /// Rule 1. The respawn loop this prevents is the expensive failure: each
    /// cycle would take every terminal pane with it and land right back here.
    #[test]
    fn born_degraded_never_escalates() {
        let mut watch = LookupWatch::new(2, 100);
        assert_eq!(watch.observe(false), Verdict::BornDegraded);
        for _ in 0..20 {
            let verdict = watch.observe(false);
            assert!(
                !verdict.is_fatal(),
                "a daemon born into a broken context must not ask to be \
                 restarted, got {verdict:?}"
            );
        }
    }

    /// ...but it does keep saying so, or daily log rotation leaves the live log
    /// with no record of why every pane is broken.
    #[test]
    fn born_degraded_re_reports_on_a_cadence() {
        let mut watch = LookupWatch::new(3, 4);
        assert_eq!(watch.observe(false), Verdict::BornDegraded); // 1st
        assert_eq!(watch.observe(false), Verdict::Quiet);
        assert_eq!(watch.observe(false), Verdict::Quiet);
        assert_eq!(watch.observe(false), Verdict::BornDegraded); // 4th
        assert_eq!(watch.observe(false), Verdict::Quiet);
        assert_eq!(watch.observe(false), Verdict::Quiet);
        assert_eq!(watch.observe(false), Verdict::Quiet);
        assert_eq!(watch.observe(false), Verdict::BornDegraded); // 8th
    }

    /// Rule 2. One failed probe is `opendirectoryd` bouncing, not a dead
    /// bootstrap port.
    #[test]
    fn a_single_failure_is_not_fatal() {
        let mut watch = LookupWatch::new(3, 120);
        watch.observe(true);
        assert_eq!(watch.observe(false), Verdict::Flaky { consecutive: 1 });
        assert_eq!(watch.observe(false), Verdict::Flaky { consecutive: 2 });
    }

    #[test]
    fn healthy_then_persistent_failure_is_fatal() {
        let mut watch = LookupWatch::new(3, 120);
        watch.observe(true);
        watch.observe(false);
        watch.observe(false);
        assert_eq!(watch.observe(false), Verdict::Died { consecutive: 3 });
    }

    /// A recovery has to clear the streak, or the failures either side of a
    /// healthy probe would add up and kill a daemon that is working.
    #[test]
    fn recovery_resets_the_streak() {
        let mut watch = LookupWatch::new(3, 120);
        watch.observe(true);
        watch.observe(false);
        watch.observe(false);
        assert_eq!(watch.observe(true), Verdict::Recovered);
        assert_eq!(watch.observe(false), Verdict::Flaky { consecutive: 1 });
    }

    /// Both knobs are floored in the constructor: zero would make the first
    /// failure fatal (undoing rule 2) and would make the re-report modulus
    /// divide by zero.
    #[test]
    fn zero_knobs_are_clamped() {
        let mut fatal = LookupWatch::new(0, 0);
        fatal.observe(true);
        assert_eq!(fatal.observe(false), Verdict::Died { consecutive: 1 });

        let mut degraded = LookupWatch::new(0, 0);
        for _ in 0..5 {
            assert_eq!(degraded.observe(false), Verdict::BornDegraded);
        }
    }
}
