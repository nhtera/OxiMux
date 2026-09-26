//! Per-device consent for agent control: may an agent drive this simulator?
//!
//! Two keys, deliberately different:
//!
//! - **The answer belongs to the device** (its udid). "Allow" lets every
//!   agent drive that simulator, from any worktree, until the user revokes it
//!   in Settings; "Don't allow" refuses the device for [`DENY_COOLDOWN`].
//! - **The question belongs to the worktree that asked.** A request is queued
//!   per `(device, worktree)` so the desktop can show it where that worktree's
//!   panel is (or point there from elsewhere), and it is never shown over
//!   another worktree's panel.
//!
//! Asking is non-blocking: a control verb that has no answer yet raises a
//! request and returns [`Verdict::Pending`] at once, and the agent retries. A
//! request nobody polls for [`PENDING_TTL`] is dropped, so an agent that gave
//! up does not leave a banner behind. While a device is denied, verbs are
//! refused without asking again.
//!
//! Approvals are persisted by the caller (the app's database, which only the
//! app writes — not the settings file an agent's tools edit as a matter of
//! course); this type only mirrors them in memory. Like every guard against a
//! process running as the same user, this is advisory: such a process could
//! write the database, or drive the simulator with `xcrun simctl` directly.
//! What it enforces is OxiMux's own verbs and stream input.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::DeviceId;
use crate::registry::WorktreeKey;

/// How long "Don't allow" holds before an agent may ask again.
pub const DENY_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// A request no agent has polled for this long is dropped.
pub const PENDING_TTL: Duration = Duration::from_secs(90);
/// A request still open this long after it was last shown is shown again on
/// the next control verb: a missed toast must not be a dead end.
pub const REANNOUNCE_AFTER: Duration = Duration::from_secs(30);

/// What a control verb may do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// Asked; waiting for the user.
    Pending,
    /// Refused; asking again is possible after this long.
    Denied { retry_after: Duration },
}

/// Where a device stands, for `sim status` (no request is raised).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    NotAsked,
    Pending,
    Allowed,
    Denied { retry_after: Duration },
}

/// One outstanding question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub udid: DeviceId,
    pub worktree: WorktreeKey,
    /// The device's name as the host resolved it (never caller text).
    pub device_name: String,
    /// Last time an agent asked or polled; the TTL runs from here.
    seen: Instant,
    /// Last time the question was shown to the user.
    announced: Instant,
}

#[derive(Debug, Default)]
pub struct Consent {
    approved: HashSet<DeviceId>,
    denied: HashMap<DeviceId, Instant>,
    pending: Vec<Request>,
}

impl Consent {
    /// Start from the persisted approvals.
    pub fn new(approved: impl IntoIterator<Item = DeviceId>) -> Self {
        Self { approved: approved.into_iter().collect(), ..Self::default() }
    }

    pub fn is_approved(&self, udid: &DeviceId) -> bool {
        self.approved.contains(udid)
    }

    /// A control verb wants `udid` on behalf of `worktree`. Returns the verdict
    /// and whether the question should be shown now: a new request, or one
    /// left open for [`REANNOUNCE_AFTER`] since it was last shown.
    pub fn check(&mut self, udid: &DeviceId, worktree: &WorktreeKey, device_name: &str, now: Instant) -> (Verdict, bool) {
        if let Some(verdict) = self.decided(udid, now) {
            return (verdict, false);
        }
        if let Some(request) = self.pending.iter_mut().find(|r| &r.udid == udid && &r.worktree == worktree) {
            request.seen = now;
            let again = now.saturating_duration_since(request.announced) >= REANNOUNCE_AFTER;
            if again {
                request.announced = now;
            }
            return (Verdict::Pending, again);
        }
        self.pending.push(Request {
            udid: udid.clone(),
            worktree: worktree.clone(),
            device_name: device_name.to_owned(),
            seen: now,
            announced: now,
        });
        (Verdict::Pending, true)
    }

    /// Where `udid` stands for `worktree`. Polling keeps that worktree's
    /// request alive (an agent waiting on `sim wait-consent` polls this).
    pub fn state(&mut self, udid: &DeviceId, worktree: &WorktreeKey, now: Instant) -> State {
        match self.decided(udid, now) {
            Some(Verdict::Allowed) => State::Allowed,
            Some(Verdict::Denied { retry_after }) => State::Denied { retry_after },
            _ => match self.pending.iter_mut().find(|r| &r.udid == udid && &r.worktree == worktree) {
                Some(request) => {
                    request.seen = now;
                    State::Pending
                }
                None => State::NotAsked,
            },
        }
    }

    /// The user allowed `udid`: every request for it is answered, from any
    /// worktree. The caller persists the approval.
    pub fn allow(&mut self, udid: &DeviceId) {
        self.approved.insert(udid.clone());
        self.denied.remove(udid);
        self.pending.retain(|r| &r.udid != udid);
    }

    /// The user refused `udid`, for [`DENY_COOLDOWN`].
    pub fn deny(&mut self, udid: &DeviceId, now: Instant) {
        self.denied.insert(udid.clone(), now + DENY_COOLDOWN);
        self.pending.retain(|r| &r.udid != udid);
    }

    /// Settings withdrew the approval.
    pub fn revoke(&mut self, udid: &DeviceId) {
        self.approved.remove(udid);
    }

    /// Drop requests nobody polled for [`PENDING_TTL`] and denials that ran
    /// out. Returns whether anything changed (the banner may go away).
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = (self.pending.len(), self.denied.len());
        self.pending.retain(|r| now.saturating_duration_since(r.seen) < PENDING_TTL);
        self.denied.retain(|_, until| *until > now);
        before != (self.pending.len(), self.denied.len())
    }

    /// The request `worktree` is waiting on for `udid` — the device attached
    /// there now, so a question about a device it has since left is never
    /// shown as if it were about this one.
    pub fn pending_for(&self, worktree: &WorktreeKey, udid: &DeviceId) -> Option<&Request> {
        self.pending.iter().find(|r| &r.worktree == worktree && &r.udid == udid)
    }

    /// `worktree` detached or switched device: its open questions are moot.
    /// Returns whether any were dropped.
    pub fn forget_worktree(&mut self, worktree: &WorktreeKey) -> bool {
        let before = self.pending.len();
        self.pending.retain(|r| &r.worktree != worktree);
        before != self.pending.len()
    }

    pub fn pending(&self) -> &[Request] {
        &self.pending
    }

    fn decided(&self, udid: &DeviceId, now: Instant) -> Option<Verdict> {
        if self.approved.contains(udid) {
            return Some(Verdict::Allowed);
        }
        match self.denied.get(udid) {
            Some(until) if *until > now => Some(Verdict::Denied { retry_after: *until - now }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(s: &str) -> DeviceId {
        DeviceId(s.into())
    }

    fn wt(s: &str) -> WorktreeKey {
        WorktreeKey::from_path(std::path::Path::new(s))
    }

    #[test]
    fn the_first_ask_raises_one_request_and_repeats_do_not() {
        let (mut c, t) = (Consent::default(), Instant::now());
        assert_eq!(c.check(&dev("A"), &wt("/w/one"), "iPhone", t), (Verdict::Pending, true));
        assert_eq!(c.check(&dev("A"), &wt("/w/one"), "iPhone", t), (Verdict::Pending, false));
        assert_eq!(c.pending().len(), 1, "a retry loop must not stack banners");
        // Another worktree asking about the same device is its own question.
        assert_eq!(c.check(&dev("A"), &wt("/w/two"), "iPhone", t), (Verdict::Pending, true));
        assert_eq!(c.pending_for(&wt("/w/two"), &dev("A")).map(|r| r.device_name.as_str()), Some("iPhone"));
    }

    #[test]
    fn an_open_question_is_shown_again_after_a_while() {
        let (mut c, t) = (Consent::default(), Instant::now());
        c.check(&dev("A"), &wt("/w"), "iPhone", t);
        assert!(!c.check(&dev("A"), &wt("/w"), "iPhone", t + REANNOUNCE_AFTER / 2).1);
        assert!(c.check(&dev("A"), &wt("/w"), "iPhone", t + REANNOUNCE_AFTER).1, "a missed toast comes back");
        assert!(!c.check(&dev("A"), &wt("/w"), "iPhone", t + REANNOUNCE_AFTER + Duration::from_secs(1)).1);
    }

    #[test]
    fn a_question_about_a_device_the_worktree_left_is_not_shown() {
        let (mut c, t) = (Consent::default(), Instant::now());
        c.check(&dev("A"), &wt("/w"), "iPhone A", t);
        // The agent switched the worktree to device B.
        assert!(c.pending_for(&wt("/w"), &dev("B")).is_none());
        assert!(c.forget_worktree(&wt("/w")));
        assert!(c.pending_for(&wt("/w"), &dev("A")).is_none());
        assert!(!c.forget_worktree(&wt("/w")));
    }

    #[test]
    fn allow_answers_the_device_for_every_worktree() {
        let (mut c, t) = (Consent::default(), Instant::now());
        c.check(&dev("A"), &wt("/w/one"), "iPhone", t);
        c.check(&dev("A"), &wt("/w/two"), "iPhone", t);
        c.check(&dev("B"), &wt("/w/one"), "iPad", t);
        c.allow(&dev("A"));
        assert_eq!(c.pending().len(), 1, "only the other device's request is left");
        assert_eq!(c.check(&dev("A"), &wt("/w/three"), "iPhone", t), (Verdict::Allowed, false));
        c.revoke(&dev("A"));
        assert_eq!(c.check(&dev("A"), &wt("/w/three"), "iPhone", t).0, Verdict::Pending);
    }

    #[test]
    fn deny_refuses_without_asking_again_until_the_cooldown_ends() {
        let (mut c, t) = (Consent::default(), Instant::now());
        c.check(&dev("A"), &wt("/w/one"), "iPhone", t);
        c.deny(&dev("A"), t);
        assert!(c.pending().is_empty());
        let later = t + Duration::from_secs(60);
        let (verdict, raised) = c.check(&dev("A"), &wt("/w/one"), "iPhone", later);
        assert_eq!(verdict, Verdict::Denied { retry_after: DENY_COOLDOWN - Duration::from_secs(60) });
        assert!(!raised, "a denied device is not asked about again");
        assert!(c.pending().is_empty());
        let after = t + DENY_COOLDOWN;
        assert!(c.expire(after));
        assert_eq!(c.check(&dev("A"), &wt("/w/one"), "iPhone", after), (Verdict::Pending, true));
    }

    #[test]
    fn an_abandoned_request_expires_but_polling_keeps_it() {
        let (mut c, t) = (Consent::default(), Instant::now());
        c.check(&dev("A"), &wt("/w/one"), "iPhone", t);
        // Polled just before it would lapse: still there after the first TTL.
        assert_eq!(c.state(&dev("A"), &wt("/w/one"), t + PENDING_TTL - Duration::from_secs(1)), State::Pending);
        assert!(!c.expire(t + PENDING_TTL));
        // Nobody polls again: gone.
        assert!(c.expire(t + PENDING_TTL * 2));
        assert_eq!(c.state(&dev("A"), &wt("/w/one"), t + PENDING_TTL * 2), State::NotAsked);
    }

    #[test]
    fn status_reports_without_raising() {
        let (mut c, t) = (Consent::new([dev("OK")]), Instant::now());
        assert_eq!(c.state(&dev("OK"), &wt("/w"), t), State::Allowed);
        assert_eq!(c.state(&dev("NEW"), &wt("/w"), t), State::NotAsked);
        assert!(c.pending().is_empty(), "status never asks the user");
    }
}
