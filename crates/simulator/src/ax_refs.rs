//! `@eN` refs for accessibility elements, the diff an action prints, and the
//! hash its settle compares (no gpui: the desktop's verify loop drives these).
//!
//! An element is named by a [`NodeKey`] — its role, its identifier (else its
//! label), and which of the elements sharing both it is, in tree order. A
//! [`RefTable`] gives each key that can be acted on a number the first time it
//! is seen and keeps it: a surviving key keeps its ref, only new keys get new
//! numbers, and a number never moves to another key — when look-alikes come or
//! go, their numbers retire and they are numbered afresh. Nothing here knows about
//! sessions, so a ref outlives the stream restarting under it (a park, a
//! reconnect); a table goes only when its worktree detaches or changes device
//! ([`RefStore::forget_worktree`]).
//!
//! Frames are in the tree's own units (points on iOS, display pixels on
//! Android); callers pass `pt`, the units per point, wherever a distance in
//! points matters.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use crate::DeviceId;
use crate::ax::{self, AxNode};
use crate::registry::WorktreeKey;

/// A ref's element further than this (points) from where it was last seen
/// still resolves, with a `moved` warning.
pub const MOVED_PT: f64 = 8.0;
/// The most elements one read is keyed from.
const MAX_NODES: usize = 5000;
/// Keys a table remembers; past it, keys not in the latest read are dropped.
const MAX_KEYS: usize = 4096;
/// Callers' tables kept per worktree and device; the least recently used goes.
pub const MAX_CALLERS: usize = 8;

/// Roles an agent acts on: the ones `ax -i` lists and refs number.
pub fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "Button" | "TextField" | "SecureTextField" | "SearchField" | "TextView" | "Switch" | "Toggle" | "CheckBox" | "RadioButton"
            | "Slider" | "Stepper" | "Incrementor" | "Link" | "Cell" | "Tab" | "SegmentedControl" | "PopUpButton" | "ComboBox"
            | "MenuItem" | "Picker" | "PickerWheel" | "Key"
            // A real iPhone's runner names `XCUIElement` types: an app icon is
            // an `Icon`, and these act like buttons too.
            | "Icon" | "MenuButton" | "ToolbarButton" | "DisclosureTriangle" | "PageIndicator" | "DatePicker" | "IncrementArrow"
    )
}

/// Roles that animate by nature: never part of a diff or the settle hash.
fn is_progress(role: &str) -> bool {
    matches!(role, "ProgressIndicator" | "ActivityIndicator" | "BusyIndicator" | "ProgressBar")
}

/// What names one element across reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
    pub role: String,
    /// The identifier, else the label, else empty.
    pub name: String,
    /// Its place among the elements sharing `role` and `name`, in tree order.
    pub nth: u32,
}

/// An element with its key, and how many share its role and name.
#[derive(Clone, Debug)]
pub struct Keyed<'a> {
    pub key: NodeKey,
    pub node: &'a AxNode,
    pub group: u32,
}

/// The elements of a read that count, keyed, in tree order: anything with a
/// label, identifier or value, and anything interactive — less what is not on
/// screen (a zero-size frame) and what changes on its own (progress roles,
/// and the status bar: elements wholly in the top 5% of the screen).
pub fn keyed(nodes: &[AxNode]) -> Vec<Keyed<'_>> {
    let top = nodes.first().map_or(0.0, |root| root.frame.y + root.frame.h * 0.05);
    let mut seen: HashMap<(String, String), u32> = HashMap::new();
    let mut out: Vec<Keyed<'_>> = ax::flatten(nodes, MAX_NODES)
        .into_iter()
        .map(|f| f.node)
        .filter(|n| ax::is_on_screen(n) && !is_progress(&n.role) && n.frame.y + n.frame.h > top)
        .filter(|n| is_interactive(&n.role) || has_text(n.label.as_deref()) || has_text(n.identifier.as_deref()) || has_text(n.value.as_deref()))
        .map(|node| {
            let name = [node.identifier.as_deref(), node.label.as_deref()].into_iter().flatten().find(|s| has_text(Some(s))).unwrap_or("");
            let count = seen.entry((node.role.clone(), name.to_owned())).or_default();
            *count += 1;
            Keyed { key: NodeKey { role: node.role.clone(), name: name.to_owned(), nth: *count - 1 }, node, group: 0 }
        })
        .collect();
    for k in &mut out {
        k.group = seen[&(k.key.role.clone(), k.key.name.clone())];
    }
    out
}

fn has_text(s: Option<&str>) -> bool {
    s.is_some_and(|s| !s.trim().is_empty())
}

/// A hash of what is on screen, to tell two reads apart: roles, labels,
/// identifiers, values and frames rounded to 2 points (`pt` units each).
pub fn ax_hash(nodes: &[AxNode], pt: f64) -> u64 {
    let grid = 2.0 * pt.max(f64::EPSILON);
    let mut h = DefaultHasher::new();
    for k in keyed(nodes) {
        let n = k.node;
        (&n.role, &n.label, &n.identifier, &n.value, n.enabled).hash(&mut h);
        for v in [n.frame.x, n.frame.y, n.frame.w, n.frame.h] {
            ((v / grid).round() as i64).hash(&mut h);
        }
    }
    h.finish()
}

/// Which way a diff line went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    Added,
    Removed,
}

#[derive(Clone, Debug)]
pub struct DiffLine<'a> {
    pub op: DiffOp,
    pub r: Option<u32>,
    pub node: &'a AxNode,
}

#[derive(Clone, Debug, Default)]
pub struct Diff<'a> {
    pub lines: Vec<DiffLine<'a>>,
    pub unchanged: u32,
}

/// What changed from `before` to `after`: elements gone, then (in `after`'s
/// order) elements whose value or enabled state changed (one line each way)
/// and new ones. Refs come from `table`; assign `after` first.
pub fn diff<'a>(before: &'a [AxNode], after: &'a [AxNode], table: &RefTable) -> Diff<'a> {
    let old: HashMap<NodeKey, &AxNode> = keyed(before).into_iter().map(|k| (k.key, k.node)).collect();
    let new = keyed(after);
    let current: std::collections::HashSet<&NodeKey> = new.iter().map(|k| &k.key).collect();
    let mut out = Diff::default();
    let line = |op, key: &NodeKey, node| DiffLine { op, r: table.ref_of(key), node };
    for k in keyed(before).into_iter().filter(|k| !current.contains(&k.key)) {
        out.lines.push(line(DiffOp::Removed, &k.key, k.node));
    }
    for k in &new {
        match old.get(&k.key) {
            Some(was) if was.value == k.node.value && was.enabled == k.node.enabled => out.unchanged += 1,
            Some(was) => {
                out.lines.push(line(DiffOp::Removed, &k.key, was));
                out.lines.push(line(DiffOp::Added, &k.key, k.node));
            }
            None => out.lines.push(line(DiffOp::Added, &k.key, k.node)),
        }
    }
    out
}

/// Why a ref did not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefError {
    /// No element ever had this number (in this caller's table).
    Unknown,
    /// Its element is not on screen.
    Gone,
    /// Elements alike in role and name came or went, so which one it was can
    /// no longer be told.
    Ambiguous,
}

/// A ref's element in a fresh read.
#[derive(Clone, Debug)]
pub struct Resolved<'a> {
    pub node: &'a AxNode,
    /// How far (points) it is from where it was last seen.
    pub moved_pt: f64,
}

#[derive(Clone, Debug)]
struct Entry {
    r: u32,
    center: (f64, f64),
    group: u32,
    /// The read it was last in.
    seen: u64,
}

/// The last full read a table saw: an action's "before" while nothing has
/// changed on screen since — judged on the same stream (`pid`) without a
/// pause or resume in between (`epoch`, see `keyframe::FrameClock`).
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// When the read started.
    pub taken_at: Instant,
    pub nodes: Arc<Vec<AxNode>>,
    pub pid: u32,
    pub epoch: u64,
}

/// One caller's refs on one device (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct RefTable {
    next: u32,
    by_key: HashMap<NodeKey, Entry>,
    by_ref: HashMap<u32, NodeKey>,
    reads: u64,
    pub last: Option<Snapshot>,
}

impl RefTable {
    /// Number the interactive elements of a read (keeping every surviving
    /// key's number) and return them with their refs, in tree order.
    pub fn assign<'a>(&mut self, nodes: &'a [AxNode]) -> Vec<(u32, &'a AxNode)> {
        self.reads += 1;
        let read = keyed(nodes);
        // Look-alikes (one role and name) came or went: which of them a
        // number named can no longer be told, so every number of that role
        // and name retires — on screen or not — rather than pass to a twin.
        // Its holder's next use fails as unknown, and asks for a fresh read.
        let sizes: HashMap<(&str, &str), u32> = read.iter().map(|k| ((k.key.role.as_str(), k.key.name.as_str()), k.group)).collect();
        let changed: Vec<NodeKey> = self
            .by_key
            .iter()
            .filter(|(key, e)| sizes.get(&(key.role.as_str(), key.name.as_str())).is_some_and(|&n| n != e.group && n.max(e.group) > 1))
            .map(|(key, _)| key.clone())
            .collect();
        for key in changed {
            if let Some(entry) = self.by_key.remove(&key) {
                self.by_ref.remove(&entry.r);
            }
        }
        let mut out = Vec::new();
        for k in read.into_iter().filter(|k| is_interactive(&k.node.role)) {
            let center = k.node.frame.center();
            let r = match self.by_key.get_mut(&k.key) {
                Some(entry) => {
                    (entry.center, entry.seen) = (center, self.reads);
                    entry.r
                }
                None => {
                    self.next += 1;
                    self.by_ref.insert(self.next, k.key.clone());
                    self.by_key.insert(k.key, Entry { r: self.next, center, group: k.group, seen: self.reads });
                    self.next
                }
            };
            out.push((r, k.node));
        }
        if self.by_key.len() > MAX_KEYS {
            let reads = self.reads;
            self.by_key.retain(|_, e| e.seen == reads);
            let by_key = &self.by_key;
            self.by_ref.retain(|_, key| by_key.contains_key(key));
        }
        out
    }

    pub fn ref_of(&self, key: &NodeKey) -> Option<u32> {
        self.by_key.get(key).map(|e| e.r)
    }

    pub fn key_of(&self, r: u32) -> Option<&NodeKey> {
        self.by_ref.get(&r)
    }

    /// Ref `r`'s element in `fresh`, a read not yet assigned (`pt` units per
    /// point, for the distance it moved).
    pub fn resolve<'a>(&self, r: u32, fresh: &'a [AxNode], pt: f64) -> Result<Resolved<'a>, RefError> {
        let key = self.by_ref.get(&r).ok_or(RefError::Unknown)?;
        let entry = &self.by_key[key];
        let found = keyed(fresh).into_iter().find(|k| &k.key == key).ok_or(RefError::Gone)?;
        if found.group != entry.group && found.group.max(entry.group) > 1 {
            return Err(RefError::Ambiguous);
        }
        let (x, y) = found.node.frame.center();
        let moved_pt = (x - entry.center.0).hypot(y - entry.center.1) / pt.max(f64::EPSILON);
        Ok(Resolved { node: found.node, moved_pt })
    }
}

/// The element `key` names in `nodes`, if it is on screen.
pub fn find_key<'a>(nodes: &'a [AxNode], key: &NodeKey) -> Option<&'a AxNode> {
    keyed(nodes).into_iter().find(|k| &k.key == key).map(|k| k.node)
}

/// Whose refs: a worktree's device, as seen by one caller (`None`: callers
/// that do not say who they are share one table).
pub type TableKey = (WorktreeKey, DeviceId, Option<u64>);

/// Every caller's tables, for the app's lifetime.
#[derive(Debug, Default)]
pub struct RefStore {
    tables: HashMap<TableKey, (RefTable, u64)>,
    tick: u64,
    /// The highest number a dropped table had given out: a new table counts
    /// on from here, so a returning caller's old refs never name something
    /// else.
    floor: u32,
}

impl RefStore {
    /// `key`'s table (made empty the first time), marked as just used. A
    /// worktree's device keeps at most [`MAX_CALLERS`] callers' tables.
    pub fn table(&mut self, key: &TableKey) -> &mut RefTable {
        self.tick += 1;
        if !self.tables.contains_key(key) {
            let same = |k: &TableKey| k.0 == key.0 && k.1 == key.1;
            while self.tables.keys().filter(|k| same(k)).count() >= MAX_CALLERS {
                let oldest = self.tables.iter().filter(|(k, _)| same(k)).min_by_key(|(_, (_, used))| *used).map(|(k, _)| k.clone());
                let Some(oldest) = oldest else { break };
                self.drop_table(&oldest);
            }
        }
        let (tick, floor) = (self.tick, self.floor);
        let (table, used) = self.tables.entry(key.clone()).or_insert_with(|| (RefTable { next: floor, ..RefTable::default() }, 0));
        *used = tick;
        table
    }

    /// Drop every table of `worktree` (it detached, or changed device).
    pub fn forget_worktree(&mut self, worktree: &WorktreeKey) {
        let gone: Vec<TableKey> = self.tables.keys().filter(|k| &k.0 == worktree).cloned().collect();
        for key in gone {
            self.drop_table(&key);
        }
    }

    fn drop_table(&mut self, key: &TableKey) {
        if let Some((table, _)) = self.tables.remove(key) {
            self.floor = self.floor.max(table.next);
        }
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

#[cfg(test)]
#[path = "ax_refs_tests.rs"]
mod tests;
