//! One definition of "needs you", for every surface that asks.
//!
//! Before this, each surface decided for itself: the dashboard counted Done
//! and Interrupted as attention, the rail badge counted status transitions,
//! the palette had a third list, and two notification paths fired in parallel.
//! The counts disagreed, so none of them could be trusted. [`of`] is the only
//! answer now, and it is pure: the same health and the same prefs give the
//! same attention every time, so the rail, the chip, the badge, the hotkey and
//! the notifier cannot drift apart.
//!
//! Three rules carry the weight:
//!
//! - **Needs you is a short list.** Only a person's own blockers count: a
//!   permission, a question, a failure, a stall. Rate limits retry themselves
//!   and finished work can wait, so they are shown but not counted — a badge
//!   that counts things nobody has to act on is a badge people stop reading.
//! - **Attention is about the person, not the agent.** A failure that has been
//!   looked at is no longer an alert. That is why [`of`] needs the prefs: the
//!   agent's state alone cannot say whether anyone has seen it.
//! - **Not every alert is a session's health.** CI exhausting its fix attempts,
//!   a plan run drifting, a terminal agent with no health at all: those come in
//!   as [`ExternalAttention`] from whoever knows about them, and are ranked
//!   against the session's own state rather than rendered somewhere else.

use crate::health::{HealthStatus, Liveness, SessionHealth};
use crate::session_prefs::SessionPrefs;

/// What a session wants from a person, in priority order. Lower [`priority`]
/// wins when several hold at once.
///
/// [`priority`]: AttentionKind::priority
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AttentionKind {
    /// A tool permission, a plan approval, an auth prompt, a question with
    /// options: the agent is stopped until someone answers.
    NeedsPermission,
    /// A plain "waiting for your reply" prompt.
    NeedsInput,
    /// The process died, or the last turn failed and nothing has run since.
    Failed,
    /// Working, but silent for longer than the threshold.
    Stalled,
    /// A provider limit closed the last turn; a retry is armed. Informational:
    /// it needs nobody.
    Limited,
    /// Finished a turn that nobody has read. Informational.
    FinishedUnread,
}

impl AttentionKind {
    /// 1 (most urgent) to 6. Kinds 1–4 are "needs you".
    pub fn priority(self) -> u8 {
        match self {
            AttentionKind::NeedsPermission => 1,
            AttentionKind::NeedsInput => 2,
            AttentionKind::Failed => 3,
            AttentionKind::Stalled => 4,
            AttentionKind::Limited => 5,
            AttentionKind::FinishedUnread => 6,
        }
    }

    /// Whether a person has to act. Only these are counted (the chip, the nav
    /// badge, the jump-to-next cycle, the NEEDS YOU section).
    pub fn is_needs_you(self) -> bool {
        self.priority() <= 4
    }

    /// Whether leaving it rests on the person rather than on the agent: a
    /// failure or a stall is dismissible, an unanswered prompt is not (only
    /// answering it clears it, and a dismissed prompt would be a lie).
    pub fn is_dismissible(self) -> bool {
        matches!(self, AttentionKind::Failed | AttentionKind::Stalled)
    }

    /// The design token the row's state dot and 2px accent use. Named, not a
    /// colour: the theme owns the hue.
    pub fn tone(self) -> AttentionTone {
        match self {
            AttentionKind::NeedsPermission | AttentionKind::NeedsInput | AttentionKind::Limited => {
                AttentionTone::Warn
            }
            AttentionKind::Failed => AttentionTone::Error,
            AttentionKind::Stalled => AttentionTone::Muted,
            AttentionKind::FinishedUnread => AttentionTone::Ok,
        }
    }

    /// The row's state verb, in the words the chat card uses.
    pub fn verb(self) -> &'static str {
        match self {
            AttentionKind::NeedsPermission => "Needs approval",
            AttentionKind::NeedsInput => "Waiting for input",
            AttentionKind::Failed => "Failed",
            AttentionKind::Stalled => "Stalled",
            AttentionKind::Limited => "Limited",
            AttentionKind::FinishedUnread => "Done",
        }
    }
}

/// Which `status_*` token paints the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionTone {
    Warn,
    Error,
    Muted,
    Ok,
}

/// What a session is asking for, and since when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub kind: AttentionKind,
    /// When this became true, in Unix milliseconds. Compared against the
    /// prefs' `last_viewed_at` to decide whether anyone has seen it, and it is
    /// half of a stall or external item's edge identity.
    pub since: i64,
    /// One line for the row and the tooltip.
    pub reason: Option<String>,
    /// Where it came from, so its edge identity can be worked out.
    pub source: AttentionSource,
}

/// Which input raised an [`Attention`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionSource {
    /// The session's own state.
    Health,
    /// An [`ExternalAttention`] under this key.
    External(String),
}

/// What identifies one alert, so a surface can tell "the same thing again"
/// from "something new".
///
/// This is why banners do not need a time window. A health re-snapshot, a tab
/// reopening, a rekey or a reconnect all produce the same edge, and the same
/// edge is never alerted twice. A *different* request on the same session is a
/// different edge, so it alerts at once instead of waiting out a window — which
/// is what a person wants when a second agent stops.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttentionEdge {
    /// Whose alert: a session id, a cockpit key, or a run root.
    pub target: String,
    pub kind: AttentionKind,
    /// Unique per occurrence within `(target, kind)`.
    pub id: String,
}

/// The edge identity of `attention` on `target`.
///
/// `prompt_request` is the provider's id for the pending prompt, which the
/// caller reads off the session handle; health alone does not carry it. Where
/// a provider gives none, the moment the prompt arrived stands in: a prompt
/// arriving is activity, so that timestamp moves with every new prompt.
pub fn edge_of(
    target: &str,
    attention: &Attention,
    health: Option<&SessionHealth>,
    prompt_request: Option<&str>,
) -> AttentionEdge {
    let id = match &attention.source {
        // The producer's key plus when it raised it: re-publishing the same
        // trip (a boot replay, a re-scan) is the same edge.
        AttentionSource::External(key) => external_edge_id(key, attention.since),
        AttentionSource::Health => match attention.kind {
            // The request being answered, so one prompt alerts once however
            // many times the session's health is re-derived.
            AttentionKind::NeedsPermission | AttentionKind::NeedsInput => prompt_request
                .map(|id| format!("req:{id}"))
                .unwrap_or_else(|| format!("prompt:{}", attention.since)),
            // A failure that is a process exit is identified by that exit; a
            // failed turn by the turn, so a retry that fails again is a new
            // edge. The exit count is only the right identity while the exit
            // is what failed: it survives a reconnect, so a later failed turn
            // would otherwise reuse the last exit's identity and be swallowed
            // by a subscriber that remembers what it has alerted on.
            AttentionKind::Failed => health
                .and_then(|h| {
                    let exited = h.exited_unexpectedly || matches!(h.liveness, Liveness::Exited(_));
                    exited.then(|| h.exit_seq.clone()).flatten().or_else(|| h.turn_id.clone())
                })
                .unwrap_or_else(|| format!("failed:{}", attention.since)),
            // The episode the silence was measured from: it re-arms only
            // after new activity and then new silence.
            AttentionKind::Stalled => format!("stall:{}", attention.since),
            AttentionKind::Limited => health
                .and_then(|h| h.limited_until.map(|until| format!("limit:{until}")))
                .unwrap_or_else(|| format!("limit:{}", attention.since)),
            AttentionKind::FinishedUnread => health
                .and_then(|h| h.turn_id.clone())
                .unwrap_or_else(|| format!("turn:{}", attention.since)),
        },
    };
    AttentionEdge { target: target.to_string(), kind: attention.kind, id }
}

/// An alert that is not a session's health: CI out of fix attempts, a plan run
/// whose ticket tripped its breaker, a terminal agent that has no health at
/// all. Raised and cleared by whoever knows about it; nothing here is
/// persisted, because every producer re-publishes from its own state on boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAttention {
    /// The producer's own key, unique across producers (`cockpit:<db_id>`,
    /// `plan:<run>:<ticket>:breaker`). Clearing is by this key.
    pub key: String,
    /// Whose row it renders on: a session id, a cockpit key, or a run root
    /// (`team:<run>` / `plan:<run>`).
    pub target: String,
    pub kind: AttentionKind,
    pub reason: String,
    /// When the producer raised it, in Unix milliseconds. With `key` it is the
    /// item's edge identity, so re-publishing the same trip never alerts twice.
    pub since: i64,
}

/// One external item's edge id, spelled in one place so the evaluator's
/// "already seen" check and [`edge_of`] cannot disagree.
fn external_edge_id(key: &str, since: i64) -> String {
    format!("{key}@{since}")
}

/// Whether anyone still has unread work in this session: it did something
/// after they last looked, or they marked it unread on purpose.
pub fn is_unread(health: &SessionHealth, prefs: &SessionPrefs) -> bool {
    prefs.unread_override
        || match prefs.last_viewed_at {
            None => true,
            Some(viewed) => health.last_activity_at > viewed,
        }
}

/// Whether the person has already dealt with a particular alert.
///
/// This cannot be answered from the health. `last_activity_at` is when the
/// session last did something, which for a stall is when the *silence* began —
/// minutes before the stall was declared — and a process dies after its last
/// output. Comparing a view against either would suppress an alert that arose
/// after the person looked, which is the one thing this phase exists to
/// prevent. So the answer comes from whoever watches the alerts appear and
/// records the views: an alert it has no record of is new, and therefore
/// unseen.
///
/// It is a question rather than a timestamp because *ordering* is what
/// matters, and a clock cannot always supply it: an alert arising in the same
/// millisecond as a view is indistinguishable from one the view covered. The
/// implementor orders the two by observation instead.
pub trait SeenEdges {
    /// Whether `edge` arrived no later than the person's last look at its
    /// target. `since` is the alert's own best idea of when it began, which an
    /// implementor may need when it has no observed order to go on.
    fn is_seen(&self, edge: &AttentionEdge, since: i64) -> bool;
}

/// No history: every alert reads as new. What a caller that watches nothing
/// uses — a remote client, which has no prefs either.
pub struct NothingSeen;

impl SeenEdges for NothingSeen {
    fn is_seen(&self, _edge: &AttentionEdge, _since: i64) -> bool {
        false
    }
}

impl<F: Fn(&AttentionEdge, i64) -> bool> SeenEdges for F {
    fn is_seen(&self, edge: &AttentionEdge, since: i64) -> bool {
        self(edge, since)
    }
}

/// What this session wants from a person: the worse of its own health and the
/// external items raised against it, or `None` when it wants nothing.
///
/// `external` is the items whose `target` is this session (or run root); the
/// caller filters, because it holds the registry. `seen` answers whether a dismissible alert has already been
/// dealt with, so it can be left out; pass [`NothingSeen`] to keep every
/// alert.
///
/// A tie between the session's own state and an external item goes to the
/// session: its health is the more local truth, and the external item is still
/// there to render on the row.
pub fn of(
    target: &str,
    health: &SessionHealth,
    prefs: &SessionPrefs,
    external: &[ExternalAttention],
    seen: &dyn SeenEdges,
) -> Option<Attention> {
    let own = from_health(target, health, prefs, seen);
    let outside = of_external(target, prefs, external, seen);
    match (own, outside) {
        (Some(a), Some(b)) if b.kind.priority() < a.kind.priority() => Some(b),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

/// The worst unseen external item for a target that has no health of its own
/// (a terminal agent, a run root).
pub fn of_external(
    target: &str,
    prefs: &SessionPrefs,
    external: &[ExternalAttention],
    seen: &dyn SeenEdges,
) -> Option<Attention> {
    external
        .iter()
        .filter(|item| !seen_external(target, item, prefs, seen))
        // The producer key breaks a tie: two items of one kind raised in the
        // same millisecond arrive in whatever order a `HashMap` iterated, and
        // picking by that would show a different reason on each pass.
        .min_by_key(|item| (item.kind.priority(), item.since, &item.key))
        .map(|item| Attention {
            kind: item.kind,
            since: item.since,
            reason: Some(item.reason.clone()),
            source: AttentionSource::External(item.key.clone()),
        })
}

/// Whether the person has dealt with one external item.
///
/// An unread finish counts here although it is not dismissible: a producer can
/// raise one, and looking at the target is what reads it. Nothing else
/// informational can be cleared by a look — a rate limit is still a rate limit
/// once you have seen it.
fn seen_external(
    target: &str,
    item: &ExternalAttention,
    prefs: &SessionPrefs,
    seen: &dyn SeenEdges,
) -> bool {
    if item.kind == AttentionKind::FinishedUnread {
        return !prefs.unread_override
            && prefs.last_viewed_at.is_some_and(|viewed| viewed >= item.since);
    }
    let edge = AttentionEdge {
        target: target.to_string(),
        kind: item.kind,
        id: external_edge_id(&item.key, item.since),
    };
    is_seen(item.kind, &edge, item.since, prefs, seen)
}

/// The session's own state, read as attention.
fn from_health(
    target: &str,
    health: &SessionHealth,
    prefs: &SessionPrefs,
    seen: &dyn SeenEdges,
) -> Option<Attention> {
    let kind = match health.status {
        HealthStatus::NeedsPermission => AttentionKind::NeedsPermission,
        HealthStatus::NeedsAnswer => AttentionKind::NeedsInput,
        HealthStatus::Failed => AttentionKind::Failed,
        HealthStatus::Limited => AttentionKind::Limited,
        // A user's own Stop, or a session that was alive at shutdown: they
        // asked for it, so it asks nothing back.
        HealthStatus::Interrupted => return None,
        HealthStatus::Working => {
            // Silence is only a stall when there is nothing else explaining
            // it. A send the provider has not answered is the session
            // working, however quiet the line is — a rate-limit hold is quiet
            // by design, and reading it as stalled would cry wolf every time.
            if health.pending_unanswered_send {
                return None;
            }
            let since = health.stalled_since?;
            let stalled = Attention {
                kind: AttentionKind::Stalled,
                since,
                reason: health.reason.clone().or_else(|| Some("no output".into())),
                source: AttentionSource::Health,
            };
            let edge = edge_of(target, &stalled, Some(health), None);
            return (!is_seen(AttentionKind::Stalled, &edge, since, prefs, seen)).then_some(stalled);
        }
        // Idle and nothing pending: the only thing left is a turn nobody read.
        // `turn_id` is what says a turn ever ran, so a session that has never
        // been sent to is simply idle, not unread.
        HealthStatus::Idle => {
            return (health.turn_id.is_some() && is_unread(health, prefs)).then(|| Attention {
                kind: AttentionKind::FinishedUnread,
                since: health.last_activity_at,
                reason: health.reason.clone(),
                source: AttentionSource::Health,
            });
        }
    };
    let raised = Attention {
        kind,
        since: health.last_activity_at,
        reason: health.reason.clone(),
        source: AttentionSource::Health,
    };
    // The edge is built without a request id: only the dismissible kinds are
    // filtered here, and neither a failure nor a stall has one.
    let edge = edge_of(target, &raised, Some(health), None);
    (!is_seen(kind, &edge, raised.since, prefs, seen)).then_some(raised)
}

/// Whether the person has already dealt with this exact alert.
///
/// Only the dismissible kinds can be "seen": viewing a session does not answer
/// its permission prompt, and pretending it did would hide a stopped agent.
/// An explicit unread mark overrides any look.
///
/// Everything else is [`SeenEdges`]' to answer, because it is a question about
/// the order in which the alert and the look were observed, not about what
/// either timestamp says.
fn is_seen(
    kind: AttentionKind,
    edge: &AttentionEdge,
    since: i64,
    prefs: &SessionPrefs,
    seen: &dyn SeenEdges,
) -> bool {
    kind.is_dismissible() && !prefs.unread_override && seen.is_seen(edge, since)
}

#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;
