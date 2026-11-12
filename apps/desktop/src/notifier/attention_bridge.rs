//! The one thing that turns attention into a banner.
//!
//! Two paths used to post agent banners: a per-tab status watcher for cockpit
//! agents (deduped by a 30-second window per tab and kind) and a view event
//! for chats (one outstanding banner per tab until refocus). Each had its own
//! idea of what deserved a person and its own idea of what counted as the
//! same alert twice. With the host's attention stream arriving as a third
//! answer, the three would have fired next to each other.
//!
//! So this subscribes to the stream and owns the rules, and the two old paths
//! are gone. What decides and what dedupes are now the same thing.
//!
//! **Dedupe is by edge identity, never by elapsed time.** The host gives every
//! alert an edge: the provider's request id for a permission, the turn for a
//! finish, the episode for a stall, `(key, since)` for a published item. One
//! edge banners at most once, ever. A health re-snapshot, a tab reopening, a
//! reconnect or a rekey re-derives the same edge and says nothing — where a
//! time window could only guess, and guessed wrong in both directions: it
//! swallowed a *second* genuine request inside 30 seconds and let the same one
//! through at 31. A different request on the same session is a different edge:
//! it banners, replacing that session's previous banner in place.
//!
//! **A child's alert belongs to its root.** A run with four children that all
//! need permission is one banner that says so, not four. The rail still shows
//! every child; this is only about what the OS pops up.
//!
//! **Boot is not an edge.** Everything already needing a person when the host
//! starts is recorded as announced without announcing it, so a restart never
//! replays yesterday's banners. The badge still counts them, because that is
//! the number, not a notification.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use oximux_agents::attention::{Attention, AttentionEdge, AttentionKind};

use crate::notifier::{NotificationKind, NotificationRequest, NotificationSource};

/// How many past edges one target remembers having announced.
///
/// Bounds the memory a long-lived session can accumulate. The set only has to
/// outlive the alerts that are still on screen or still outstanding, and
/// anything older than 64 of them has been superseded many times over.
const ANNOUNCED_MEMORY: usize = 64;

/// What the bridge wants done about one change.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeAction {
    /// Post (or replace in place) the banner under `replace_key`.
    Post(Post),
    /// Take back the banner under this key: the thing it was about is over.
    Retire { replace_key: String },
}

/// A banner to put on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    /// The stable identifier: posting again under it replaces the delivered
    /// banner rather than stacking a second one.
    pub replace_key: String,
    /// Whose alert this is, as a namespaced target — what the click goes to,
    /// and what the caller re-reads to check the alert is still current.
    pub target: String,
    pub kind: NotificationKind,
    /// The line under the title. Empty is allowed; the notifier has a default.
    pub body: String,
    /// No sound: an update to a banner the person has already heard.
    pub quiet: bool,
    /// Whether this is a root's coalesced child count rather than a session's
    /// own alert. The click goes somewhere different, and the title is about
    /// the root.
    pub root: bool,
}

/// One change off the host's stream, with the little the stream cannot know.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The namespaced target (`chat:…`, `cockpit:…`, `team:…`, `plan:…`).
    pub target: String,
    /// What it wants now, or `None` when it wants nothing.
    pub attention: Option<Attention>,
    /// The edge that is current, if any.
    pub edge: Option<AttentionEdge>,
    /// The edge that just left, when a different one replaced it.
    pub retired: Option<AttentionEdge>,
    /// The root this session hangs under, when it is somebody's child.
    pub root: Option<String>,
}

/// Everything the bridge remembers. Pure: it decides, and the caller does.
///
/// Separated from the subscription so the rules can be tested without a host,
/// a notifier or a window — they are the part that is easy to get wrong.
#[derive(Default)]
pub struct BridgeState {
    /// The edges each target has already announced, oldest first.
    announced: HashMap<String, VecDeque<String>>,
    /// Children currently waiting on a person, per root, in arrival order,
    /// each with what it is waiting for. The body counts these, the worst
    /// kind among them decides the banner's title and which of the person's
    /// per-kind toggles gates it, and the banner retires when a root's list
    /// empties.
    waiting: HashMap<String, Vec<(String, AttentionKind)>>,
    /// Whether the first snapshot has been taken in.
    seeded: bool,
    /// A number per target, for the banner identifier.
    ///
    /// The identifier has to be a short string the OS hands back on a click,
    /// and [`ClickTarget`](crate::notifier::ClickTarget) is `Copy` because one
    /// click is re-sent to every open window. So the target's own namespaced
    /// key does not travel; a number does, and this is where it is resolved
    /// back. Minted in order, never reused, so a stale click cannot land on
    /// whatever session happens to hold a recycled number.
    handles: HashMap<String, u64>,
    targets: HashMap<u64, String>,
    next_handle: u64,
}

impl BridgeState {
    pub fn new() -> Self {
        // Handles start somewhere unpredictable rather than at zero. macOS
        // keeps delivered banners across a restart, so a handle counted from
        // zero would be minted again next run and an old `session:7` would
        // resolve to whichever session happened to be seventh this time — a
        // click opening the wrong conversation. From a random base a stale
        // handle resolves to nothing, and a click that opens nothing is what
        // the router already does for a banner whose row has gone.
        //
        // Not the clock: two bases drawn in the same millisecond would be the
        // same base, and this run's handles have to be this run's.
        Self { next_handle: random_base(), ..Self::default() }
    }

    /// Every target the bridge has announced something for. What a re-read
    /// has to revisit, because these are the ones that may have something on
    /// screen to take back.
    ///
    /// The children counted under a root are in here too, and by
    /// construction: the raise that counts a child is the same one that
    /// records its edge, so one map answers for both rather than two that
    /// could drift.
    pub fn tracked(&self) -> Vec<String> {
        self.announced.keys().cloned().collect()
    }

    /// Record everything already needing a person as announced, announcing
    /// nothing.
    ///
    /// Called once with the host's first snapshot. Without it, every alert
    /// outstanding at launch would banner at launch — the restart replay the
    /// stored look exists to prevent, arriving through the other door.
    pub fn seed(&mut self, snapshot: impl IntoIterator<Item = (String, AttentionEdge)>) {
        for (target, edge) in snapshot {
            self.remember(&target, &edge.id);
        }
        self.seeded = true;
    }

    /// Whether the first snapshot has been taken in. A change arriving before
    /// it is still handled; this only says whether the seeding has happened.
    pub fn seeded(&self) -> bool {
        self.seeded
    }

    /// What to do about `change`.
    ///
    /// Returns at most two actions, and in order: a root's count is updated
    /// after the child's own retirement, so a banner never briefly shows a
    /// count that includes an alert already taken back.
    pub fn on_change(&mut self, change: &Change) -> Vec<BridgeAction> {
        match (&change.attention, &change.edge) {
            (Some(attention), Some(edge)) => self.on_raise(change, attention, edge),
            // Nothing wanted: whatever this target had on screen is over.
            _ => self.on_quiet(change),
        }
    }

    fn on_raise(
        &mut self,
        change: &Change,
        attention: &Attention,
        edge: &AttentionEdge,
    ) -> Vec<BridgeAction> {
        // A child's alert is its root's banner, never its own.
        if let Some(root) = &change.root {
            if !attention.kind.is_needs_you() {
                // A child that finished, or one being rate-limited, is not
                // something to interrupt a person for: the root's banner is
                // about who needs them, and the rail shows the rest. Phase 5
                // revisits this when it nests the rows.
                return self.leave_waiting(root, &change.target);
            }
            let first = self.note_waiting(root, &change.target, attention.kind);
            // An edge already announced for this child does not re-announce,
            // but it does keep the count honest.
            let fresh = self.remember(&change.target, &edge.id);
            return match (fresh, first) {
                // Neither new nor the first: the same child re-deriving the
                // same edge, which is not news and not a change in the count.
                (false, false) => Vec::new(),
                // Another agent needing a person *is* news, first or fourth,
                // so it sounds. Only a count coming down is silent.
                _ => vec![BridgeAction::Post(self.root_post(root, false))],
            };
        }
        // Its own alert. A re-derived edge is not news.
        if !self.remember(&change.target, &edge.id) {
            return Vec::new();
        }
        let replace_key = self.session_key(&change.target);
        vec![BridgeAction::Post(Post {
            replace_key,
            target: change.target.clone(),
            kind: NotificationKind::from_attention(attention.kind),
            body: attention.reason.clone().unwrap_or_default(),
            quiet: false,
            root: false,
        })]
    }

    fn on_quiet(&mut self, change: &Change) -> Vec<BridgeAction> {
        match &change.root {
            // The child stops waiting; the root says so with a lower count.
            Some(root) => {
                let root = root.clone();
                self.leave_waiting(&root, &change.target)
            }
            None => {
                vec![BridgeAction::Retire { replace_key: self.session_key(&change.target) }]
            }
        }
    }

    /// Drop `child` from its root's waiting list and say what that does to the
    /// root's banner.
    fn leave_waiting(&mut self, root: &str, child: &str) -> Vec<BridgeAction> {
        let Some(waiting) = self.waiting.get_mut(root) else {
            return Vec::new();
        };
        let before = waiting.len();
        waiting.retain(|(c, _)| c != child);
        if waiting.len() == before {
            return Vec::new();
        }
        if waiting.is_empty() {
            self.waiting.remove(root);
            return vec![BridgeAction::Retire { replace_key: self.root_key(root) }];
        }
        // Quietly: a count going down is not news, and the person has already
        // heard this banner.
        vec![BridgeAction::Post(self.root_post(root, true))]
    }

    /// Add `child` to its root's waiting list, or update what it waits for.
    /// True when the list was empty, which is the banner's first post rather
    /// than an update.
    fn note_waiting(&mut self, root: &str, child: &str, kind: AttentionKind) -> bool {
        let waiting = self.waiting.entry(root.to_string()).or_default();
        let first = waiting.is_empty();
        match waiting.iter_mut().find(|(c, _)| c == child) {
            Some(known) => known.1 = kind,
            None => waiting.push((child.to_string(), kind)),
        }
        first
    }

    /// The root's banner as it stands now.
    ///
    /// The title and the gating come from the *worst* thing any of its
    /// children is waiting for, so a permission under a root is gated by the
    /// person's permission toggle rather than by a kind none of the children
    /// is in. With one child waiting the banner reads exactly as that child's
    /// own would have.
    fn root_post(&mut self, root: &str, quiet: bool) -> Post {
        let waiting = self.waiting.get(root).map(Vec::as_slice).unwrap_or_default();
        let worst = waiting
            .iter()
            .map(|(_, kind)| *kind)
            .min_by_key(|kind| kind.priority())
            .unwrap_or(AttentionKind::NeedsPermission);
        let count = waiting.len();
        let replace_key = self.root_key(root);
        Post {
            replace_key,
            target: root.to_string(),
            kind: NotificationKind::from_attention(worst),
            body: waiting_body(count),
            quiet,
            root: true,
        }
    }

    /// Record `edge` as announced for `target`. False when it already was,
    /// which is the whole of the dedupe.
    fn remember(&mut self, target: &str, edge_id: &str) -> bool {
        let announced = self.announced.entry(target.to_string()).or_default();
        if announced.iter().any(|known| known == edge_id) {
            return false;
        }
        announced.push_back(edge_id.to_string());
        while announced.len() > ANNOUNCED_MEMORY {
            announced.pop_front();
        }
        true
    }

    /// The child a click on `root`'s banner should open: the one that has
    /// been waiting longest, which is where the jump-to-next hotkey would
    /// land too.
    pub fn oldest_waiting(&self, root: &str) -> Option<&str> {
        self.waiting.get(root)?.first().map(|(child, _)| child.as_str())
    }
}

impl BridgeState {
    /// The banner identifier for a session's own alert.
    fn session_key(&mut self, target: &str) -> String {
        format!("session:{}", self.handle_of(target))
    }

    /// The banner identifier for a root's coalesced child count.
    fn root_key(&mut self, root: &str) -> String {
        format!("root:{}", self.handle_of(root))
    }

    /// This target's number, minting one the first time it is needed.
    fn handle_of(&mut self, target: &str) -> u64 {
        if let Some(handle) = self.handles.get(target) {
            return *handle;
        }
        self.next_handle += 1;
        let handle = self.next_handle;
        self.handles.insert(target.to_string(), handle);
        self.targets.insert(handle, target.to_string());
        handle
    }

    /// The target a clicked banner's handle names, if this process minted it.
    pub fn target_of(&self, handle: u64) -> Option<&str> {
        self.targets.get(&handle).map(String::as_str)
    }
}

/// A starting point for this run's handles, with room above it to count.
///
/// Shifted down so incrementing can never wrap, and so the numbers in a
/// banner identifier stay short enough to read in a log.
fn random_base() -> u64 {
    (uuid::Uuid::new_v4().as_u64_pair().0 >> 24) + 1
}

/// The body for a root's banner. Singular is worth the branch: "1 agents
/// under api need you" is the kind of thing that makes an app feel unfinished.
fn waiting_body(waiting: usize) -> String {
    match waiting {
        1 => "1 agent needs you".to_string(),
        n => format!("{n} agents need you"),
    }
}

/// Every banner the bridge sends is agent-state traffic, which is the source
/// the user's own per-source toggle gates.
pub const BRIDGE_SOURCE: NotificationSource = NotificationSource::AgentState;

/// What the stream cannot know about a target: what to call it, which
/// workspace it belongs to, and whether the person is already looking at it.
///
/// Supplied by the windows, because it is a fact about tabs and panes rather
/// than about sessions. A target with no context — a session another agent
/// spawned and nobody has opened, a run root with no tab — still banners; it
/// just banners with its own name and counts as not visible, which is true.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetContext {
    pub label: String,
    pub workspace_key: String,
    pub pane_visible: bool,
    pub window_active: bool,
}

impl TargetContext {
    /// The reading for a target no window holds: not visible anywhere, named
    /// after itself, and in no workspace.
    pub fn unseen(target: &str) -> Self {
        Self {
            label: fallback_label(target),
            workspace_key: String::new(),
            pane_visible: false,
            window_active: false,
        }
    }
}

/// A name for a target with no tab: the id after its namespace, which is at
/// least recognisable in a banner ("plan:r-7" reads as "r-7").
fn fallback_label(target: &str) -> String {
    target.split_once(':').map(|(_, id)| id.to_string()).unwrap_or_else(|| target.to_string())
}

/// Turn one change into what the notifier should be told, given the context
/// of the target it is about.
///
/// Split from both the subscription and the state so the *whole* mapping —
/// the rules, the title, the gating inputs — can be tested without a host, a
/// window or an OS. The caller does the talking.
pub fn requests_for(
    state: &mut BridgeState,
    change: &Change,
    context: &TargetContext,
) -> Vec<Told> {
    state
        .on_change(change)
        .into_iter()
        .map(|action| match action {
            BridgeAction::Retire { replace_key } => Told::Retire { replace_key },
            BridgeAction::Post(post) => Told::Post(NotificationRequest {
                source: BRIDGE_SOURCE,
                kind: post.kind,
                // The handle is in the identifier; a banner from here is
                // routed by that, not by a tab id.
                tab_id: crate::notifier::TabId(0),
                workspace_key: context.workspace_key.clone(),
                label: context.label.clone(),
                body: post.body,
                window_active: context.window_active,
                // A root's banner is never "already visible": its children are
                // elsewhere, and the root's own tab being open says nothing
                // about them.
                pane_visible: context.pane_visible && !post.root,
                replace_key: Some(post.replace_key),
                quiet: post.quiet,
            }),
        })
        .collect()
}

/// What to tell the notifier.
#[derive(Debug, Clone, PartialEq)]
pub enum Told {
    Post(NotificationRequest),
    Retire { replace_key: String },
}

// ─── the one subscriber ──────────────────────────────────────────────────────

/// The process's bridge. A global so a second window cannot start a second
/// subscriber and double every banner — the bug this whole module exists to
/// make impossible.
struct BridgeGlobal(Arc<AttentionBridge>);

impl gpui::Global for BridgeGlobal {}

/// Subscribes to the host's attention stream and posts what it decides.
pub struct AttentionBridge {
    notifier: Arc<dyn crate::notifier::Notifier>,
    host: Arc<oximux_remote_host::HostCore>,
    state: Mutex<BridgeState>,
}

impl AttentionBridge {
    /// Start the bridge, once per process.
    ///
    /// Called by each window as it builds; the second call onwards is a no-op.
    /// Silent when the host has not started — remote disabled, or a window
    /// built before `HostCore`: there is then no stream to read, and the next
    /// window to open starts it.
    pub fn start(notifier: Arc<dyn crate::notifier::Notifier>, cx: &mut gpui::App) {
        if cx.has_global::<BridgeGlobal>() {
            return;
        }
        let Some(host) = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
        else {
            return;
        };
        // Subscribe *before* reading the snapshot: the stream is not replayed,
        // so a change between the two would otherwise reach the bridge by
        // neither route.
        let mut changes = host.subscribe_attention();
        let bridge = Arc::new(Self {
            notifier,
            host: host.clone(),
            state: Mutex::new(BridgeState::new()),
        });
        bridge.seed();
        cx.set_global(BridgeGlobal(bridge.clone()));
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            loop {
                match changes.recv().await {
                    Ok(change) => cx.update(|cx| bridge.on_change(change, cx)),
                    // Fell behind: the missed changes are gone, so read the
                    // world instead. Nothing double-banners — the dedupe is by
                    // edge, not by how the edge arrived — and anything raised
                    // while the bridge was behind still announces.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "attention bridge fell behind; re-reading");
                        cx.update(|cx| bridge.resync(cx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        })
        .detach();
    }

    /// The bridge, for the click router.
    pub fn global(cx: &gpui::App) -> Option<Arc<AttentionBridge>> {
        cx.try_global::<BridgeGlobal>().map(|g| g.0.clone())
    }

    /// Record everything already needing a person as announced.
    fn seed(&self) {
        let outstanding: Vec<(String, AttentionEdge)> = self
            .host
            .needs_you()
            .into_iter()
            .filter_map(|(target, _)| {
                self.host.attention_with_edge(&target).1.map(|edge| (target, edge))
            })
            .collect();
        self.state.lock().unwrap().seed(outstanding);
    }

    /// Re-read the world after falling behind.
    ///
    /// Not just what needs someone now: a change the channel dropped could
    /// have been an answer or a retirement, and such a target is *absent*
    /// from `needs_you` — so visiting only that list would leave its banner
    /// standing for the rest of the run. Everything the bridge is holding
    /// something for is revisited too, which is exactly the set with
    /// something on screen to take back.
    fn resync(&self, cx: &mut gpui::App) {
        let mut targets: Vec<String> = self.host.attention_targets();
        targets.extend(self.state.lock().unwrap().tracked());
        targets.sort();
        targets.dedup();
        for target in targets {
            self.reconsider(&target, cx);
        }
    }

    /// Handle one change off the stream.
    fn on_change(&self, change: oximux_remote_host::attention::AttentionChanged, cx: &mut gpui::App) {
        let target = change.target.clone();
        let change = Change {
            target: change.target,
            attention: change.attention,
            edge: change.edge,
            retired: change.retired,
            root: self.root_of(&target),
        };
        self.apply(change, cx);
    }

    /// Re-derive one target's state and feed it through.
    fn reconsider(&self, target: &str, cx: &mut gpui::App) {
        let (attention, edge) = self.host.attention_with_edge(target);
        self.apply(
            Change {
                target: target.to_string(),
                attention,
                edge,
                retired: None,
                root: self.root_of(target),
            },
            cx,
        );
    }

    fn apply(&self, change: Change, cx: &mut gpui::App) {
        // Re-validate against the host right before deciding: this change has
        // been through a channel and a GPUI hop, and a prompt answered in that
        // window must not banner. The host is the authority on both sides, so
        // asking it again costs a lock and settles it.
        if let Some(edge) = &change.edge {
            let current = self.host.attention_with_edge(&change.target).1;
            if current.as_ref() != Some(edge) {
                // What it wants now is something else; that arrives as its own
                // change rather than being guessed at here.
                return;
            }
        }
        let context = context_of(&change.target, cx);
        let told = {
            let mut state = self.state.lock().unwrap();
            requests_for(&mut state, &change, &context)
        };
        for one in told {
            match one {
                Told::Post(request) => self.notifier.notify(request),
                Told::Retire { replace_key } => self.notifier.retire(&replace_key),
            }
        }
    }

    /// The root `target` hangs under, when it is somebody's child.
    fn root_of(&self, target: &str) -> Option<String> {
        let session = oximux_agents::session_prefs::chat_id(target)?;
        let row = self.host.lineage().row(session).ok().flatten()?;
        // A root is not its own child: `root_id` is itself for a top-level
        // session that happens to have a row.
        (row.root_id != session).then(|| oximux_agents::session_prefs::chat_key(&row.root_id))
    }

    /// The target a clicked banner names, and whether the click should go to a
    /// child of it.
    pub fn clicked(&self, target: crate::notifier::ClickTarget) -> Option<String> {
        let state = self.state.lock().unwrap();
        match target {
            crate::notifier::ClickTarget::NeedsYou(handle) => {
                state.target_of(handle).map(str::to_string)
            }
            // A root's banner goes to the child that has waited longest — the
            // same place the jump-to-next hotkey would land.
            crate::notifier::ClickTarget::NeedsYouRoot(handle) => {
                let root = state.target_of(handle)?;
                state.oldest_waiting(root).map(str::to_string).or_else(|| Some(root.to_string()))
            }
            _ => None,
        }
    }
}

/// What the windows know about `target`.
///
/// Walks every open window, because a session's tab can be in any of them and
/// a banner must not be suppressed as "visible" by a window that is not in
/// front. A target no window holds reads as not visible, which is the honest
/// answer for a session another agent spawned.
fn context_of(target: &str, cx: &mut gpui::App) -> TargetContext {
    let chat = oximux_agents::session_prefs::chat_id(target).map(str::to_string);
    for (_, root) in crate::platform::window_registry::all_windows(cx) {
        let found = root.read(cx).attention_context_for(chat.as_deref(), target, cx);
        if let Some(context) = found {
            return context;
        }
    }
    TargetContext::unseen(target)
}

#[cfg(test)]
#[path = "attention_bridge_tests.rs"]
mod tests;
