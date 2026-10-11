//! How `oximux sim` prints accessibility elements, settle reports and refs —
//! for a person (one line each) and for `--json`.

use oximux_remote_proto::simulator::{
    SimAxNodeWire, SimDiffLineWire, SimDiffOpWire, SimPointWire, SimRefNodeWire, SimSettleOutcomeWire, SimSettleWire,
};
use serde_json::{Value, json};

pub fn ax_json(n: &SimAxNodeWire) -> Value {
    json!({
        "depth": n.depth,
        "role": n.role,
        "label": n.label,
        "identifier": n.identifier,
        "value": n.value,
        "enabled": n.enabled,
        "frame": { "x": n.frame[0], "y": n.frame[1], "width": n.frame[2], "height": n.frame[3] },
    })
}

pub fn ax_line(n: &SimAxNodeWire, flat: bool) -> String {
    let indent = if flat { String::new() } else { "  ".repeat(n.depth as usize) };
    let [x, y, w, h] = n.frame;
    format!("{indent}{}  [{x:.0},{y:.0} {w:.0}×{h:.0}]", element(n))
}

/// Role, “label”, #identifier, = value, (disabled): an element in words.
fn element(n: &SimAxNodeWire) -> String {
    let mut line = n.role.clone();
    if let Some(label) = n.label.as_deref().filter(|l| !l.is_empty()) {
        line.push_str(&format!(" “{label}”"));
    }
    if let Some(id) = n.identifier.as_deref().filter(|i| !i.is_empty()) {
        line.push_str(&format!(" #{id}"));
    }
    if let Some(value) = n.value.as_deref().filter(|v| !v.is_empty()) {
        line.push_str(&format!(" = {value}"));
    }
    if !n.enabled {
        line.push_str(" (disabled)");
    }
    line
}

fn ref_word(r: Option<u32>) -> String {
    r.map(|r| format!("@e{r} ")).unwrap_or_default()
}

/// `ax -i`: `@e7 Button “Sign in”  [16,400 370×44]`.
pub fn refs_human(nodes: &[SimRefNodeWire]) -> String {
    if nodes.is_empty() {
        return "(nothing on screen can be acted on)".into();
    }
    nodes.iter().map(|n| format!("@e{} {}", n.r, ax_line(&n.node, true))).collect::<Vec<_>>().join("\n")
}

pub fn refs_json(nodes: &[SimRefNodeWire]) -> Value {
    json!({ "nodes": nodes.iter().map(|n| json!({ "ref": n.r, "node": ax_json(&n.node) })).collect::<Vec<_>>() })
}

fn outcome_str(o: SimSettleOutcomeWire) -> &'static str {
    match o {
        SimSettleOutcomeWire::Settled => "settled",
        SimSettleOutcomeWire::SettledFrame => "settled-frame",
        SimSettleOutcomeWire::FrameBusy => "frame-busy",
        SimSettleOutcomeWire::AxUnstable => "ax-unstable",
    }
}

fn point_json(p: &SimPointWire) -> Value {
    json!({ "x": p.x, "y": p.y })
}

fn diff_json(l: &SimDiffLineWire) -> Value {
    let op = match l.op {
        SimDiffOpWire::Added => "+",
        SimDiffOpWire::Removed => "-",
    };
    json!({ "op": op, "ref": l.r, "node": ax_json(&l.node) })
}

pub fn settle_json(s: &SimSettleWire) -> Value {
    json!({
        "outcome": s.outcome.map(outcome_str),
        "elapsed_ms": s.elapsed_ms,
        "target": s.target.as_ref().map(ax_json),
        "at": s.at.as_ref().map(point_json),
        "warnings": s.warnings,
        "diff": s.diff.iter().map(diff_json).collect::<Vec<_>>(),
        "unchanged": s.unchanged,
    })
}

/// `ok tap @e7 “Sign in” (201,612) settled 640ms`, then a timeout line if
/// the cap ran out, warnings, the `+`/`-` lines and `= N unchanged`.
/// `named` says the verb already names its element (by label or id), so the
/// target is not repeated.
pub fn settle_human(verb: &str, s: &SimSettleWire, named: bool) -> String {
    let mut head = format!("ok {verb}");
    if let Some(target) = s.target.as_ref().filter(|_| !named) {
        head.push_str(&format!(" {}", target.label.as_deref().or(target.identifier.as_deref()).map_or_else(|| target.role.clone(), |l| format!("“{l}”"))));
    }
    if let Some(at) = &s.at {
        head.push_str(&format!(" ({:.0},{:.0})", at.x, at.y));
    }
    let ms = s.elapsed_ms;
    match s.outcome {
        Some(SimSettleOutcomeWire::Settled) => head.push_str(&format!(" settled {ms}ms")),
        Some(SimSettleOutcomeWire::SettledFrame) => head.push_str(&format!(" settled (frame) {ms}ms")),
        Some(SimSettleOutcomeWire::FrameBusy | SimSettleOutcomeWire::AxUnstable) => head.push_str(&format!(" {ms}ms")),
        None => {}
    }
    let mut lines = vec![head];
    if let Some(o @ (SimSettleOutcomeWire::FrameBusy | SimSettleOutcomeWire::AxUnstable)) = s.outcome {
        lines.push(format!("settle: timeout ({})", outcome_str(o)));
    }
    lines.extend(s.warnings.iter().map(|w| format!("warning: {w}")));
    for l in &s.diff {
        let op = match l.op {
            SimDiffOpWire::Added => '+',
            SimDiffOpWire::Removed => '-',
        };
        lines.push(format!("{op} {}{}", ref_word(l.r), element(&l.node)));
    }
    // Only after a read: a settle that never read the tree counted nothing.
    if s.outcome.is_some() && (s.unchanged > 0 || !s.diff.is_empty()) {
        lines.push(format!("= {} unchanged", s.unchanged));
    }
    lines.join("\n")
}

/// `wait`/`assert` held: the element as found, or that it is gone.
pub fn matched_human(what: &str, node: Option<&SimAxNodeWire>, elapsed_ms: u32) -> String {
    match node {
        Some(n) => format!("ok {what} after {elapsed_ms}ms: {}", ax_line(n, true)),
        None => format!("ok {what} is gone after {elapsed_ms}ms"),
    }
}

pub fn matched_json(node: Option<&SimAxNodeWire>, elapsed_ms: u32) -> Value {
    json!({ "found": node.is_some(), "node": node.map(ax_json), "elapsed_ms": elapsed_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, label: &str) -> SimAxNodeWire {
        SimAxNodeWire {
            depth: 2,
            role: role.into(),
            label: Some(label.into()),
            identifier: None,
            value: None,
            enabled: true,
            frame: [16.0, 300.0, 370.0, 44.0],
        }
    }

    #[test]
    fn ax_lines_read_like_the_tree() {
        let node = SimAxNodeWire {
            depth: 2,
            role: "Button".into(),
            label: Some("Settings".into()),
            identifier: Some("settings".into()),
            value: None,
            enabled: false,
            frame: [10.0, 20.5, 100.0, 44.0],
        };
        assert_eq!(ax_line(&node, false), "    Button “Settings” #settings (disabled)  [10,20 100×44]");
        assert!(ax_line(&node, true).starts_with("Button"));
    }

    #[test]
    fn a_settle_report_reads_as_one_head_line_then_the_diff() {
        let report = SimSettleWire {
            outcome: Some(SimSettleOutcomeWire::Settled),
            elapsed_ms: 640,
            diff: vec![
                SimDiffLineWire { op: SimDiffOpWire::Removed, r: Some(7), node: node("Button", "Sign in") },
                SimDiffLineWire { op: SimDiffOpWire::Added, r: Some(31), node: node("TextField", "Email") },
                SimDiffLineWire { op: SimDiffOpWire::Added, r: None, node: node("StaticText", "Welcome") },
            ],
            unchanged: 12,
            warnings: vec![],
            target: Some(node("Button", "Sign in")),
            at: Some(SimPointWire { x: 201.0, y: 612.4 }),
        };
        assert_eq!(
            settle_human("tap @e7", &report, false),
            "ok tap @e7 “Sign in” (201,612) settled 640ms\n- @e7 Button “Sign in”\n+ @e31 TextField “Email”\n+ StaticText “Welcome”\n= 12 unchanged"
        );
        let v = settle_json(&report);
        assert_eq!((v["outcome"].as_str(), v["diff"][1]["ref"].as_u64(), v["diff"][0]["op"].as_str()), (Some("settled"), Some(31), Some("-")));
    }

    #[test]
    fn a_cap_that_ran_out_says_why_and_still_shows_the_diff() {
        let report = SimSettleWire {
            outcome: Some(SimSettleOutcomeWire::FrameBusy),
            elapsed_ms: 6100,
            diff: vec![],
            unchanged: 3,
            warnings: vec!["moved: @e2 was 24 pt from where it was last seen; tapped where it is now".into()],
            target: None,
            at: None,
        };
        let out = settle_human("tap “Next”", &report, true);
        assert_eq!(out.lines().collect::<Vec<_>>()[..2], ["ok tap “Next” 6100ms", "settle: timeout (frame-busy)"]);
        assert!(out.contains("warning: moved") && out.ends_with("= 3 unchanged"));
        let unread = SimSettleWire { unchanged: 0, warnings: vec!["no accessibility read: the screen kept changing".into()], ..report.clone() };
        assert!(!settle_human("swipe", &unread, true).contains("unchanged"), "nothing was counted");
        // Not asked to settle (a bare `tap @e7`): no outcome, no counts.
        let bare = SimSettleWire { outcome: None, warnings: vec![], ..report };
        assert_eq!(settle_human("tap @e2", &bare, false), "ok tap @e2");
    }
}
