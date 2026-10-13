//! Naming an element exactly — what a recorded flow uses, so a replay never
//! lands on "Sign in with Apple" for "Sign in" (`ax::find`'s label lookup
//! falls back to a substring; this never does).
//!
//! An exact selector is a label and/or identifier that must be equal, a role
//! that must be equal, and which match to take (`nth`, from 0, in tree
//! order) — among elements on screen only.

use crate::ax::{AxNode, flatten, is_on_screen};

/// An element named exactly. Owned: it is what a recording keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exact {
    pub label: Option<String>,
    pub id: Option<String>,
    pub role: Option<String>,
    pub nth: usize,
}

impl Exact {
    fn matches(&self, n: &AxNode) -> bool {
        let same = |want: &Option<String>, have: &Option<String>| want.as_ref().is_none_or(|w| have.as_ref() == Some(w));
        same(&self.label, &n.label) && same(&self.id, &n.identifier) && self.role.as_ref().is_none_or(|r| *r == n.role)
    }

    /// Whether it names anything (a selector with no field matches every
    /// element, which is never what a step means).
    pub fn is_empty(&self) -> bool {
        self.label.is_none() && self.id.is_none() && self.role.is_none()
    }
}

/// What typing after a tap on `node` goes into: `Some(true)` a password
/// field, `Some(false)` a plain text field, `None` anything else (a label, a
/// field's hint, a button — where the typing lands is not known).
pub fn field_kind(node: &AxNode) -> Option<bool> {
    if node.secure {
        return Some(true);
    }
    matches!(node.role.as_str(), "TextField" | "SearchField" | "TextView" | "TextArea").then_some(false)
}

/// The `sel.nth`-th element on screen matching `sel` exactly.
pub fn find_exact<'a>(nodes: &'a [AxNode], sel: &Exact) -> Option<&'a AxNode> {
    if sel.is_empty() {
        return None;
    }
    flatten(nodes, usize::MAX).into_iter().map(|f| f.node).filter(|n| is_on_screen(n) && sel.matches(n)).nth(sel.nth)
}

/// The exact selector that names `node` and nothing else on screen: its
/// identifier, else its label, when either is unique; else its label (or,
/// without one, its role) and its position among those. `None` when `node`
/// is not on screen, or has neither label nor role.
pub fn exact_selector(nodes: &[AxNode], node: &AxNode) -> Option<Exact> {
    if !is_on_screen(node) {
        return None;
    }
    let shown: Vec<&AxNode> = flatten(nodes, usize::MAX).into_iter().map(|f| f.node).filter(|n| is_on_screen(n)).collect();
    // Where `node` falls among those `sel` matches.
    let place = |sel: &Exact| shown.iter().filter(|n| sel.matches(n)).position(|n| std::ptr::eq(*n, node));
    let count = |sel: &Exact| shown.iter().filter(|n| sel.matches(n)).count();
    let by_id = node.identifier.clone().filter(|i| !i.is_empty()).map(|id| Exact { id: Some(id), ..Exact::default() });
    if let Some(sel) = by_id.filter(|s| count(s) == 1) {
        return Some(sel);
    }
    let by_label = node.label.clone().filter(|l| !l.is_empty()).map(|label| Exact { label: Some(label), ..Exact::default() });
    let mut sel = match by_label {
        Some(sel) if count(&sel) == 1 => return Some(sel),
        Some(sel) => sel,
        None if !node.role.is_empty() => Exact { role: Some(node.role.clone()), ..Exact::default() },
        None => return None,
    };
    sel.nth = place(&sel)?;
    Some(sel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    fn n(role: &str, label: Option<&str>, id: Option<&str>, x: f64) -> AxNode {
        AxNode {
            label: label.map(Into::into),
            identifier: id.map(Into::into),
            value: None,
            role: role.into(),
            role_description: String::new(),
            enabled: true,
            secure: false,
            frame: Rect::new(x, 0.0, 10.0, 10.0),
            children: vec![],
        }
    }

    fn screen() -> Vec<AxNode> {
        let mut root = n("Application", Some("App"), None, 0.0);
        root.frame = Rect::new(0.0, 0.0, 400.0, 800.0);
        root.children = vec![
            n("Button", Some("Sign in"), None, 10.0),
            n("Button", Some("Sign in with Apple"), None, 20.0),
            n("Button", Some("Delete"), None, 30.0),
            n("Button", Some("Delete"), Some("row2_delete"), 40.0),
            n("Image", None, None, 50.0),
            n("Image", None, None, 60.0),
        ];
        vec![root]
    }

    #[test]
    fn an_exact_label_never_matches_part_of_another() {
        let nodes = screen();
        let sel = Exact { label: Some("Sign in".into()), ..Exact::default() };
        assert_eq!(find_exact(&nodes, &sel).map(|n| n.frame.x), Some(10.0));
        let sel = Exact { label: Some("sign in".into()), ..Exact::default() };
        assert!(find_exact(&nodes, &sel).is_none(), "case counts");
        let second = Exact { label: Some("Delete".into()), nth: 1, ..Exact::default() };
        assert_eq!(find_exact(&nodes, &second).map(|n| n.frame.x), Some(40.0));
        let images = Exact { role: Some("Image".into()), nth: 1, ..Exact::default() };
        assert_eq!(find_exact(&nodes, &images).map(|n| n.frame.x), Some(60.0));
        assert!(find_exact(&nodes, &Exact::default()).is_none(), "an empty selector names nothing");
    }

    #[test]
    fn a_selector_is_unique_or_says_which_one() {
        let nodes = screen();
        let kids = &nodes[0].children;
        assert_eq!(exact_selector(&nodes, &kids[0]), Some(Exact { label: Some("Sign in".into()), ..Exact::default() }));
        assert_eq!(exact_selector(&nodes, &kids[3]), Some(Exact { id: Some("row2_delete".into()), ..Exact::default() }));
        assert_eq!(exact_selector(&nodes, &kids[2]), Some(Exact { label: Some("Delete".into()), nth: 0, ..Exact::default() }));
        assert_eq!(exact_selector(&nodes, &kids[5]), Some(Exact { role: Some("Image".into()), nth: 1, ..Exact::default() }));
        for kid in kids {
            let sel = exact_selector(&nodes, kid).unwrap();
            assert!(std::ptr::eq(find_exact(&nodes, &sel).unwrap(), kid), "{sel:?} finds its own element");
        }
    }
}
