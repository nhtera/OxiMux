//! Agent-context capture for the browser tab: hand the live page to an AI
//! agent as pasteable text. Four read-only probes, all driven through the
//! webview's JS bridge (`window.ipc.postMessage`) — no CDP, no network
//! capture:
//!
//! - **console / errors** — a document-start init script hooks `console.*`,
//!   `onerror`, and `unhandledrejection` into capped ring buffers; a command
//!   reads them back.
//! - **DOM snapshot** — an injected depth-bounded tree-walker emits the
//!   interesting/landmark/interactive elements; the host numbers them
//!   `@ref1…` so the agent can refer to them compactly.
//! - **element picker** — an injected shadow-overlay highlights the element
//!   under the cursor; `C` copies a formatted payload, `S` screenshots its
//!   rect, `Esc` cancels. All read-only — the picker never mutates the page.
//! - **screenshot** — native (see [`super::native`]); the picker's `S` path
//!   routes a rect through it.
//!
//! Everything formats to text/markdown for the clipboard ([`super::mod`]
//! wires the buttons + IPC). Page payloads are untrusted: sizes are clamped
//! in the injected JS and the host only ever treats them as inert text.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Document-start script — re-run by the webview on every navigation, so the
/// buffers survive reloads; the `__oxInstalled` guard keeps a soft SPA
/// navigation (where the script may not re-run) from double-hooking.
pub const INIT_SCRIPT: &str = r#"
(function () {
  if (window.__oxInstalled) return;
  window.__oxInstalled = true;
  window.__oxConsole = [];
  window.__oxErrors = [];
  var CAP = 512;
  function push(arr, v) { arr.push(v); if (arr.length > CAP) arr.shift(); }
  function render(a) {
    try { return typeof a === 'string' ? a : JSON.stringify(a); }
    catch (e) { return String(a); }
  }
  ['log', 'info', 'warn', 'error', 'debug'].forEach(function (level) {
    var orig = console[level];
    console[level] = function () {
      var args = Array.prototype.slice.call(arguments);
      try { push(window.__oxConsole, { level: level, text: args.map(render).join(' ') }); } catch (e) {}
      return orig.apply(console, args);
    };
  });
  window.addEventListener('error', function (e) {
    push(window.__oxErrors, (e.message || '') + ' @ ' + (e.filename || '') + ':' + (e.lineno || 0));
  });
  window.addEventListener('unhandledrejection', function (e) {
    var r = e.reason;
    push(window.__oxErrors, 'unhandledrejection: ' + (r && r.message ? r.message : String(r)));
  });
})();
"#;

/// Read the captured console + error buffers back to the host.
pub const READ_CONSOLE_JS: &str = r#"
window.ipc.postMessage(JSON.stringify({
  kind: 'console',
  items: (window.__oxConsole || []),
  errors: (window.__oxErrors || [])
}));
"#;

/// Depth-bounded DOM walk: emit interesting / interactive / landmark
/// elements (capped) plus page title/url and a leading innerText snippet.
pub const SNAPSHOT_JS: &str = r#"
(function () {
  var out = [];
  var CAP = 200;
  function sel(el) {
    if (el.id) return '#' + CSS.escape(el.id);
    var s = el.tagName.toLowerCase();
    if (el.classList && el.classList.length) {
      s += '.' + Array.prototype.slice.call(el.classList, 0, 2).map(function (c) { return CSS.escape(c); }).join('.');
    }
    return s;
  }
  var ROLES = { A: 'link', BUTTON: 'button', INPUT: 'input', SELECT: 'select', TEXTAREA: 'textarea',
                H1: 'heading', H2: 'heading', H3: 'heading', H4: 'heading',
                NAV: 'nav', MAIN: 'main', FORM: 'form', LABEL: 'label' };
  function role(el) { return el.getAttribute('role') || ROLES[el.tagName] || ''; }
  function name(el) {
    var n = el.getAttribute('aria-label') || el.getAttribute('alt') || el.getAttribute('placeholder')
      || (el.textContent || '').trim().slice(0, 80) || '';
    return n.replace(/\s+/g, ' ');
  }
  var INTERESTING = { A: 1, BUTTON: 1, INPUT: 1, SELECT: 1, TEXTAREA: 1,
                      H1: 1, H2: 1, H3: 1, H4: 1, NAV: 1, MAIN: 1, FORM: 1, LABEL: 1 };
  function walk(el, depth) {
    if (out.length >= CAP) return;
    if (!el || el.nodeType !== 1) return;
    var r = role(el);
    if (INTERESTING[el.tagName] || r) {
      out.push({ tag: el.tagName.toLowerCase(), role: r, name: name(el), selector: sel(el), depth: depth });
    }
    var kids = el.children || [];
    for (var i = 0; i < kids.length; i++) walk(kids[i], depth + 1);
  }
  if (document.body) walk(document.body, 0);
  window.ipc.postMessage(JSON.stringify({
    kind: 'dom',
    title: document.title || '',
    url: location.href,
    text: (document.body ? document.body.innerText : '').slice(0, 2000),
    entries: out
  }));
})();
"#;

/// Inject the element-picker shadow-overlay. Hover highlights
/// `elementFromPoint`; `C` copies the element payload, `S` screenshots its
/// rect, `Esc` cancels. Listeners are capture-phase + `stopPropagation` so
/// the page's own handlers never see the picker's keys.
pub const PICKER_JS: &str = r#"
(function () {
  if (window.__oxPicker) { window.__oxPicker.stop(); }
  var host = document.createElement('div');
  host.style.cssText = 'position:fixed;inset:0;z-index:2147483647;cursor:crosshair;';
  var shadow = host.attachShadow({ mode: 'open' });
  var box = document.createElement('div');
  box.style.cssText = 'position:fixed;pointer-events:none;border:2px solid #4c8bf5;background:rgba(76,139,245,0.15);';
  var label = document.createElement('div');
  label.style.cssText = 'position:fixed;pointer-events:none;font:12px monospace;background:#111;color:#fff;padding:2px 6px;border-radius:3px;white-space:nowrap;';
  shadow.appendChild(box);
  shadow.appendChild(label);
  document.documentElement.appendChild(host);
  var cur = null;
  function sel(el) {
    if (el.id) return '#' + CSS.escape(el.id);
    var s = el.tagName.toLowerCase();
    if (el.classList && el.classList.length) {
      s += '.' + Array.prototype.slice.call(el.classList, 0, 2).map(function (c) { return CSS.escape(c); }).join('.');
    }
    return s;
  }
  function cssPath(el) {
    var parts = [];
    while (el && el.nodeType === 1 && parts.length < 8) {
      var part = sel(el);
      if (part.charAt(0) === '#') { parts.unshift(part); break; }
      parts.unshift(part);
      el = el.parentElement;
    }
    return parts.join(' > ');
  }
  function under(e) {
    host.style.pointerEvents = 'none';
    var el = document.elementFromPoint(e.clientX, e.clientY);
    host.style.pointerEvents = 'auto';
    return el;
  }
  function move(e) {
    var el = under(e);
    if (!el || el === host || el === cur) return;
    cur = el;
    var r = el.getBoundingClientRect();
    box.style.left = r.left + 'px'; box.style.top = r.top + 'px';
    box.style.width = r.width + 'px'; box.style.height = r.height + 'px';
    label.textContent = sel(el);
    label.style.left = r.left + 'px';
    label.style.top = Math.max(0, r.top - 20) + 'px';
  }
  function payload(el) {
    var r = el.getBoundingClientRect();
    var cs = getComputedStyle(el);
    var styles = {};
    ['color', 'background-color', 'font-size', 'font-family', 'display', 'padding', 'margin'].forEach(function (k) {
      styles[k] = cs.getPropertyValue(k);
    });
    return { kind: 'pick', selector: sel(el), css_path: cssPath(el),
             html: (el.outerHTML || '').slice(0, 4096), text: (el.textContent || '').trim().slice(0, 500),
             styles: styles, rect: { x: r.left, y: r.top, w: r.width, h: r.height } };
  }
  function key(e) {
    if (e.key === 'Escape') {
      window.ipc.postMessage(JSON.stringify({ kind: 'pick_cancel' }));
      stop(); e.preventDefault(); e.stopPropagation(); return;
    }
    // Leave OS chords (Cmd/Ctrl+C, Cmd/Ctrl+S, …) to the page / browser — the
    // picker only claims the bare C / S keys.
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (!cur) return;
    if (e.key === 'c' || e.key === 'C') {
      window.ipc.postMessage(JSON.stringify(payload(cur)));
      stop(); e.preventDefault(); e.stopPropagation();
    } else if (e.key === 's' || e.key === 'S') {
      var r = cur.getBoundingClientRect();
      window.ipc.postMessage(JSON.stringify({ kind: 'pick_shot', rect: { x: r.left, y: r.top, w: r.width, h: r.height } }));
      stop(); e.preventDefault(); e.stopPropagation();
    }
  }
  function stop() {
    host.remove();
    window.removeEventListener('mousemove', move, true);
    window.removeEventListener('keydown', key, true);
    window.__oxPicker = null;
  }
  window.addEventListener('mousemove', move, true);
  window.addEventListener('keydown', key, true);
  window.__oxPicker = { stop: stop };
})();
"#;

/// A captured page rect (CSS pixels, viewport-relative) — drives the
/// element screenshot.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PickRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Deserialize)]
pub struct LogEntry {
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct DomEntry {
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub selector: String,
    #[serde(default)]
    pub depth: u32,
}

#[derive(Debug, Deserialize)]
pub struct DomSnapshot {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub entries: Vec<DomEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PickPayload {
    #[serde(default)]
    pub selector: String,
    #[serde(default)]
    pub css_path: String,
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub styles: BTreeMap<String, String>,
    pub rect: PickRect,
}

/// One message from the injected probes, tagged by `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcMessage {
    Console {
        #[serde(default)]
        items: Vec<LogEntry>,
        #[serde(default)]
        errors: Vec<String>,
    },
    Dom(DomSnapshot),
    Pick(PickPayload),
    PickShot {
        rect: PickRect,
    },
    /// Picker dismissed with `Esc` — no payload, just a signal to hand the
    /// keyboard back to the host.
    PickCancel,
}

impl IpcMessage {
    /// Parse a raw IPC body; `None` if it isn't one of our probe messages.
    pub fn parse(body: &str) -> Option<Self> {
        serde_json::from_str(body).ok()
    }
}

/// Wrap page-controlled `body` in a fenced code block whose backtick run is
/// longer than any run inside it — so a page that logs/embeds ``` ``` ``` can't
/// close the fence early and corrupt the markdown handed to the agent.
fn fenced_block(lang: &str, body: &str) -> String {
    let longest = body
        .bytes()
        .fold((0usize, 0usize), |(max, cur), b| {
            if b == b'`' {
                (max.max(cur + 1), cur + 1)
            } else {
                (max, 0)
            }
        })
        .0;
    let ticks = "`".repeat(longest.max(2) + 1);
    format!("{ticks}{lang}\n{body}\n{ticks}\n")
}

/// Collapse any newlines in page-controlled text so it can't inject its own
/// markdown structure into a single-line list item.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// Format the console + error buffers as a fenced markdown block.
pub fn format_console(items: &[LogEntry], errors: &[String]) -> String {
    let mut body = String::new();
    if items.is_empty() && errors.is_empty() {
        body.push_str("(no console output captured)");
    } else {
        for it in items {
            body.push_str(&format!("[{}] {}\n", it.level, it.text));
        }
        for e in errors {
            body.push_str(&format!("[error] {e}\n"));
        }
    }
    format!("## Browser console\n\n{}", fenced_block("text", body.trim_end()))
}

/// Format the DOM snapshot, numbering entries `@ref1…` so an agent can name
/// them back compactly. Indentation tracks the walk depth (clamped).
pub fn format_dom_snapshot(snap: &DomSnapshot) -> String {
    let mut out = format!(
        "## Page snapshot\n\n- title: {}\n- url: {}\n",
        one_line(&snap.title),
        one_line(&snap.url)
    );
    if !snap.text.is_empty() {
        out.push_str(&format!("- text: {}\n", one_line(&snap.text)));
    }
    out.push_str("\n### Elements\n\n");
    if snap.entries.is_empty() {
        out.push_str("(no interactive or landmark elements found)\n");
        return out;
    }
    for (i, e) in snap.entries.iter().enumerate() {
        let indent = "  ".repeat(e.depth.min(8) as usize);
        let role = if e.role.is_empty() { e.tag.clone() } else { e.role.clone() };
        let mut line = format!("{indent}@ref{} {role} `{}`", i + 1, e.selector);
        if !e.name.is_empty() {
            line.push_str(&format!(" — {}", e.name));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Format a picked element as a markdown block: identity, styles, and a
/// clamped HTML excerpt.
pub fn format_pick(p: &PickPayload) -> String {
    let mut out = String::from("## Picked element\n\n");
    out.push_str(&format!("- selector: `{}`\n", p.selector));
    if !p.css_path.is_empty() {
        out.push_str(&format!("- path: `{}`\n", p.css_path));
    }
    out.push_str(&format!(
        "- rect: {:.0}×{:.0} at ({:.0}, {:.0})\n",
        p.rect.w, p.rect.h, p.rect.x, p.rect.y
    ));
    if !p.text.is_empty() {
        out.push_str(&format!("- text: {}\n", one_line(&p.text)));
    }
    if !p.styles.is_empty() {
        out.push_str("\n### Computed styles\n\n");
        for (k, v) in &p.styles {
            out.push_str(&format!("- {}: {}\n", one_line(k), one_line(v)));
        }
    }
    if !p.html.is_empty() {
        out.push_str("\n### HTML\n\n");
        out.push_str(&fenced_block("html", &p.html));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_message() {
        let body = r#"{"kind":"console","items":[{"level":"warn","text":"hi"}],"errors":["boom @ a.js:3"]}"#;
        match IpcMessage::parse(body).expect("parse") {
            IpcMessage::Console { items, errors } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].level, "warn");
                assert_eq!(errors, vec!["boom @ a.js:3".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_pick_and_pick_shot() {
        let pick = r##"{"kind":"pick","selector":"#go","rect":{"x":1,"y":2,"w":3,"h":4}}"##;
        assert!(matches!(IpcMessage::parse(pick), Some(IpcMessage::Pick(_))));
        let shot = r#"{"kind":"pick_shot","rect":{"x":0,"y":0,"w":10,"h":10}}"#;
        assert!(matches!(IpcMessage::parse(shot), Some(IpcMessage::PickShot { .. })));
    }

    #[test]
    fn ignores_foreign_ipc() {
        assert!(IpcMessage::parse("not json").is_none());
        assert!(IpcMessage::parse(r#"{"kind":"other"}"#).is_none());
    }

    #[test]
    fn dom_snapshot_assigns_sequential_refs() {
        let snap = DomSnapshot {
            title: "T".into(),
            url: "https://x.dev/".into(),
            text: String::new(),
            entries: vec![
                DomEntry { tag: "nav".into(), role: "nav".into(), name: String::new(), selector: "nav".into(), depth: 0 },
                DomEntry { tag: "a".into(), role: "link".into(), name: "Home".into(), selector: "#home".into(), depth: 1 },
            ],
        };
        let md = format_dom_snapshot(&snap);
        assert!(md.contains("@ref1 nav `nav`"));
        assert!(md.contains("@ref2 link `#home` — Home"));
        // depth-1 entry is indented under the depth-0 one.
        assert!(md.contains("\n  @ref2"));
    }

    #[test]
    fn console_formats_empty_and_full() {
        assert!(format_console(&[], &[]).contains("no console output"));
        let items = vec![LogEntry { level: "log".into(), text: "ready".into() }];
        let md = format_console(&items, &["bad".into()]);
        assert!(md.contains("[log] ready"));
        assert!(md.contains("[error] bad"));
    }

    #[test]
    fn fenced_block_outgrows_inner_backticks() {
        // A body containing a ``` run must be wrapped in a longer run so it
        // can't close the fence early.
        let md = fenced_block("html", "<pre>```</pre>");
        assert!(md.starts_with("````html\n"), "got: {md}");
        assert!(md.trim_end().ends_with("````"));
        // Plain body still uses the minimal 3-backtick fence.
        assert!(fenced_block("text", "hi").starts_with("```text\n"));
    }

    #[test]
    fn pick_formats_styles_and_html() {
        let mut styles = BTreeMap::new();
        styles.insert("color".to_string(), "rgb(0, 0, 0)".to_string());
        let p = PickPayload {
            selector: "#go".into(),
            css_path: "body > #go".into(),
            html: "<a id=\"go\">x</a>".into(),
            text: "x".into(),
            styles,
            rect: PickRect { x: 1.0, y: 2.0, w: 30.0, h: 40.0 },
        };
        let md = format_pick(&p);
        assert!(md.contains("selector: `#go`"));
        assert!(md.contains("path: `body > #go`"));
        assert!(md.contains("rect: 30×40 at (1, 2)"));
        assert!(md.contains("- color: rgb(0, 0, 0)"));
        assert!(md.contains("```html"));
    }
}
