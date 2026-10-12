//! Parses the helper's `ax_describe`/`ax_frontmost` replies into a tree the
//! rest of the crate can search and tap, per the protocol's `4 Reply json
//! {id, ok|err, body}` frame (`phase-03-simulator-core-crate.md`'s wire
//! table).
//!
//! AX frames are in **logical display points, current orientation** — not
//! portrait-normalized (`phase-03-simulator-core-crate.md`'s AX fact and the
//! spike's "AX frames are in the rotated (logical) space" finding). A
//! `tap --label` verb must run [`center`]'s result through
//! [`crate::geometry::logical_points_to_portrait_normalized`] before sending
//! a touch. Real fixtures: `tests/fixtures/ax_describe_safari_portrait.json`
//! (root frame 402×874), `ax_describe_safari_landscape_left.json`
//! (orientation `LandscapeLeft`, root frame 874×402), `ax_frontmost_safari.json`.

use serde::Deserialize;
use serde_json::Value;

use crate::geometry::Rect;
use crate::{Result, SimError};

/// One accessibility node. Field names follow this crate's convention
/// (`label`/`identifier`/`value`/`role`) rather than the wire's `AX*`/`type`
/// keys, which [`WireNode`] parses and [`From`] renames away.
#[derive(Clone, Debug, PartialEq)]
pub struct AxNode {
    pub label: Option<String>,
    pub identifier: Option<String>,
    pub value: Option<String>,
    /// The wire's `type` (e.g. `"Button"`, `"TextField"`, `"Application"`).
    pub role: String,
    pub role_description: String,
    pub enabled: bool,
    pub frame: Rect,
    pub children: Vec<AxNode>,
}

#[derive(Clone, Debug, Deserialize)]
struct WireFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct WireNode {
    #[serde(rename = "AXLabel")]
    ax_label: Option<String>,
    #[serde(rename = "AXUniqueId")]
    ax_unique_id: Option<String>,
    /// `AXValue` is a string, a number, or `null` on the wire (see the
    /// fixtures: a heading's is the string `"1"`, most others are `null`);
    /// normalized to `Option<String>` by [`value_to_string`].
    #[serde(rename = "AXValue", default)]
    ax_value: Option<Value>,
    #[serde(rename = "type")]
    ty: String,
    role_description: String,
    enabled: bool,
    frame: WireFrame,
    #[serde(default)]
    children: Vec<WireNode>,
}

fn value_to_string(v: Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    }
}

impl From<WireNode> for AxNode {
    fn from(w: WireNode) -> Self {
        AxNode {
            label: w.ax_label,
            identifier: w.ax_unique_id,
            value: w.ax_value.and_then(value_to_string),
            role: w.ty,
            role_description: w.role_description,
            enabled: w.enabled,
            frame: Rect::new(w.frame.x, w.frame.y, w.frame.width, w.frame.height),
            children: w.children.into_iter().map(Into::into).collect(),
        }
    }
}

/// Pulls the `result` value out of a helper reply, accepting either the
/// whole event object (`{"event":"response","id":N,"ok":true,"result":...}`)
/// or a bare `result` value — so callers that already unwrapped the envelope
/// don't have to re-wrap it.
fn result_value(json: &[u8], what: &str) -> Result<Value> {
    let v: Value = serde_json::from_slice(json)
        .map_err(|e| SimError::Parse { what: what.to_owned(), detail: e.to_string() })?;
    match v {
        Value::Object(mut obj) if obj.contains_key("result") => Ok(obj.remove("result").unwrap()),
        other => Ok(other),
    }
}

/// Parses an `ax_describe` reply into its node tree.
pub fn parse_describe(json: &[u8]) -> Result<Vec<AxNode>> {
    let result = result_value(json, "ax_describe")?;
    let wire: Vec<WireNode> = serde_json::from_value(result)
        .map_err(|e| SimError::Parse { what: "ax_describe".to_owned(), detail: e.to_string() })?;
    Ok(wire.into_iter().map(Into::into).collect())
}

/// The frontmost app, from an `ax_frontmost` reply.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Frontmost {
    #[serde(rename = "bundleId")]
    pub bundle_id: String,
    pub pid: u32,
}

/// Parses an `ax_frontmost` reply.
pub fn parse_frontmost(json: &[u8]) -> Result<Frontmost> {
    let result = result_value(json, "ax_frontmost")?;
    serde_json::from_value(result)
        .map_err(|e| SimError::Parse { what: "ax_frontmost".to_owned(), detail: e.to_string() })
}

/// One node plus its depth in the tree, as produced by [`flatten`].
#[derive(Clone, Debug)]
pub struct FlatNode<'a> {
    pub node: &'a AxNode,
    pub depth: usize,
}

/// Depth-first, pre-order flattening of `nodes`, capped at `cap` entries —
/// a large tree (a full Settings screen can run to several hundred nodes)
/// should not blow past the protocol's 16 MB frame cap or a chat transcript's
/// budget when handed to an agent.
pub fn flatten(nodes: &[AxNode], cap: usize) -> Vec<FlatNode<'_>> {
    let mut out = Vec::new();
    let mut stack: Vec<(&AxNode, usize)> = nodes.iter().rev().map(|n| (n, 0)).collect();
    while let Some((node, depth)) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        out.push(FlatNode { node, depth });
        for child in node.children.iter().rev() {
            stack.push((child, depth + 1));
        }
    }
    out
}

/// A lookup key for [`find`].
#[derive(Clone, Copy, Debug)]
pub enum Query<'a> {
    Label(&'a str),
    Id(&'a str),
}

/// Depth-first search for a node matching `query`. `Query::Id` is always an
/// exact match. `Query::Label` tries an exact match first, then falls back
/// to a case-insensitive substring match — AX labels are often longer than
/// what an agent or a human types (e.g. "Return to Settings" for "Settings").
///
/// An element on screen wins over one that is not: a real iPhone's home
/// screen carries a zero-size twin of every icon ahead of the visible one,
/// and tapping that twin's centre taps the top-left corner. Only when nothing
/// on screen matches is a zero-size node returned.
pub fn find<'a>(nodes: &'a [AxNode], query: Query<'_>) -> Option<&'a AxNode> {
    find_where(nodes, query, is_on_screen).or_else(|| find_where(nodes, query, |_| true))
}

/// [`find`], among elements on screen only: what `wait` and `assert` see,
/// where a hidden twin must neither pass for the element nor keep it from
/// being gone.
pub fn find_on_screen<'a>(nodes: &'a [AxNode], query: Query<'_>) -> Option<&'a AxNode> {
    find_where(nodes, query, is_on_screen)
}

/// Whether `node` has an area (a zero-size frame is never on screen).
pub fn is_on_screen(node: &AxNode) -> bool {
    node.frame.w > 0.0 && node.frame.h > 0.0
}

fn find_where<'a>(nodes: &'a [AxNode], query: Query<'_>, shown: fn(&AxNode) -> bool) -> Option<&'a AxNode> {
    fn walk<'a>(nodes: &'a [AxNode], pred: &dyn Fn(&AxNode) -> bool) -> Option<&'a AxNode> {
        for n in nodes {
            if pred(n) {
                return Some(n);
            }
            if let Some(found) = walk(&n.children, pred) {
                return Some(found);
            }
        }
        None
    }
    match query {
        Query::Id(id) => walk(nodes, &|n| shown(n) && n.identifier.as_deref() == Some(id)),
        Query::Label(label) => walk(nodes, &|n| shown(n) && n.label.as_deref() == Some(label)).or_else(|| {
            let needle = label.to_lowercase();
            walk(nodes, &|n| shown(n) && n.label.as_deref().is_some_and(|l| l.to_lowercase().contains(&needle)))
        }),
    }
}

/// A node's center, in the same logical-point space as its `frame`.
pub fn center(node: &AxNode) -> (f64, f64) {
    node.frame.center()
}

/// The element under `pt` for the panel's inspector: the smallest on-screen
/// node whose frame (in `rect_of`'s space) contains it, the deepest on a tie
/// — so a button wins over the full-screen container around it.
pub fn hit_test<'a>(nodes: &'a [AxNode], pt: (f64, f64), rect_of: &dyn Fn(&AxNode) -> Rect) -> Option<&'a AxNode> {
    let mut best: Option<(f64, usize, &AxNode)> = None;
    for flat in flatten(nodes, usize::MAX) {
        if !is_on_screen(flat.node) {
            continue;
        }
        let r = rect_of(flat.node);
        let inside = r.w > 0.0 && r.h > 0.0 && (r.x..=r.x + r.w).contains(&pt.0) && (r.y..=r.y + r.h).contains(&pt.1);
        let area = r.w * r.h;
        if inside && best.is_none_or(|(a, d, _)| area < a || (area == a && flat.depth > d)) {
            best = Some((area, flat.depth, flat.node));
        }
    }
    best.map(|(_, _, node)| node)
}

/// How a command names an element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    /// `--id`: its identifier, which no other element on screen has.
    Id(String),
    /// `--label`: its exact label, which no other element on screen has.
    Label(String),
}

/// The selector that names `node` and nothing else on screen ([`find`]
/// would land on it): its identifier, else its label; `None` when neither
/// does (a point is then the only way to name it).
pub fn unique_selector(nodes: &[AxNode], node: &AxNode) -> Option<Selector> {
    let shown: Vec<&AxNode> = flatten(nodes, usize::MAX).into_iter().map(|f| f.node).filter(|n| is_on_screen(n)).collect();
    let only = |same: &dyn Fn(&AxNode) -> bool| shown.iter().filter(|n| same(n)).count() == 1;
    if let Some(id) = node.identifier.as_deref().filter(|id| !id.is_empty())
        && only(&|n| n.identifier.as_deref() == Some(id))
    {
        return Some(Selector::Id(id.to_owned()));
    }
    let label = node.label.as_deref().filter(|l| !l.is_empty())?;
    only(&|n| n.label.as_deref() == Some(label)).then(|| Selector::Label(label.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{self, Size};
    use crate::Orientation;

    const PORTRAIT_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/ax_describe_safari_portrait.json");
    const LANDSCAPE_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/ax_describe_safari_landscape_left.json");
    const FRONTMOST_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/ax_frontmost_safari.json");

    #[test]
    fn parses_the_portrait_fixture_root_and_frame() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        assert_eq!(nodes.len(), 1);
        let root = &nodes[0];
        assert_eq!(root.role, "Application");
        assert_eq!(root.label.as_deref(), Some("Safari"));
        assert_eq!(root.frame, Rect::new(0.0, 0.0, 402.0, 874.0));
        assert!(!root.children.is_empty());
    }

    #[test]
    fn parses_string_and_null_ax_values() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        let heading = find(&nodes, Query::Label("Example Domain")).unwrap();
        assert_eq!(heading.value.as_deref(), Some("1"));
        let back = find(&nodes, Query::Label("Back")).unwrap();
        assert_eq!(back.value, None);
    }

    #[test]
    fn find_by_id_is_exact() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        let node = find(&nodes, Query::Id("ReloadButton")).unwrap();
        assert_eq!(node.label.as_deref(), Some("refresh"));
        assert!(find(&nodes, Query::Id("reloadbutton")).is_none(), "id lookup is exact, not case-insensitive");
    }

    #[test]
    fn find_by_label_falls_back_to_case_insensitive_contains() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        assert!(find(&nodes, Query::Label("Address")).is_some(), "exact match");
        let fuzzy = find(&nodes, Query::Label("settings")).unwrap();
        assert_eq!(fuzzy.label.as_deref(), Some("Return to Settings"));
    }

    #[test]
    fn find_returns_none_for_no_match() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        assert!(find(&nodes, Query::Label("nonexistent-thing")).is_none());
    }

    #[test]
    fn flatten_is_depth_first_and_respects_cap() {
        let nodes = parse_describe(PORTRAIT_FIXTURE).unwrap();
        let flat = flatten(&nodes, 500);
        assert_eq!(flat[0].node.role, "Application");
        assert_eq!(flat[0].depth, 0);
        assert!(flat[1].depth == 1, "first child of the root is depth 1");
        assert!(flat.len() > 1);

        let capped = flatten(&nodes, 2);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn parses_frontmost() {
        let f = parse_frontmost(FRONTMOST_FIXTURE).unwrap();
        assert_eq!(f.bundle_id, "com.apple.mobilesafari");
        assert_eq!(f.pid, 23104);
    }

    #[test]
    fn parses_frontmost_from_a_bare_result_object() {
        let bare = br#"{"pid":1,"bundleId":"com.apple.springboard"}"#;
        let f = parse_frontmost(bare).unwrap();
        assert_eq!(f.bundle_id, "com.apple.springboard");
    }

    /// The landscape fixture's Address element sits near the display's top
    /// (small logical `y` in the 874×402 root frame); converted to
    /// portrait-normalized via geometry for `LandscapeLeft`, that should land
    /// near the *buffer's* right edge — see
    /// `geometry::landscape_left_buffer_right_edge_is_display_top`.
    #[test]
    fn landscape_address_center_converts_to_portrait_near_the_buffer_edge() {
        let nodes = parse_describe(LANDSCAPE_FIXTURE).unwrap();
        let address = find(&nodes, Query::Label("Address")).unwrap();
        let (cx, cy) = center(address);
        // Sanity check in logical/display space first: near the top.
        assert!(cy / 402.0 < 0.15, "address bar should be near the display top, got y={cy}");

        let (px, py) = geometry::logical_points_to_portrait_normalized(
            Orientation::LandscapeLeft,
            (cx, cy),
            Size::new(874.0, 402.0),
        );
        assert!(px > 0.85, "expected near the portrait buffer's right edge, got px={px}");
        assert!((py - 0.5).abs() < 0.1, "expected roughly mid-height, got py={py}");
    }

    fn node(role: &str, label: Option<&str>, id: Option<&str>, frame: (f64, f64, f64, f64), children: Vec<AxNode>) -> AxNode {
        AxNode {
            label: label.map(Into::into),
            identifier: id.map(Into::into),
            value: None,
            role: role.into(),
            role_description: String::new(),
            enabled: true,
            frame: Rect::new(frame.0, frame.1, frame.2, frame.3),
            children,
        }
    }

    fn screen_tree() -> Vec<AxNode> {
        vec![node("Application", Some("App"), None, (0.0, 0.0, 400.0, 800.0), vec![
            node("Other", None, None, (0.0, 500.0, 400.0, 300.0), vec![
                node("Button", Some("Sign in"), Some("login_btn"), (120.0, 600.0, 162.0, 44.0), vec![]),
                node("Button", Some("Help"), None, (10.0, 600.0, 60.0, 44.0), vec![]),
            ]),
            node("Button", Some("Help"), None, (330.0, 20.0, 60.0, 44.0), vec![]),
            node("Icon", Some("Ghost"), None, (0.0, 0.0, 0.0, 0.0), vec![]),
        ])]
    }

    #[test]
    fn the_smallest_element_under_the_pointer_is_hit() {
        let tree = screen_tree();
        let same = |n: &AxNode| n.frame;
        assert_eq!(hit_test(&tree, (200.0, 620.0), &same).and_then(|n| n.label.as_deref()), Some("Sign in"));
        assert_eq!(hit_test(&tree, (200.0, 520.0), &same).map(|n| n.role.as_str()), Some("Other"), "the container, nothing smaller there");
        assert_eq!(hit_test(&tree, (200.0, 100.0), &same).map(|n| n.role.as_str()), Some("Application"));
        assert!(hit_test(&tree, (500.0, 100.0), &same).is_none(), "outside every element");
        assert_ne!(hit_test(&tree, (0.0, 0.0), &same).and_then(|n| n.label.as_deref()), Some("Ghost"), "a zero-size node is never hit");
        // In another space (here: halved), the same element.
        let half = |n: &AxNode| Rect::new(n.frame.x / 2.0, n.frame.y / 2.0, n.frame.w / 2.0, n.frame.h / 2.0);
        assert_eq!(hit_test(&tree, (100.0, 310.0), &half).and_then(|n| n.label.as_deref()), Some("Sign in"));
    }

    #[test]
    fn a_selector_names_the_element_and_nothing_else() {
        let tree = screen_tree();
        let find_label = |l: &str| flatten(&tree, 100).into_iter().map(|f| f.node).find(|n| n.label.as_deref() == Some(l)).unwrap();
        assert_eq!(unique_selector(&tree, find_label("Sign in")), Some(Selector::Id("login_btn".into())));
        assert_eq!(unique_selector(&tree, find_label("App")), Some(Selector::Label("App".into())));
        assert_eq!(unique_selector(&tree, find_label("Help")), None, "two Help buttons: only a point names one");
    }
}
