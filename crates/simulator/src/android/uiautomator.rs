//! `uiautomator dump` into the same [`AxNode`] tree the iOS helper's AX reply
//! becomes, so `oximux sim ax` / `tap --label` work unchanged on Android.
//!
//! Frames are in **display pixels, current rotation** (uiautomator's
//! `bounds="[x1,y1][x2,y2]"`) — the same space the video stream is in, so an
//! Android device's AX-to-screen scale is 1.

use std::time::Duration;

use crate::ax::AxNode;
use crate::geometry::Rect;
use crate::runner::Runner;
use crate::{Result, SimError};

/// Parse a dump. adb appends a status line after the XML
/// (`UI hierchary dumped to: /dev/tty`), so only the `<hierarchy>` element
/// is read.
pub fn parse_dump(out: &str) -> Result<Vec<AxNode>> {
    let start = out.find("<hierarchy").ok_or_else(|| parse_error("no <hierarchy> in the dump"))?;
    let end = out.rfind("</hierarchy>").ok_or_else(|| parse_error("the dump ends early"))? + "</hierarchy>".len();
    let doc = roxmltree::Document::parse(&out[start..end]).map_err(|e| parse_error(&e.to_string()))?;
    Ok(doc.root_element().children().filter(|n| n.has_tag_name("node")).map(node).collect())
}

/// `adb -s S exec-out uiautomator dump /dev/tty`, parsed. Slow (a second or
/// two): call from a background executor.
pub fn describe(runner: &dyn Runner, adb: &std::path::Path, serial: &str, timeout: Duration) -> Result<Vec<AxNode>> {
    let out = runner
        .run(&adb.to_string_lossy(), &["-s", serial, "exec-out", "uiautomator", "dump", "/dev/tty"], None, timeout)?
        .into_success("adb")?;
    parse_dump(&out.stdout_str())
}

fn node(n: roxmltree::Node<'_, '_>) -> AxNode {
    let attr = |key: &str| n.attribute(key).filter(|v| !v.is_empty()).map(str::to_owned);
    let class = n.attribute("class").unwrap_or_default();
    let (text, desc) = (attr("text"), attr("content-desc"));
    let role = role(class, n.attribute("clickable") == Some("true"));
    // The description is what TalkBack reads; the text is what is shown.
    let label = desc.clone().or_else(|| text.clone());
    let value = if desc.is_some() || role == "TextField" { text } else { None };
    AxNode {
        label,
        identifier: attr("resource-id"),
        value,
        role: role.to_owned(),
        role_description: class.to_owned(),
        enabled: n.attribute("enabled") != Some("false"),
        frame: n.attribute("bounds").and_then(parse_bounds).unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0)),
        children: n.children().filter(|c| c.has_tag_name("node")).map(node).collect(),
    }
}

/// An iOS-style role for an Android widget class, so the same vocabulary
/// (`Button`, `TextField`…) reads across platforms.
fn role(class: &str, clickable: bool) -> &'static str {
    let short = class.rsplit('.').next().unwrap_or(class);
    match short {
        "Button" | "ImageButton" | "FloatingActionButton" | "MaterialButton" => "Button",
        "EditText" | "AutoCompleteTextView" | "TextInputEditText" => "TextField",
        "Switch" | "SwitchCompat" | "SwitchMaterial" | "ToggleButton" => "Switch",
        "CheckBox" | "RadioButton" => "CheckBox",
        "ImageView" => {
            if clickable { "Button" } else { "Image" }
        }
        "TextView" => {
            if clickable { "Button" } else { "StaticText" }
        }
        "ScrollView" | "HorizontalScrollView" | "RecyclerView" | "ListView" => "ScrollArea",
        "WebView" => "WebArea",
        _ if clickable => "Button",
        _ => "Group",
    }
}

/// `[x1,y1][x2,y2]` → a rect.
fn parse_bounds(bounds: &str) -> Option<Rect> {
    let nums: Vec<f64> = bounds.split(|c: char| !c.is_ascii_digit() && c != '-').filter(|s| !s.is_empty()).map(|s| s.parse().ok()).collect::<Option<_>>()?;
    let [x1, y1, x2, y2] = nums[..] else { return None };
    (x2 >= x1 && y2 >= y1).then(|| Rect::new(x1, y1, x2 - x1, y2 - y1))
}

fn parse_error(detail: &str) -> SimError {
    SimError::Parse { what: "uiautomator dump".into(), detail: detail.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::{self, Query};

    const LAUNCHER: &str = include_str!("../../tests/fixtures/uiautomator-launcher.xml");

    /// A real dump (Android 17 emulator, the launcher's home screen).
    #[test]
    fn a_real_dump_becomes_a_searchable_tree() {
        let tree = parse_dump(LAUNCHER).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].frame, Rect::new(0.0, 0.0, 1080.0, 2400.0), "the root covers the display, in pixels");
        let gmail = ax::find(&tree, Query::Label("Gmail")).expect("the Gmail icon");
        assert!(gmail.frame.w > 0.0 && gmail.frame.h > 0.0);
        assert!(gmail.enabled);
        assert!(ax::flatten(&tree, 1000).len() > 10);
    }

    #[test]
    fn widgets_read_with_ios_roles_and_labels() {
        let xml = r#"<?xml version='1.0' ?><hierarchy rotation="0">
<node class="android.widget.EditText" text="hello" content-desc="" resource-id="com.x:id/q" clickable="true" enabled="true" bounds="[10,20][110,70]"/>
<node class="android.widget.ImageButton" text="" content-desc="Search" resource-id="" clickable="true" enabled="false" bounds="[0,0][48,48]"/>
<node class="android.widget.FrameLayout" text="" content-desc="" clickable="false" bounds="bad"/>
</hierarchy>UI hierchary dumped to: /dev/tty"#;
        let tree = parse_dump(xml).unwrap();
        assert_eq!(tree[0].role, "TextField");
        assert_eq!((tree[0].label.as_deref(), tree[0].value.as_deref()), (Some("hello"), Some("hello")));
        assert_eq!(tree[0].identifier.as_deref(), Some("com.x:id/q"));
        assert_eq!(tree[0].frame, Rect::new(10.0, 20.0, 100.0, 50.0));
        assert_eq!((tree[1].role.as_str(), tree[1].label.as_deref(), tree[1].enabled), ("Button", Some("Search"), false));
        assert_eq!((tree[2].role.as_str(), tree[2].frame), ("Group", Rect::new(0.0, 0.0, 0.0, 0.0)));
        assert!(parse_dump("ERROR: could not get idle state.").is_err());
    }
}
