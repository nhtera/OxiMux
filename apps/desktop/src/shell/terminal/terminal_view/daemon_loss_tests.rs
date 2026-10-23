//! A session that dies with its daemon is not a program exit: the view marks
//! it lost (so it can be brought back, and a quit keeps its restore hint) and
//! never asks for its tab to be auto-closed.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::TestAppContext;
use oximux_pty::{SpawnConfig, TerminalEvent, TerminalSnapshot};

use super::*;

/// A backend whose one session was lost with its daemon: once `lost` is set,
/// its next drain reports `DaemonLost`, the way the relay backend's inherited
/// sessions do. Held back until then so the test is listening first — the
/// view's own poll task drains too.
struct LostBackend {
    lost: Arc<AtomicBool>,
    reported: bool,
    /// The daemon PTY id it reports. Unique per test that reads the process-
    /// wide lock set, which parallel tests would otherwise share.
    pty: &'static str,
}

impl TerminalBackend for LostBackend {
    fn spawn(&mut self, _cfg: SpawnConfig) -> anyhow::Result<TerminalSessionId> {
        anyhow::bail!("not used")
    }
    fn external_id_of(&self, _id: TerminalSessionId) -> Option<String> {
        Some(self.pty.into())
    }
    fn write(&mut self, _id: TerminalSessionId, _bytes: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _id: TerminalSessionId, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }
    fn snapshot(&self, _id: TerminalSessionId) -> anyhow::Result<TerminalSnapshot> {
        Ok(TerminalSnapshot::empty(80, 24))
    }
    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        Vec::new()
    }
    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        if !self.lost.load(Ordering::SeqCst) || std::mem::replace(&mut self.reported, true) {
            Vec::new()
        } else {
            vec![TerminalEvent::DaemonLost { id }]
        }
    }
    fn close(&mut self, _id: TerminalSessionId) -> anyhow::Result<()> {
        Ok(())
    }
}

#[gpui::test]
async fn a_session_lost_with_its_daemon_is_marked_and_never_clean_exits(cx: &mut TestAppContext) {
    let lost = Arc::new(AtomicBool::new(false));
    let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(LostBackend {
        lost: Arc::clone(&lost),
        reported: false,
        pty: "pty-gone",
    })));
    let window = cx.add_window(|win, cx| {
        TerminalView::mount_background(
            backend,
            TerminalSessionId(1),
            SurfaceIds::restored("/proj", "surface-1".into(), "tab-1".into()),
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    let view = window.root(cx).expect("view");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    cx.update(|cx| {
        cx.subscribe(&view, move |_, event, _| {
            sink.borrow_mut().push(match event {
                TerminalViewEvent::CleanExit { .. } => "clean-exit",
                TerminalViewEvent::DaemonLost { .. } => "daemon-lost",
                TerminalViewEvent::Recovered { .. } => "recovered",
            });
        })
        .detach();
    });

    lost.store(true, Ordering::SeqCst);
    view.update(cx, |v, cx| v.tick(cx));
    cx.run_until_parked();

    view.read_with(cx, |v, _| {
        assert!(v.lost_to_daemon, "marked lost");
        assert_eq!(v.exited, Some(-1), "shown as ended until it is brought back");
        assert_eq!(
            v.relay_id_for_capture().as_deref(),
            Some("pty-gone"),
            "a quit before recovery still restores it from its checkpoint"
        );
    });
    assert_eq!(*seen.borrow(), ["daemon-lost"], "lost, and never a clean exit");
}

// With no relay to bring it back on, a lost shell keeps its "exited" banner —
// never a silent in-process replacement that would look recovered yet die
// with the app. (No shared relay backend is installed in unit tests.)
#[gpui::test]
async fn without_a_relay_a_lost_shell_stays_lost(cx: &mut TestAppContext) {
    let lost = Arc::new(AtomicBool::new(true));
    let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(LostBackend {
        lost,
        reported: false,
        pty: "pty-gone",
    })));
    let window = cx.add_window(|win, cx| {
        TerminalView::mount_background(
            backend,
            TerminalSessionId(1),
            SurfaceIds::restored("/proj", "surface-1".into(), "tab-1".into()),
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    let view = window.root(cx).expect("view");
    view.update(cx, |v, cx| v.tick(cx));

    view.update(cx, |v, cx| v.respawn_after_loss(cx));
    cx.run_until_parked();

    view.read_with(cx, |v, _| {
        assert!(v.is_lost_to_daemon(), "still lost");
        assert_eq!(v.session_id, TerminalSessionId(1), "no replacement session");
        assert!(!v.recovering_from_loss, "free to try again");
    });
}

// A pane whose program had already exited on its own stays exited — with its
// own code — when its daemon later goes: only a running session was lost.
#[gpui::test]
async fn an_already_exited_pane_is_not_marked_lost(cx: &mut TestAppContext) {
    let lost = Arc::new(AtomicBool::new(false));
    let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(LostBackend {
        lost: Arc::clone(&lost),
        reported: false,
        pty: "pty-gone",
    })));
    let window = cx.add_window(|win, cx| {
        TerminalView::mount_background(
            backend,
            TerminalSessionId(1),
            SurfaceIds::restored("/proj", "surface-1".into(), "tab-1".into()),
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    let view = window.root(cx).expect("view");
    view.update(cx, |v, _| v.exited = Some(1));
    lost.store(true, Ordering::SeqCst);

    view.update(cx, |v, cx| v.tick(cx));

    view.read_with(cx, |v, _| {
        assert!(!v.is_lost_to_daemon());
        assert_eq!(v.exited, Some(1), "its own exit code survives");
    });
}

/// A locked terminal restored onto a daemon PTY locks that PTY for remote
/// input as soon as the view adopts it — the id is unknown until then.
#[gpui::test]
async fn adopting_a_session_locks_its_pty_when_the_terminal_is_locked(cx: &mut TestAppContext) {
    use crate::shell::terminal::terminal_locks::{self as locks, pty_is_locked, shared_locked_ptys};
    let (placeholder, placeholder_id) = spawn_local_pty_dormant(80, 24).expect("dormant spawn");
    let window = cx.add_window(|win, cx| {
        TerminalView::mount_pending(
            placeholder,
            placeholder_id,
            SurfaceIds::restored("/proj", "surface-lock".into(), "tab-lock-adopt".into()),
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    locks::set("tab-lock-adopt", None, true);
    assert!(!pty_is_locked(&shared_locked_ptys(), "pty-lock-adopt"));
    let live: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(LostBackend {
        lost: Arc::new(AtomicBool::new(false)),
        reported: false,
        pty: "pty-lock-adopt",
    })));
    window
        .update(cx, |v, _w, cx| assert!(v.adopt_live_session(live, TerminalSessionId(1), cx)))
        .unwrap();
    assert!(pty_is_locked(&shared_locked_ptys(), "pty-lock-adopt"), "the adopted PTY is locked");
    locks::set("tab-lock-adopt", Some("pty-lock-adopt"), false);
}
