use super::*;
use crate::geometry::Rect;

fn node(role: &str, label: &str, frame: (f64, f64)) -> AxNode {
    AxNode {
        label: (!label.is_empty()).then(|| label.to_owned()),
        identifier: None,
        value: None,
        role: role.to_owned(),
        role_description: String::new(),
        enabled: true,
        frame: Rect::new(frame.0, frame.1, 100.0, 44.0),
        children: Vec::new(),
    }
}

/// A 402×874 screen holding `children`.
fn screen(children: Vec<AxNode>) -> Vec<AxNode> {
    let mut root = node("Application", "Demo", (0.0, 0.0));
    root.frame = Rect::new(0.0, 0.0, 402.0, 874.0);
    root.children = children;
    vec![root]
}

fn refs(table: &mut RefTable, nodes: &[AxNode]) -> Vec<(u32, String)> {
    table.assign(nodes).into_iter().map(|(r, n)| (r, n.label.clone().unwrap_or_default())).collect()
}

#[test]
fn refs_are_monotonic_and_kept_by_surviving_keys() {
    let mut table = RefTable::default();
    let login = screen(vec![node("TextField", "Email", (16.0, 300.0)), node("Button", "Sign in", (16.0, 400.0)), node("StaticText", "Welcome", (16.0, 200.0))]);
    assert_eq!(refs(&mut table, &login), [(1, "Email".into()), (2, "Sign in".into())], "only interactive elements get refs");
    // Email goes; a password field arrives: Sign in keeps 2, the new key gets 3.
    let next = screen(vec![node("SecureTextField", "Password", (16.0, 300.0)), node("Button", "Sign in", (16.0, 400.0))]);
    assert_eq!(refs(&mut table, &next), [(3, "Password".into()), (2, "Sign in".into())]);
    // Email comes back: its number was never given to anything else.
    assert_eq!(refs(&mut table, &login)[0], (1, "Email".into()));
}

#[test]
fn alike_elements_are_told_apart_by_their_place() {
    let rows = screen(vec![node("Button", "Delete", (0.0, 100.0)), node("Button", "Delete", (0.0, 200.0))]);
    let keys: Vec<NodeKey> = keyed(&rows).into_iter().filter(|k| k.key.role == "Button").map(|k| k.key).collect();
    assert_eq!(keys.iter().map(|k| k.nth).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(keyed(&rows).iter().filter(|k| k.key.role == "Button").map(|k| k.group).collect::<Vec<_>>(), [2, 2]);
    let mut table = RefTable::default();
    assert_eq!(table.assign(&rows).iter().map(|(r, _)| *r).collect::<Vec<_>>(), [1, 2]);
    // One row deleted: which ref the survivor was cannot be told.
    let one = screen(vec![node("Button", "Delete", (0.0, 100.0))]);
    assert_eq!(table.resolve(2, &one, 1.0).unwrap_err(), RefError::Gone);
    assert_eq!(table.resolve(1, &one, 1.0).unwrap_err(), RefError::Ambiguous);
}

#[test]
fn an_identifier_names_an_element_before_its_label() {
    let mut field = node("TextField", "Email", (0.0, 300.0));
    field.identifier = Some("email".into());
    let mut table = RefTable::default();
    table.assign(&screen(vec![field.clone()]));
    // Relabelled (a localized string), same identifier: the same ref.
    field.label = Some("Courriel".into());
    assert_eq!(table.assign(&screen(vec![field])).first().map(|(r, _)| *r), Some(1));
}

#[test]
fn a_changed_value_is_one_line_out_and_one_in() {
    let mut table = RefTable::default();
    let before = screen(vec![node("TextField", "Email", (16.0, 300.0)), node("Button", "Next", (16.0, 400.0))]);
    table.assign(&before);
    let mut typed = node("TextField", "Email", (16.0, 300.0));
    typed.value = Some("a@b.co".into());
    let after = screen(vec![typed, node("Button", "Next", (16.0, 400.0)), node("Button", "Forgot?", (16.0, 500.0))]);
    table.assign(&after);
    let d = diff(&before, &after, &table);
    let lines: Vec<(DiffOp, Option<u32>, Option<&str>)> =
        d.lines.iter().map(|l| (l.op, l.r, l.node.value.as_deref().or(l.node.label.as_deref()))).collect();
    assert_eq!(
        lines,
        [(DiffOp::Removed, Some(1), Some("Email")), (DiffOp::Added, Some(1), Some("a@b.co")), (DiffOp::Added, Some(3), Some("Forgot?"))]
    );
    // The app's root and Next are unchanged.
    assert_eq!(d.unchanged, 2);
    // Gone elements come first.
    let d = diff(&after, &before, &table);
    assert_eq!(d.lines.iter().map(|l| (l.op, l.r)).collect::<Vec<_>>()[0], (DiffOp::Removed, Some(3)));
}

#[test]
fn the_hash_ignores_the_clock_and_spinners_but_not_content() {
    let base = |clock: &str, spinner: f64| {
        let mut spin = node("ActivityIndicator", "Loading", (180.0, spinner));
        spin.value = Some(format!("{spinner}"));
        // A status-bar clock: 22 pt tall, inside the top 5% of 874 pt.
        let mut clock = node("StaticText", clock, (30.0, 14.0));
        clock.frame.h = 22.0;
        screen(vec![clock, spin, node("Button", "Sign in", (16.0, 400.0))])
    };
    let h = ax_hash(&base("9:41", 500.0), 1.0);
    assert_eq!(h, ax_hash(&base("9:42", 520.0), 1.0), "a status-bar clock and a spinner are not change");
    let mut moved = base("9:41", 500.0);
    moved[0].children[2].frame.y = 420.0;
    assert_ne!(h, ax_hash(&moved, 1.0));
    // Within the 2-point grid, a frame jitter is not change (3 px per point).
    let jitter = |y: f64| screen(vec![node("Button", "Sign in", (16.0, y))]);
    assert_eq!(ax_hash(&jitter(1200.0), 3.0), ax_hash(&jitter(1201.0), 3.0));
}

#[test]
fn a_ref_resolves_where_its_element_is_now() {
    let mut table = RefTable::default();
    table.assign(&screen(vec![node("Button", "Sign in", (16.0, 400.0))]));
    let near = screen(vec![node("Button", "Sign in", (16.0, 405.0))]);
    assert!(table.resolve(1, &near, 1.0).unwrap().moved_pt < MOVED_PT);
    let far = screen(vec![node("Button", "Sign in", (16.0, 460.0))]);
    let found = table.resolve(1, &far, 1.0).unwrap();
    assert!((found.moved_pt - 60.0).abs() < 1e-9 && found.node.frame.y == 460.0);
    // Android: 3 display pixels per dp, so 24 px is 8 dp.
    assert!((table.resolve(1, &screen(vec![node("Button", "Sign in", (16.0, 424.0))]), 3.0).unwrap().moved_pt - 8.0).abs() < 1e-9);
    assert_eq!(table.resolve(1, &screen(vec![]), 1.0).unwrap_err(), RefError::Gone);
    assert_eq!(table.resolve(9, &near, 1.0).unwrap_err(), RefError::Unknown);
    assert_eq!(find_key(&far, table.key_of(1).unwrap()).map(|n| n.frame.y), Some(460.0));
}

#[test]
fn tables_belong_to_a_caller_and_outlive_the_session() {
    let worktree = WorktreeKey::from_path(std::path::Path::new("/w"));
    let device = DeviceId("SIM".into());
    let mut store = RefStore::default();
    let nodes = screen(vec![node("Button", "A", (0.0, 100.0)), node("Button", "B", (0.0, 200.0))]);
    store.table(&(worktree.clone(), device.clone(), Some(1))).assign(&nodes);
    // A second caller numbers the same screen from 1, in its own table.
    let other = store.table(&(worktree.clone(), device.clone(), Some(2)));
    other.assign(&nodes[0].children[1..]);
    assert_eq!(other.ref_of(&keyed(&nodes)[2].key), Some(1));
    // Nothing about a session is in the key: a restarted stream finds the
    // same table and the same refs.
    assert_eq!(store.table(&(worktree.clone(), device.clone(), Some(1))).ref_of(&keyed(&nodes)[2].key), Some(2));
    // At most MAX_CALLERS per device; the least recently used goes.
    for caller in 3..=(MAX_CALLERS as u64 + 1) {
        store.table(&(worktree.clone(), device.clone(), Some(caller)));
    }
    assert_eq!(store.len(), MAX_CALLERS);
    assert!(store.table(&(worktree.clone(), device.clone(), Some(2))).key_of(1).is_none(), "caller 2 was the oldest");
    store.forget_worktree(&worktree);
    assert!(store.is_empty());
}

/// A real iPhone's home screen: a zero-size twin of each icon comes first in
/// the tree. Only the visible one is keyed, numbered and found.
#[test]
fn zero_size_twins_are_not_on_screen() {
    let mut twin = node("Icon", "Settings", (0.0, 0.0));
    twin.frame = Rect::new(0.0, 0.0, 0.0, 0.0);
    let shown = node("Icon", "Settings", (228.0, 508.0));
    let home = screen(vec![twin.clone(), shown]);
    let icons: Vec<_> = keyed(&home).into_iter().filter(|k| k.key.role == "Icon").collect();
    assert_eq!((icons.len(), icons[0].key.nth, icons[0].group), (1, 0, 1));
    let mut table = RefTable::default();
    assert_eq!(table.assign(&home).iter().map(|(_, n)| n.frame.y).collect::<Vec<_>>(), [508.0], "an icon gets a ref");
    assert_eq!(crate::ax::find(&home, crate::ax::Query::Label("Settings")).map(|n| n.frame.x), Some(228.0));
    // Nothing visible matches: the zero-size node is still found.
    let hidden = screen(vec![twin]);
    assert!(crate::ax::find(&hidden, crate::ax::Query::Label("Settings")).is_some());
}
