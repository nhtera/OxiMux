//! Accessibility reads, one at a time per device, and the agents' `@eN` ref
//! tables.
//!
//! A read is slow where it matters most — an Android `uiautomator` dump takes
//! a second or two, a real iPhone's runner works one job at a time — and the
//! verify loop reads often (settle, `wait`, `assert`, `ax`). So a device has at
//! most one read in flight, and everyone who asks while it runs awaits that
//! same read ([`Coalescer`]) — unless they need one started after a moment of
//! their own (an action's settle must not get a tree from before its tap):
//! theirs then starts once the one in flight is done, never beside it.
//!
//! The ref tables live here for the app's lifetime, beside the device they
//! number, and go only when their worktree detaches or changes device (see
//! [`oximux_simulator::ax_refs`]).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::future::{LocalBoxFuture, Shared};
use gpui::Context;
use oximux_simulator::DeviceId;
use oximux_simulator::ax::AxNode;
use oximux_simulator::ax_refs::{RefTable, TableKey};
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::session::StreamSession;

use super::SimulatorHub;

/// One read's outcome, shared by everyone who awaited it.
pub(crate) type Tree = Result<Arc<Vec<AxNode>>, String>;
pub(crate) type SharedRead<T> = Shared<LocalBoxFuture<'static, T>>;

/// At most one read in flight per key; callers who ask meanwhile share it.
pub(crate) struct Coalescer<K, T: Clone> {
    inflight: HashMap<K, (Instant, SharedRead<T>)>,
}

impl<K, T: Clone> Default for Coalescer<K, T> {
    fn default() -> Self {
        Self { inflight: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone, T: Clone + 'static> Coalescer<K, T> {
    /// The read in flight for `key` — if it started no earlier than
    /// `not_before` — else a new one from `start`, begun once any read still
    /// in flight is done.
    pub(crate) fn join(
        &mut self,
        key: &K,
        now: Instant,
        not_before: Option<Instant>,
        start: impl FnOnce() -> LocalBoxFuture<'static, T> + 'static,
    ) -> SharedRead<T> {
        let busy = self.inflight.get(key).filter(|(_, read)| read.peek().is_none());
        if let Some((started, read)) = busy
            && not_before.is_none_or(|at| *started >= at)
        {
            return read.clone();
        }
        let read = match busy {
            Some((_, ahead)) => {
                let ahead = ahead.clone();
                async move {
                    ahead.await;
                    start().await
                }
                .boxed_local()
            }
            None => start(),
        }
        .shared();
        self.inflight.insert(key.clone(), (now, read.clone()));
        read
    }
}

impl SimulatorHub {
    /// `udid`'s accessibility tree from `session`, joining a read already in
    /// flight that started no earlier than `not_before`.
    pub(crate) fn read_tree(
        &mut self,
        udid: &DeviceId,
        session: &StreamSession,
        not_before: Option<Instant>,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> SharedRead<Tree> {
        let (session, executor) = (session.clone(), cx.background_executor().clone());
        self.snapshots.join(udid, Instant::now(), not_before, move || {
            let read = executor.spawn(async move { session.describe(timeout).map(Arc::new).map_err(|e| e.to_string()) });
            read.boxed_local()
        })
    }

    /// The ref table of `key`'s caller on its worktree's device.
    pub(crate) fn ref_table(&mut self, key: &TableKey) -> &mut RefTable {
        self.refs.table(key)
    }

    /// A worktree's refs are void once it detaches or changes device.
    pub(super) fn forget_refs(&mut self, worktree: &WorktreeKey) {
        self.refs.forget_worktree(worktree);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A read that counts its starts and finishes when `gate` opens.
    fn counted(starts: &Rc<Cell<u32>>, gate: futures::channel::oneshot::Receiver<u32>) -> impl FnOnce() -> LocalBoxFuture<'static, u32> + 'static {
        let starts = starts.clone();
        move || {
            starts.set(starts.get() + 1);
            async move { gate.await.unwrap_or(0) }.boxed_local()
        }
    }

    #[test]
    fn concurrent_callers_share_one_read() {
        let mut c: Coalescer<&str, u32> = Coalescer::default();
        let starts = Rc::new(Cell::new(0));
        let t0 = Instant::now();
        let (open, gate) = futures::channel::oneshot::channel();
        let a = c.join(&"sim", t0, None, counted(&starts, gate));
        let (_, unused) = futures::channel::oneshot::channel();
        let b = c.join(&"sim", t0 + Duration::from_millis(5), Some(t0), counted(&starts, unused));
        assert_eq!(starts.get(), 1, "the second caller joined the first read");
        open.send(7).unwrap();
        assert_eq!(futures::executor::block_on(futures::future::join(a, b)), (7, 7));
        // Done: the next caller gets a read of its own.
        let (open, gate) = futures::channel::oneshot::channel();
        let c2 = c.join(&"sim", t0 + Duration::from_millis(10), None, counted(&starts, gate));
        assert_eq!(starts.get(), 2);
        open.send(8).unwrap();
        assert_eq!(futures::executor::block_on(c2), 8);
    }

    #[test]
    fn a_read_from_before_the_callers_moment_is_not_joined() {
        let mut c: Coalescer<&str, u32> = Coalescer::default();
        let starts = Rc::new(Cell::new(0));
        let t0 = Instant::now();
        let (old_open, gate) = futures::channel::oneshot::channel::<u32>();
        let old = c.join(&"sim", t0, None, counted(&starts, gate));
        // A settle after its tap needs a read that started after the tap —
        // queued behind the one in flight, never beside it.
        let (open, gate) = futures::channel::oneshot::channel();
        let fresh = c.join(&"sim", t0 + Duration::from_millis(50), Some(t0 + Duration::from_millis(40)), counted(&starts, gate));
        assert_eq!(starts.get(), 1, "not started while the old read runs");
        // Another device never shares, and never waits.
        let (_k, gate) = futures::channel::oneshot::channel::<u32>();
        let _other = c.join(&"avd", t0, None, counted(&starts, gate));
        assert_eq!(starts.get(), 2);
        old_open.send(9).unwrap();
        open.send(1).unwrap();
        assert_eq!(futures::executor::block_on(futures::future::join(old, fresh)), (9, 1));
        assert_eq!(starts.get(), 3, "the fresh read started after the old one");
    }
}
