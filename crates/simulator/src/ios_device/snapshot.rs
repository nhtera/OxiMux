//! The runner's `snapshot` as this crate's accessibility tree ([`AxNode`]).
//!
//! The runner flattens XCTest's tree depth first — each node with its
//! `depth`, `role` (an `XCUIElement.ElementType` name: `button`,
//! `staticText`, …), `label`, `identifier`, `value`, `frame` `[x, y, w, h]`
//! in points and `enabled` — and caps it at 2000 nodes. Here the depths are
//! folded back into children, and roles read like the simulator helper's
//! (`Button`, `StaticText`), so agents see one vocabulary.

use serde::Deserialize;
use serde_json::Value;

use crate::ax::AxNode;
use crate::geometry::Rect;
use crate::{Result, SimError};

#[derive(Deserialize)]
struct Flat {
    depth: usize,
    role: String,
    label: Option<String>,
    identifier: Option<String>,
    value: Option<Value>,
    frame: [f64; 4],
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

/// The tree in a `snapshot` reply's `data`, and whether the runner cut it
/// short.
pub fn tree(data: &Value) -> Result<(Vec<AxNode>, bool)> {
    let nodes = data.get("nodes").cloned().unwrap_or(Value::Array(Vec::new()));
    let flat: Vec<Flat> = serde_json::from_value(nodes).map_err(|e| SimError::Parse { what: "runner snapshot".into(), detail: e.to_string() })?;
    let truncated = data.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    Ok((fold(flat), truncated))
}

/// Depth-first `(depth, node)` pairs back into a forest. A depth that skips
/// a level (a cut tree) hangs the node under the nearest shallower one.
fn fold(flat: Vec<Flat>) -> Vec<AxNode> {
    // The open path from a root: (depth, node), children filled as we go.
    let mut path: Vec<(usize, AxNode)> = Vec::new();
    let mut roots = Vec::new();
    for item in flat {
        let depth = item.depth;
        while path.last().is_some_and(|(d, _)| *d >= depth) {
            close(&mut path, &mut roots);
        }
        path.push((depth, node(item)));
    }
    while !path.is_empty() {
        close(&mut path, &mut roots);
    }
    roots
}

fn close(path: &mut Vec<(usize, AxNode)>, roots: &mut Vec<AxNode>) {
    let Some((_, done)) = path.pop() else { return };
    match path.last_mut() {
        Some((_, parent)) => parent.children.push(done),
        None => roots.push(done),
    }
}

fn node(flat: Flat) -> AxNode {
    let [x, y, w, h] = flat.frame;
    AxNode {
        label: flat.label.filter(|l| !l.trim().is_empty()),
        identifier: flat.identifier.filter(|i| !i.is_empty()),
        value: flat.value.and_then(|v| match v {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        }),
        role_description: words(&flat.role),
        role: capitalized(&flat.role),
        enabled: flat.enabled,
        frame: Rect::new(x, y, w, h),
        children: Vec::new(),
    }
}

/// `staticText` → `StaticText`.
fn capitalized(role: &str) -> String {
    let mut chars = role.chars();
    chars.next().map(|c| c.to_ascii_uppercase().to_string() + chars.as_str()).unwrap_or_default()
}

/// `staticText` → `static text`.
fn words(role: &str) -> String {
    let mut out = String::with_capacity(role.len() + 4);
    for c in role.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn depths_fold_back_into_children() {
        let data = json!({"nodes": [
            {"depth": 0, "enabled": true, "frame": [0, 0, 430, 932], "label": " ", "role": "application"},
            {"depth": 1, "enabled": true, "frame": [0, 0, 430, 932], "role": "window"},
            {"depth": 2, "enabled": true, "frame": [20, 60, 64, 64], "label": "Settings", "identifier": "com.apple.Preferences", "role": "icon"},
            {"depth": 2, "enabled": false, "frame": [100, 60, 64, 64], "label": "Wi-Fi", "value": 1, "role": "switch"},
            {"depth": 1, "enabled": true, "frame": [0, 900, 430, 32], "label": "Page 1 of 2", "role": "pageIndicator"},
            {"depth": 3, "enabled": true, "frame": [0, 0, 1, 1], "role": "staticText", "value": null}
        ], "truncated": true});
        let (roots, truncated) = tree(&data).unwrap();
        assert!(truncated);
        assert_eq!(roots.len(), 1);
        let app = &roots[0];
        assert_eq!((app.role.as_str(), app.label.as_deref()), ("Application", None), "a blank label is no label");
        assert_eq!(app.children.len(), 2);
        let window = &app.children[0];
        assert_eq!(window.children.len(), 2);
        let settings = &window.children[0];
        assert_eq!((settings.label.as_deref(), settings.identifier.as_deref()), (Some("Settings"), Some("com.apple.Preferences")));
        assert_eq!(settings.frame, Rect::new(20.0, 60.0, 64.0, 64.0));
        let wifi = &window.children[1];
        assert_eq!((wifi.value.as_deref(), wifi.enabled, wifi.role_description.as_str()), (Some("1"), false, "switch"));
        // A skipped level hangs under the nearest shallower node.
        let indicator = &app.children[1];
        assert_eq!((indicator.role.as_str(), indicator.role_description.as_str()), ("PageIndicator", "page indicator"));
        assert_eq!(indicator.children[0].role, "StaticText");
    }

    #[test]
    fn an_empty_or_unreadable_snapshot() {
        assert_eq!(tree(&json!({"nodes": []})).unwrap(), (Vec::new(), false));
        assert!(tree(&json!({"nodes": [{"depth": "x"}]})).is_err());
    }
}
