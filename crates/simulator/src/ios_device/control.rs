//! Driving a real iPhone through its runner: one queue per phone, shared by
//! the panel and agents, worked by one thread (the runner takes one command
//! at a time, each a second or so).
//!
//! The panel's input arrives as the touch and key stream it sends every
//! device ([`DeviceControl::send`], never blocking): [`InputMap`] turns it
//! into gestures, and queued gestures merge — typed characters into one
//! `type`, scroll drags into one, a second click on the same spot within
//! [`DOUBLE_TAP`] into a double tap (a lone tap waits that long for it).
//! Panel input queued longer than [`STALE`], or behind a runner failure, is
//! dropped: a tap landing on whatever the screen shows ten seconds later is
//! worse than none. Agents queue whole gestures and commands and wait for
//! their reply ([`DeviceControl::call`]).
//!
//! Points: the panel's coordinates are `0..1` of the screen; the runner's
//! are points, from the `viewport` it reports — read again when the screen's
//! shape flips (the phone was turned).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::input_map::{Gesture, InputMap};
use super::runner_client::Reply;
use super::runner_supervisor::{ControlError, RunnerSupervisor};
use super::snapshot;
use crate::ax::AxNode;
use crate::protocol::Command;

/// How long a lone tap waits for a second click.
pub const DOUBLE_TAP: Duration = Duration::from_millis(250);
/// Panel input older than this when its turn comes is dropped.
pub const STALE: Duration = Duration::from_secs(5);
/// The home screen and system dialogs (the default target).
pub const SPRINGBOARD: &str = "com.apple.springboard";
/// The same hint is not repeated sooner than this.
const HINT_EVERY: Duration = Duration::from_secs(3);

/// What runs the runner's commands (the supervisor; a fake in tests).
pub trait Exec: Send + Sync {
    fn call(&self, command: &str, fields: Value) -> Result<Reply, ControlError>;
    /// Shut down after the command in flight.
    fn stop(&self);
    /// End now; the command in flight fails.
    fn abort(&self);
}

impl Exec for RunnerSupervisor {
    fn call(&self, command: &str, fields: Value) -> Result<Reply, ControlError> {
        RunnerSupervisor::call(self, command, fields)
    }

    fn stop(&self) {
        RunnerSupervisor::stop(self);
    }

    fn abort(&self) {
        RunnerSupervisor::abort(self);
    }
}

/// What the panel hears from a phone's control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    /// Panel input failed.
    Error(String),
    /// The runner cannot be used until the user acts (it gave up after a
    /// relaunch, or would not start): control should show this and stop.
    /// `expired`: its provisioning profile ran out (a rebuild cures it).
    Failed { message: String, expired: bool },
    /// Input the phone does not take.
    Hint(&'static str),
    /// A gesture first brought `app` back to the front.
    Reactivated(String),
    /// The queue started (true) or ran dry (false).
    Busy(bool),
}

enum Job {
    Panel { gesture: Gesture, queued: Instant },
    AgentGesture { gesture: Gesture, reply: SyncSender<Result<Reply, ControlError>> },
    Agent { command: String, fields: Value, reply: SyncSender<Result<Reply, ControlError>> },
}

struct State {
    queue: VecDeque<Job>,
    input: InputMap,
    /// The screen in points, and the captured screen's shape it was read
    /// for (landscape or not; `None`: unknown).
    viewport: Option<((f64, f64), Option<bool>)>,
    target: String,
    closed: bool,
    hinted: Option<(&'static str, Instant)>,
}

struct Shared {
    exec: Arc<dyn Exec>,
    state: Mutex<State>,
    wake: Condvar,
    /// The captured screen's size, for its shape.
    frame: Box<dyn Fn() -> Option<(u32, u32)> + Send + Sync>,
    events: Sender<ControlEvent>,
    /// The worker is running a job (so a stop must not wait on it).
    working: std::sync::atomic::AtomicBool,
}

/// One phone's control: its queue and worker.
pub struct DeviceControl {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl DeviceControl {
    pub fn new(exec: Arc<dyn Exec>, frame: impl Fn() -> Option<(u32, u32)> + Send + Sync + 'static, events: Sender<ControlEvent>) -> Self {
        let shared = Arc::new(Shared {
            exec,
            state: Mutex::new(State {
                queue: VecDeque::new(),
                input: InputMap::default(),
                viewport: None,
                target: SPRINGBOARD.into(),
                closed: false,
                hinted: None,
            }),
            wake: Condvar::new(),
            frame: Box::new(frame),
            events,
            working: Default::default(),
        });
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new().name("iphone-control".into()).spawn(move || work(&shared)).ok()
        };
        Self { shared, worker: Mutex::new(worker) }
    }

    /// The app gestures and typing address (default the home screen).
    pub fn target(&self) -> String {
        lock(&self.shared.state).target.clone()
    }

    pub fn set_target(&self, bundle_id: &str) {
        lock(&self.shared.state).target = bundle_id.to_owned();
    }

    /// The panel's input (never blocks).
    pub fn send(&self, command: &Command) {
        let mut state = lock(&self.shared.state);
        match state.input.feed(command, Instant::now()) {
            Ok(Some(gesture)) => {
                push(&mut state, Job::Panel { gesture, queued: Instant::now() });
                self.shared.wake.notify_all();
            }
            Ok(None) => {}
            Err(hint) => {
                let now = Instant::now();
                if !state.hinted.is_some_and(|(h, at)| h == hint && now.duration_since(at) < HINT_EVERY) {
                    state.hinted = Some((hint, now));
                    let _ = self.shared.events.send(ControlEvent::Hint(hint));
                }
            }
        }
    }

    /// Pasted text, typed into whatever has the phone's keyboard focus (in
    /// pieces the runner takes).
    pub fn type_text(&self, text: &str) {
        let chars: Vec<char> = text.chars().filter(|c| *c != '\r').collect();
        let mut state = lock(&self.shared.state);
        for piece in chars.chunks(super::input_map::MAX_TEXT) {
            push(&mut state, Job::Panel { gesture: Gesture::Text(piece.iter().collect()), queued: Instant::now() });
        }
        drop(state);
        self.shared.wake.notify_all();
    }

    /// The panel's toolbar button (queued like a gesture).
    pub fn press(&self, button: super::input_map::RunnerButton) {
        let mut state = lock(&self.shared.state);
        push(&mut state, Job::Panel { gesture: Gesture::Button(button), queued: Instant::now() });
        self.shared.wake.notify_all();
    }

    /// An agent's gesture, in its turn; its reply.
    pub fn gesture(&self, gesture: Gesture) -> Result<Reply, ControlError> {
        self.wait(|reply| Job::AgentGesture { gesture, reply })
    }

    /// An agent's runner command, in its turn; its reply.
    pub fn call(&self, command: &str, fields: Value) -> Result<Reply, ControlError> {
        let command = command.to_owned();
        self.wait(|reply| Job::Agent { command, fields, reply })
    }

    /// The target app's accessibility tree.
    pub fn describe(&self) -> Result<Vec<AxNode>, ControlError> {
        let reply = self.call("snapshot", json!({"app": self.target()}))?;
        snapshot::tree(&reply.data).map(|(nodes, _)| nodes).map_err(|e| ControlError::Runner(e.to_string()))
    }

    /// Ends the worker and the runner: politely when idle, at once when a
    /// command is in flight (a long paste is not waited for). Queued agent
    /// calls fail.
    pub fn stop(&self) {
        self.close();
        if self.shared.working.load(std::sync::atomic::Ordering::Acquire) {
            self.shared.exec.abort();
        }
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
        self.shared.exec.stop();
    }

    /// Ends everything now, without waiting (quit).
    pub fn abort(&self) {
        self.close();
        self.shared.exec.abort();
    }

    fn close(&self) {
        let mut state = lock(&self.shared.state);
        state.closed = true;
        fail_all(&mut state);
        drop(state);
        self.shared.wake.notify_all();
    }

    fn wait(&self, job: impl FnOnce(SyncSender<Result<Reply, ControlError>>) -> Job) -> Result<Reply, ControlError> {
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut state = lock(&self.shared.state);
            if state.closed {
                return Err(ControlError::Cancelled);
            }
            state.queue.push_back(job(tx));
        }
        self.shared.wake.notify_all();
        rx.recv().unwrap_or(Err(ControlError::Cancelled))
    }
}

impl Drop for DeviceControl {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.closed = true;
        drop(state);
        self.shared.wake.notify_all();
    }
}

/// Queues `job`, folding panel input into the panel job before it.
fn push(state: &mut State, job: Job) {
    let job = match (state.queue.back_mut(), job) {
        (Some(Job::Panel { gesture: last, queued: at }), Job::Panel { gesture, queued }) => {
            // A second click counts as a double tap only when it came soon.
            let soon = !matches!(gesture, Gesture::Tap { .. }) || at.elapsed() < DOUBLE_TAP;
            match if soon { last.merge(gesture) } else { Some(gesture) } {
                None => return,
                Some(gesture) => Job::Panel { gesture, queued },
            }
        }
        (_, job) => job,
    };
    state.queue.push_back(job);
}

/// The worker: one job at a time, until the control closes.
fn work(shared: &Shared) {
    let mut busy = false;
    loop {
        let job = {
            let mut state = lock(&shared.state);
            loop {
                if state.closed {
                    fail_all(&mut state);
                    return;
                }
                // Typing is not aimed at a spot: a long paste's later pieces
                // wait their turn.
                while let Some(Job::Panel { queued, gesture }) = state.queue.front()
                    && !matches!(gesture, Gesture::Text(_))
                    && queued.elapsed() > STALE
                {
                    state.queue.pop_front();
                }
                match state.queue.front() {
                    None => {
                        if busy {
                            busy = false;
                            let _ = shared.events.send(ControlEvent::Busy(false));
                        }
                        state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    // A lone tap waits a moment for a second click.
                    Some(Job::Panel { gesture: Gesture::Tap { taps: 1, .. }, queued }) if state.queue.len() == 1 && queued.elapsed() < DOUBLE_TAP => {
                        let left = DOUBLE_TAP.saturating_sub(queued.elapsed());
                        state = shared.wake.wait_timeout(state, left).map(|(s, _)| s).unwrap_or_else(|e| e.into_inner().0);
                    }
                    Some(_) => break state.queue.pop_front(),
                }
            }
        };
        let Some(job) = job else { continue };
        if !busy {
            busy = true;
            let _ = shared.events.send(ControlEvent::Busy(true));
        }
        shared.working.store(true, std::sync::atomic::Ordering::Release);
        run(shared, job);
        shared.working.store(false, std::sync::atomic::Ordering::Release);
    }
}

fn run(shared: &Shared, job: Job) {
    match job {
        Job::Panel { gesture, .. } => match perform(shared, &gesture) {
            Ok(reply) if reply.reactivated => {
                let _ = shared.events.send(ControlEvent::Reactivated(lock(&shared.state).target.clone()));
            }
            Ok(_) => {}
            Err(error) => {
                // What was queued behind it was aimed at a screen that may
                // not be there now.
                lock(&shared.state).queue.retain(|j| !matches!(j, Job::Panel { .. }));
                if !report_failure(shared, &error) {
                    let _ = shared.events.send(ControlEvent::Error(error.to_string()));
                }
            }
        },
        Job::AgentGesture { gesture, reply } => {
            let result = perform(shared, &gesture);
            if let Err(error) = &result {
                report_failure(shared, error);
            }
            let _ = reply.send(result);
        }
        Job::Agent { command, fields, reply } => {
            let result = shared.exec.call(&command, fields);
            if let Err(error) = &result {
                report_failure(shared, error);
            }
            let _ = reply.send(result);
        }
    }
}

/// Says so when `error` leaves the runner unusable until the user acts;
/// whether it did.
fn report_failure(shared: &Shared, error: &ControlError) -> bool {
    let unusable = matches!(
        error,
        ControlError::GaveUp(_)
            | ControlError::ProfileExpired
            | ControlError::NotTrusted
            | ControlError::DeveloperModeOff
            | ControlError::Busy
            | ControlError::LaunchFailed(_)
    );
    if unusable {
        let expired = matches!(error, ControlError::ProfileExpired);
        let _ = shared.events.send(ControlEvent::Failed { message: error.to_string(), expired });
    }
    unusable
}

/// `gesture` as a runner command, in points.
fn perform(shared: &Shared, gesture: &Gesture) -> Result<Reply, ControlError> {
    let app = lock(&shared.state).target.clone();
    let needs_points = matches!(gesture, Gesture::Tap { .. } | Gesture::LongPress { .. } | Gesture::Drag { .. });
    let (w, h) = if needs_points { viewport(shared)? } else { (0.0, 0.0) };
    let pt = |(x, y): (f64, f64)| json!({"x": round(x * w), "y": round(y * h)});
    let (command, fields) = match gesture {
        Gesture::Tap { at, taps } => ("tap", json!({"app": app, "x": round(at.0 * w), "y": round(at.1 * h), "taps": taps})),
        Gesture::LongPress { at, ms } => ("longPress", json!({"app": app, "x": round(at.0 * w), "y": round(at.1 * h), "durationMs": ms})),
        Gesture::Drag { from, to, ms, hold, settle } => {
            ("drag", json!({"app": app, "from": pt(*from), "to": pt(*to), "durationMs": ms, "holdMs": hold, "settle": settle}))
        }
        Gesture::Text(text) => ("type", json!({"app": app, "text": text})),
        Gesture::Return => ("keyboardReturn", json!({"app": app})),
        Gesture::Delete(count) => ("keyboardDelete", json!({"app": app, "count": count})),
        Gesture::Button(button) => ("button", json!({"name": button.wire_name()})),
    };
    shared.exec.call(command, fields)
}

/// The screen in points: the runner's `viewport` of the home screen (an
/// iPhone app fills the screen), read again when the captured screen's
/// shape no longer matches it.
fn viewport(shared: &Shared) -> Result<(f64, f64), ControlError> {
    let landscape = (shared.frame)().map(|(w, h)| w > h);
    if let Some((size, shape)) = lock(&shared.state).viewport
        && (shape == landscape || landscape.is_none())
    {
        return Ok(size);
    }
    let reply = shared.exec.call("viewport", json!({"app": SPRINGBOARD}))?;
    let size = |k: &str| reply.data.get(k).and_then(Value::as_f64).filter(|v| *v > 0.0);
    let (Some(w), Some(h)) = (size("width"), size("height")) else {
        return Err(ControlError::Runner("the runner reported no screen size".into()));
    };
    lock(&shared.state).viewport = Some(((w, h), landscape));
    Ok((w, h))
}

fn round(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn fail_all(state: &mut State) {
    for job in state.queue.drain(..) {
        match job {
            Job::AgentGesture { reply, .. } | Job::Agent { reply, .. } => {
                let _ = reply.send(Err(ControlError::Cancelled));
            }
            Job::Panel { .. } => {}
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::protocol::{KeyPhase, TouchPhase};

    /// Records every runner command; each takes `delay`.
    struct Fake {
        calls: Mutex<Vec<(String, Value)>>,
        delay: Duration,
        viewport: Mutex<(f64, f64)>,
        fail: AtomicBool,
        stopped: AtomicBool,
        aborted: AtomicBool,
    }

    impl Exec for Fake {
        fn call(&self, command: &str, fields: Value) -> Result<Reply, ControlError> {
            std::thread::sleep(self.delay);
            self.calls.lock().unwrap().push((command.to_owned(), fields));
            if command == "viewport" {
                let (w, h) = *self.viewport.lock().unwrap();
                return Ok(Reply { data: json!({"width": w, "height": h}), reactivated: false });
            }
            if self.fail.load(Ordering::Relaxed) {
                return Err(ControlError::Runner("the runner is gone".into()));
            }
            Ok(Reply { data: json!({}), reactivated: command == "tap" })
        }

        fn stop(&self) {
            self.stopped.store(true, Ordering::Relaxed);
        }

        fn abort(&self) {
            self.aborted.store(true, Ordering::Relaxed);
        }
    }

    fn fake(delay_ms: u64) -> Arc<Fake> {
        Arc::new(Fake {
            calls: Mutex::new(Vec::new()),
            delay: Duration::from_millis(delay_ms),
            viewport: Mutex::new((430.0, 932.0)),
            fail: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
        })
    }

    fn control(exec: Arc<Fake>, frame: (u32, u32)) -> (DeviceControl, mpsc::Receiver<ControlEvent>) {
        let (tx, rx) = mpsc::channel();
        let frame = Arc::new(Mutex::new(frame));
        (DeviceControl::new(exec, move || Some(*frame.lock().unwrap()), tx), rx)
    }

    fn click(control: &DeviceControl, x: f64, y: f64) {
        control.send(&Command::Touch { phase: TouchPhase::Begin, x, y, edge: 0 });
        control.send(&Command::Touch { phase: TouchPhase::End, x, y, edge: 0 });
    }

    fn settle(exec: &Fake, count: usize) -> Vec<(String, Value)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while exec.calls.lock().unwrap().len() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        exec.calls.lock().unwrap().clone()
    }

    #[test]
    fn a_click_is_a_tap_in_points_and_says_it_brought_the_app_back() {
        let exec = fake(0);
        let (control, events) = control(exec.clone(), (1290, 2796));
        click(&control, 0.5, 0.25);
        let calls = settle(&exec, 2);
        assert_eq!(calls[0].0, "viewport");
        assert_eq!(calls[1], ("tap".into(), json!({"app": SPRINGBOARD, "x": 215.0, "y": 233.0, "taps": 1})));
        let events: Vec<ControlEvent> = events.try_iter().collect();
        assert!(events.contains(&ControlEvent::Reactivated(SPRINGBOARD.into())), "{events:?}");
        assert!(events.contains(&ControlEvent::Busy(true)));
        control.stop();
        assert!(exec.stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn a_quick_second_click_is_a_double_tap() {
        let exec = fake(0);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        click(&control, 0.5, 0.5);
        click(&control, 0.502, 0.5);
        let calls = settle(&exec, 2);
        let taps: Vec<&Value> = calls.iter().filter(|(c, _)| c == "tap").map(|(_, f)| &f["taps"]).collect();
        assert_eq!(taps, [&json!(2)]);
        control.stop();
    }

    #[test]
    fn typing_while_busy_is_one_type_command() {
        let exec = fake(150);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        // A button keeps the runner busy while the keys queue.
        control.press(super::super::input_map::RunnerButton::Home);
        std::thread::sleep(Duration::from_millis(30));
        for usage in [0x0b, 0x0c] {
            control.send(&Command::Key { phase: KeyPhase::Down, usage });
            control.send(&Command::Key { phase: KeyPhase::Up, usage });
        }
        control.send(&Command::Key { phase: KeyPhase::Down, usage: 0x28 });
        let calls = settle(&exec, 3);
        let names: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, ["button", "type", "keyboardReturn"]);
        assert_eq!(calls[1].1["text"], "hi");
        control.stop();
    }

    #[test]
    fn a_turned_phone_is_measured_again() {
        let exec = fake(0);
        let frame = Arc::new(Mutex::new((1290u32, 2796u32)));
        let (tx, _rx) = mpsc::channel();
        let shape = frame.clone();
        let control = DeviceControl::new(exec.clone(), move || Some(*shape.lock().unwrap()), tx);
        click(&control, 0.5, 0.5);
        settle(&exec, 2);
        *frame.lock().unwrap() = (2796, 1290);
        *exec.viewport.lock().unwrap() = (932.0, 430.0);
        click(&control, 0.5, 0.5);
        let calls = settle(&exec, 4);
        let viewports = calls.iter().filter(|(c, _)| c == "viewport").count();
        assert_eq!(viewports, 2);
        assert_eq!(calls.last().unwrap().1["x"], 466.0);
        control.stop();
    }

    #[test]
    fn panel_input_behind_a_failure_is_dropped_and_the_failure_said() {
        let exec = fake(100);
        exec.fail.store(true, Ordering::Relaxed);
        let (control, events) = control(exec.clone(), (1290, 2796));
        control.press(super::super::input_map::RunnerButton::Home);
        std::thread::sleep(Duration::from_millis(20));
        control.press(super::super::input_map::RunnerButton::VolumeUp);
        control.send(&Command::Key { phase: KeyPhase::Down, usage: 0x04 });
        let calls = settle(&exec, 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(exec.calls.lock().unwrap().len(), 1, "{calls:?}");
        assert!(events.try_iter().any(|e| matches!(e, ControlEvent::Error(m) if m.contains("gone"))));
        control.stop();
    }

    #[test]
    fn agents_wait_their_turn_and_get_their_reply() {
        let exec = fake(50);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        control.press(super::super::input_map::RunnerButton::Home);
        let reply = control.gesture(Gesture::Tap { at: (0.1, 0.1), taps: 1 }).unwrap();
        assert!(reply.reactivated);
        let names: Vec<String> = exec.calls.lock().unwrap().iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(names, ["button", "viewport", "tap"]);
        control.set_target("com.example.app");
        control.call("snapshot", json!({"app": control.target()})).unwrap();
        assert_eq!(exec.calls.lock().unwrap().last().unwrap().1["app"], "com.example.app");
        control.stop();
        assert!(matches!(control.call("viewport", json!({})), Err(ControlError::Cancelled)));
    }

    #[test]
    fn a_long_paste_is_typed_in_pieces_the_runner_takes() {
        let exec = fake(0);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        control.type_text(&"é".repeat(super::super::input_map::MAX_TEXT + 10));
        let calls = settle(&exec, 2);
        let lengths: Vec<usize> = calls.iter().map(|(_, f)| f["text"].as_str().unwrap().chars().count()).collect();
        assert_eq!(lengths, [super::super::input_map::MAX_TEXT, 10]);
        control.stop();
    }

    #[test]
    fn hints_are_said_once_in_a_while() {
        let exec = fake(0);
        let (control, events) = control(exec, (1290, 2796));
        for _ in 0..5 {
            control.send(&Command::Key { phase: KeyPhase::Down, usage: 0x4f });
        }
        let hints = events.try_iter().filter(|e| matches!(e, ControlEvent::Hint(_))).count();
        assert_eq!(hints, 1);
        control.stop();
    }

    #[test]
    fn a_held_tap_goes_at_once_when_an_agent_queues_behind_it() {
        let exec = fake(10);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        // Queue a panel tap (will wait DOUBLE_TAP for a second click)
        click(&control, 0.5, 0.5);
        // Immediately queue an agent gesture; it should not wait for double-tap
        let start = Instant::now();
        let _ = control.gesture(Gesture::Tap { at: (0.1, 0.1), taps: 1 });
        let elapsed = start.elapsed();
        // Well under the 250 ms a lone tap waits: the tap went at once.
        assert!(elapsed < Duration::from_millis(240), "took {elapsed:?}, agent should bypass double-tap");
        let calls = settle(&exec, 3);
        // viewport, two taps
        assert_eq!(calls.iter().filter(|(c, _)| c == "tap").count(), 2);
        control.stop();
    }

    #[test]
    fn stop_does_not_wait_for_a_command_in_flight() {
        let exec = fake(400);
        let (busy, _events) = control(exec.clone(), (1290, 2796));
        busy.type_text("a long paste");
        std::thread::sleep(Duration::from_millis(50));
        busy.stop();
        assert!(exec.aborted.load(Ordering::Relaxed), "a busy runner is aborted, not waited for");
        // Idle: a polite stop, no abort.
        let exec = fake(0);
        let (idle, _events) = control(exec.clone(), (1290, 2796));
        idle.stop();
        assert!(!exec.aborted.load(Ordering::Relaxed) && exec.stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn a_runner_that_gave_up_is_reported_as_failed_not_as_a_toast() {
        struct GivingUp;
        impl Exec for GivingUp {
            fn call(&self, _: &str, _: Value) -> Result<Reply, ControlError> {
                Err(ControlError::GaveUp("the runner stopped listening".into()))
            }
            fn stop(&self) {}
            fn abort(&self) {}
        }
        let (tx, events) = mpsc::channel();
        let control = DeviceControl::new(Arc::new(GivingUp), || Some((1290, 2796)), tx);
        control.press(super::super::input_map::RunnerButton::Home);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline && !seen.iter().any(|e| matches!(e, ControlEvent::Failed { .. })) {
            seen.extend(events.try_iter());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen.iter().any(|e| matches!(e, ControlEvent::Failed { expired: false, .. })), "{seen:?}");
        assert!(!seen.iter().any(|e| matches!(e, ControlEvent::Error(_))), "{seen:?}");
        control.stop();
    }

    #[test]
    fn a_long_paste_is_not_dropped_as_stale() {
        let exec = fake(0);
        let (control, _events) = control(exec.clone(), (1290, 2796));
        // Its second piece waits behind the first longer than STALE.
        lock(&control.shared.state).queue.push_back(Job::Panel { gesture: Gesture::Text("late".into()), queued: Instant::now() - STALE * 2 });
        lock(&control.shared.state).queue.push_back(Job::Panel { gesture: Gesture::Return, queued: Instant::now() - STALE * 2 });
        control.shared.wake.notify_all();
        let calls = settle(&exec, 1);
        let names: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, ["type"], "the text is typed, the stale Return dropped");
        control.stop();
    }
}
