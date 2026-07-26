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

/// Briefly flash the page white to signal a screenshot was captured. Injected
/// *after* the snapshot lands so it's never in the shot itself. Self-removing.
pub const SCREENSHOT_FLASH_JS: &str = r#"
(function () {
  var f = document.createElement('div');
  f.style.cssText = 'position:fixed;inset:0;z-index:2147483647;pointer-events:none;background:#fff;opacity:0.5;transition:opacity 0.2s ease-out;';
  document.documentElement.appendChild(f);
  requestAnimationFrame(function () { f.style.opacity = '0'; });
  setTimeout(function () { f.remove(); }, 240);
})();
"#;

/// A floating "✓ <label>" toast pinned to the page's top-right, used to confirm
/// toolbar-button copies. It lives in the page (its own shadow root) rather
/// than the toolbar: a host-drawn pill in the toolbar's flex row shoved every
/// icon left as it appeared, and a GPUI overlay would render *under* the native
/// webview. Slides down + fades in, then auto-dismisses; a fresh toast replaces
/// any in-flight one. `__OX_LABEL__` is substituted with a JSON-encoded string
/// by [`confirm_toast_js`], so the label can never break out of the literal.
const CONFIRM_TOAST_JS: &str = r#"
(function () {
  var id = '__oxToast';
  var prev = document.getElementById(id); if (prev) prev.remove();
  var host = document.createElement('div'); host.id = id;
  host.style.cssText = 'position:fixed;top:10px;right:12px;z-index:2147483647;pointer-events:none;';
  var sh = host.attachShadow({ mode: 'open' });
  var s = document.createElement('style');
  s.textContent =
    '.t{display:inline-flex;align-items:center;gap:7px;font:12px -apple-system,system-ui,sans-serif;'
    + 'font-weight:600;color:#1d1d1f;background:rgba(255,255,255,0.97);padding:7px 13px 7px 9px;'
    + 'border-radius:999px;box-shadow:0 8px 26px rgba(0,0,0,0.28);'
    + '-webkit-backdrop-filter:blur(20px);backdrop-filter:blur(20px);'
    + 'opacity:0;transform:translateY(-10px);transition:opacity .2s ease,transform .2s ease;}'
    + '.t.in{opacity:1;transform:translateY(0);}'
    + '.b{display:inline-flex;align-items:center;justify-content:center;width:16px;height:16px;'
    + 'border-radius:50%;background:#34c759;color:#fff;font-size:11px;line-height:1;flex:0 0 auto;}';
  sh.appendChild(s);
  var t = document.createElement('div'); t.className = 't';
  var b = document.createElement('span'); b.className = 'b'; b.textContent = '✓'; t.appendChild(b);
  var x = document.createElement('span'); x.textContent = __OX_LABEL__; t.appendChild(x);
  sh.appendChild(t);
  document.documentElement.appendChild(host);
  requestAnimationFrame(function () { t.classList.add('in'); });
  setTimeout(function () { if (t) t.classList.remove('in'); }, 1500);
  setTimeout(function () { host.remove(); }, 1740);
})();
"#;

/// Build the in-page confirmation toast for `label`, JSON-encoding the label so
/// page-safe text can't escape the JS string literal. See [`CONFIRM_TOAST_JS`].
pub fn confirm_toast_js(label: &str) -> String {
    let json = serde_json::to_string(label).unwrap_or_else(|_| "\"Copied\"".to_string());
    CONFIRM_TOAST_JS.replace("__OX_LABEL__", &json)
}

/// Page-theme dropdown menu, injected in-page (its own shadow root) when the
/// toolbar's appearance button is clicked — the off-macOS fallback, where the
/// native view has no AppKit `NSMenu` to pop over it. A dark translucent panel
/// at the top-right listing System / Light / Dark with a ✓ on the active one.
/// Selecting a row posts `set_appearance` back to the host; a full-page backdrop
/// (and `Esc`, when the page holds focus) dismisses it. `__OX_CURRENT__` is
/// substituted with the active slug by [`appearance_menu_js`].
#[cfg_attr(target_os = "macos", allow(dead_code))]
const APPEARANCE_MENU_JS: &str = r#"
(function () {
  if (window.__oxThemeMenu) { window.__oxThemeMenu.remove(); window.__oxThemeMenu = null; }
  var host = document.createElement('div');
  host.style.cssText = 'position:fixed;inset:0;z-index:2147483647;pointer-events:none;';
  var rs = host.attachShadow({ mode: 'open' });
  var st = document.createElement('style');
  st.textContent =
    '.backdrop{position:fixed;inset:0;pointer-events:auto;}'
    + '.menu{position:fixed;top:8px;right:10px;min-width:172px;pointer-events:auto;'
    + 'background:rgba(38,38,42,0.98);color:#fff;border:0.5px solid rgba(255,255,255,0.16);'
    + 'border-radius:10px;padding:5px;font:13px -apple-system,system-ui,sans-serif;'
    + 'box-shadow:0 14px 38px rgba(0,0,0,0.5);-webkit-backdrop-filter:blur(20px);backdrop-filter:blur(20px);'
    + 'transform-origin:top right;animation:oxpop .13s cubic-bezier(.2,.8,.2,1);}'
    + '@keyframes oxpop{from{opacity:0;transform:scale(.95) translateY(-6px);}to{opacity:1;transform:none;}}'
    + '.head{padding:4px 9px 7px;font:11px ui-monospace,monospace;color:rgba(255,255,255,0.5);'
    + 'border-bottom:0.5px solid rgba(255,255,255,0.1);margin-bottom:4px;}'
    + '.item{display:flex;align-items:center;gap:8px;padding:6px 9px;border-radius:6px;cursor:pointer;'
    + 'white-space:nowrap;}'
    + '.item:hover{background:#3478f6;color:#fff;}'
    + '.tick{width:13px;text-align:center;color:#34c759;font-size:12px;flex:0 0 auto;}'
    + '.item:hover .tick{color:#fff;}'
    + '.lbl{flex:1;}';
  rs.appendChild(st);
  var bd = document.createElement('div'); bd.className = 'backdrop';
  bd.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); cleanup(); }, true);
  rs.appendChild(bd);
  var menu = document.createElement('div'); menu.className = 'menu';
  var head = document.createElement('div'); head.className = 'head'; head.textContent = 'Page theme'; menu.appendChild(head);
  var CUR = __OX_CURRENT__;
  [['system', 'System'], ['light', 'Light'], ['dark', 'Dark']].forEach(function (it) {
    var row = document.createElement('div'); row.className = 'item';
    var tk = document.createElement('span'); tk.className = 'tick'; tk.textContent = (it[0] === CUR ? '✓' : ''); row.appendChild(tk);
    var lb = document.createElement('span'); lb.className = 'lbl'; lb.textContent = it[1]; row.appendChild(lb);
    row.addEventListener('click', function (e) {
      e.preventDefault(); e.stopPropagation();
      window.ipc.postMessage(JSON.stringify({ kind: 'set_appearance', value: it[0] }));
      cleanup();
    }, true);
    menu.appendChild(row);
  });
  rs.appendChild(menu);
  document.documentElement.appendChild(host);
  function cleanup() { window.removeEventListener('keydown', esc, true); host.remove(); window.__oxThemeMenu = null; }
  function esc(e) { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); cleanup(); } }
  window.addEventListener('keydown', esc, true);
  window.__oxThemeMenu = host;
})();
"#;

/// Build the page-theme menu JS with the active slug (`system`/`light`/`dark`)
/// marked. See [`APPEARANCE_MENU_JS`]. Off-macOS fallback (macOS uses `NSMenu`).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn appearance_menu_js(current: &str) -> String {
    let cur = serde_json::to_string(current).unwrap_or_else(|_| "\"system\"".to_string());
    APPEARANCE_MENU_JS.replace("__OX_CURRENT__", &cur)
}

/// Profiles dropdown, injected in-page when the toolbar's profile button is
/// clicked — the off-macOS fallback (same rationale as the theme menu). Lists
/// every cookie-isolated profile with a ✓ + a highlighted row on the active
/// one, then a "New Profile…" action below a divider. Selecting a profile posts
/// `switch_profile`; the action posts `new_profile`. `__OX_PROFILES__` is
/// substituted by [`profile_menu_js`] with a JSON array of `{id, name, active}`
/// (the host owns the list, so the page can't fabricate a profile).
#[cfg_attr(target_os = "macos", allow(dead_code))]
const PROFILE_MENU_JS: &str = r#"
(function () {
  if (window.__oxProfileMenu) { window.__oxProfileMenu.remove(); window.__oxProfileMenu = null; }
  var ITEMS = __OX_PROFILES__;
  var host = document.createElement('div');
  host.style.cssText = 'position:fixed;inset:0;z-index:2147483647;pointer-events:none;';
  var rs = host.attachShadow({ mode: 'open' });
  var st = document.createElement('style');
  st.textContent =
    '.backdrop{position:fixed;inset:0;pointer-events:auto;}'
    + '.menu{position:fixed;top:8px;right:10px;min-width:204px;max-width:320px;pointer-events:auto;'
    + 'background:rgba(38,38,42,0.98);color:#fff;border:0.5px solid rgba(255,255,255,0.16);'
    + 'border-radius:10px;padding:5px;font:13px -apple-system,system-ui,sans-serif;'
    + 'box-shadow:0 14px 38px rgba(0,0,0,0.5);-webkit-backdrop-filter:blur(20px);backdrop-filter:blur(20px);'
    + 'transform-origin:top right;animation:oxpop .13s cubic-bezier(.2,.8,.2,1);}'
    + '@keyframes oxpop{from{opacity:0;transform:scale(.95) translateY(-6px);}to{opacity:1;transform:none;}}'
    + '.head{padding:4px 9px 7px;font:11px ui-monospace,monospace;color:rgba(255,255,255,0.5);'
    + 'border-bottom:0.5px solid rgba(255,255,255,0.1);margin-bottom:4px;}'
    + '.item{display:flex;align-items:center;gap:8px;padding:6px 9px;border-radius:6px;cursor:pointer;white-space:nowrap;}'
    + '.item.active{background:rgba(255,255,255,0.08);}'
    + '.item:hover{background:#3478f6;color:#fff;}'
    + '.tick{width:13px;text-align:center;color:#34c759;font-size:12px;flex:0 0 auto;}'
    + '.item:hover .tick{color:#fff;}'
    + '.lbl{flex:1;overflow:hidden;text-overflow:ellipsis;}'
    + '.sep{height:0.5px;margin:5px 6px;background:rgba(255,255,255,0.12);}';
  rs.appendChild(st);
  var bd = document.createElement('div'); bd.className = 'backdrop';
  bd.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); cleanup(); }, true);
  rs.appendChild(bd);
  var menu = document.createElement('div'); menu.className = 'menu';
  var head = document.createElement('div'); head.className = 'head'; head.textContent = 'Profiles'; menu.appendChild(head);
  function post(m) { window.ipc.postMessage(JSON.stringify(m)); cleanup(); }
  ITEMS.forEach(function (it) {
    var row = document.createElement('div'); row.className = 'item' + (it.active ? ' active' : '');
    var tk = document.createElement('span'); tk.className = 'tick'; tk.textContent = (it.active ? '✓' : ''); row.appendChild(tk);
    var lb = document.createElement('span'); lb.className = 'lbl'; lb.textContent = it.name; row.appendChild(lb);
    row.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); post({ kind: 'switch_profile', id: it.id }); }, true);
    menu.appendChild(row);
  });
  var sep = document.createElement('div'); sep.className = 'sep'; menu.appendChild(sep);
  var nw = document.createElement('div'); nw.className = 'item';
  nw.appendChild(document.createElement('span')).className = 'tick';
  var nwlb = document.createElement('span'); nwlb.className = 'lbl'; nwlb.textContent = 'New Profile…'; nw.appendChild(nwlb);
  nw.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); post({ kind: 'new_profile' }); }, true);
  menu.appendChild(nw);
  rs.appendChild(menu);
  document.documentElement.appendChild(host);
  function cleanup() { window.removeEventListener('keydown', esc, true); host.remove(); window.__oxProfileMenu = null; }
  function esc(e) { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); cleanup(); } }
  window.addEventListener('keydown', esc, true);
  window.__oxProfileMenu = host;
})();
"#;

/// Build the profiles menu JS from a JSON array of `{id, name, active}` rows.
/// See [`PROFILE_MENU_JS`]. Off-macOS fallback (macOS uses `NSMenu`).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn profile_menu_js(items_json: &str) -> String {
    PROFILE_MENU_JS.replace("__OX_PROFILES__", items_json)
}

/// Inject the element-picker shadow-overlay. Hover highlights
/// `elementFromPoint`; a **click copies the element's context as text by
/// default** (tag, role, accessible name, selector, dimensions, text, nearby
/// context, computed styles, HTML, ancestor + full DOM path — the most
/// generally useful grab) and drops a white "✓ Copied" chip with a **⋯ More**
/// button. The ⋯ opens a small popover with the other facets — copy the
/// element, HTML, styles, or text, copy a screenshot of its rect, or send it
/// to the agent. The chip + popover live in their own shadow root (a
/// host-drawn popover would render *under* the native webview). Bare `C` / `A`
/// / `S` keyboard accelerators copy element / send to agent / screenshot
/// directly while hovering; `Esc` cancels. Listeners are capture-phase +
/// `stopPropagation` so the page's own handlers never see the picker's clicks
/// or keys.
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
  var hint = document.createElement('div');
  hint.textContent = 'Click to copy element · ⋯ for more · S screenshot · A → agent · Esc';
  hint.style.cssText = 'position:fixed;left:50%;bottom:16px;transform:translateX(-50%);pointer-events:none;font:11px system-ui,sans-serif;background:rgba(17,17,17,0.92);color:#fff;padding:4px 10px;border-radius:999px;white-space:nowrap;';
  shadow.appendChild(box);
  shadow.appendChild(label);
  shadow.appendChild(hint);
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
  var ROLES = { A: 'link', BUTTON: 'button', INPUT: 'input', SELECT: 'select', TEXTAREA: 'textarea',
                H1: 'heading', H2: 'heading', H3: 'heading', H4: 'heading',
                NAV: 'navigation', MAIN: 'main', FORM: 'form', LABEL: 'label', IMG: 'img' };
  function roleOf(el) { return el.getAttribute('role') || ROLES[el.tagName] || ''; }
  function clean(s) { return (s || '').replace(/\s+/g, ' ').trim(); }
  // tag#id or tag.class.class — always tag-qualified (the hover `sel` drops the
  // tag for an id; payload selectors and DOM paths want the full form).
  function fullSel(el) {
    var s = el.tagName.toLowerCase();
    if (el.id) return s + '#' + CSS.escape(el.id);
    if (el.classList && el.classList.length) {
      s += '.' + Array.prototype.slice.call(el.classList, 0, 3).map(function (c) { return CSS.escape(c); }).join('.');
    }
    return s;
  }
  // ARIA accessible name: aria-label(ledby), then text for button/link/label,
  // then title / alt / placeholder. Bounded so a huge subtree can't blow it up.
  function accName(el) {
    var n = clean(el.getAttribute('aria-label'));
    if (!n) {
      var lb = el.getAttribute('aria-labelledby');
      if (lb) { var ps = []; lb.split(/\s+/).forEach(function (id) { var t = document.getElementById(id); if (t) ps.push(clean(t.textContent)); }); n = clean(ps.join(' ')); }
    }
    var tag = el.tagName.toLowerCase();
    if (!n && (tag === 'button' || tag === 'a' || tag === 'label')) n = clean(el.textContent).slice(0, 100);
    if (!n) n = clean(el.getAttribute('title') || el.getAttribute('alt') || el.getAttribute('placeholder') || '');
    return n.slice(0, 120);
  }
  // A few short strings that contextualize the element: its label(s),
  // described-by text, placeholder, and immediate siblings (own text excluded).
  // Capped at 5 entries × 140 chars.
  function nearby(el) {
    var out = [], seen = {};
    function add(s) { s = clean(s); if (s && s.length <= 140 && !seen[s] && out.length < 5) { seen[s] = 1; out.push(s); } }
    if (el.id) { try { document.querySelectorAll('label[for="' + CSS.escape(el.id) + '"]').forEach(function (l) { add(l.textContent); }); } catch (e) {} }
    var lab = el.closest ? el.closest('label') : null; if (lab) add(lab.textContent);
    var db = el.getAttribute('aria-describedby');
    if (db) db.split(/\s+/).forEach(function (id) { var t = document.getElementById(id); if (t) add(t.textContent); });
    add(el.getAttribute('placeholder'));
    if (el.previousElementSibling) add(el.previousElementSibling.textContent);
    if (el.nextElementSibling) add(el.nextElementSibling.textContent);
    var own = clean(el.textContent);
    return out.filter(function (s) { return s !== own; });
  }
  // Ancestor chain (element excluded), each `tag` or `tag[role=…]`, root-first.
  function ancestorPath(el) {
    var parts = [], n = el.parentElement, hops = 0;
    while (n && n.nodeType === 1 && hops < 12) {
      var r = n.getAttribute('role');
      parts.unshift(r ? n.tagName.toLowerCase() + '[role=' + r + ']' : n.tagName.toLowerCase());
      n = n.parentElement; hops++;
    }
    return parts.join(' > ');
  }
  // Full selector path from the nearest body/html down to the element.
  function fullPath(el) {
    var parts = [], n = el, hops = 0;
    while (n && n.nodeType === 1 && hops < 16) {
      parts.unshift(fullSel(n));
      if (n.tagName === 'BODY' || n.tagName === 'HTML') break;
      n = n.parentElement; hops++;
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
    return { kind: 'pick', url: location.href, tag: el.tagName.toLowerCase(),
             role: roleOf(el), name: accName(el), selector: fullSel(el),
             text: clean(el.textContent).slice(0, 500), nearby: nearby(el),
             styles: styles, html: (el.outerHTML || '').slice(0, 4096),
             ancestor_path: ancestorPath(el), full_path: fullPath(el),
             rect: { x: r.left, y: r.top, w: r.width, h: r.height } };
  }
  function copyImage(rect) {
    window.ipc.postMessage(JSON.stringify({ kind: 'pick_shot', rect: rect }));
  }
  function emitPart(p, part) { var m = Object.assign({}, p); m.kind = 'pick'; m.part = part; window.ipc.postMessage(JSON.stringify(m)); }
  function emitAgent(p) { var m = Object.assign({}, p); m.kind = 'pick_to_agent'; window.ipc.postMessage(JSON.stringify(m)); }

  // After the default image copy, show a white "✓ Copied" chip anchored above
  // the element with a "⋯ More" button. The chip + its popover get their own
  // shadow root so they survive the picking overlay's teardown and stay clear
  // of page CSS. `p`/`rect` snapshot the clicked element for the deferred
  // options.
  function showResult(rect, p) {
    var rHost = document.createElement('div');
    rHost.style.cssText = 'position:fixed;inset:0;z-index:2147483647;pointer-events:none;';
    var rs = rHost.attachShadow({ mode: 'open' });
    var st = document.createElement('style');
    st.textContent =
      '.pill{position:fixed;display:inline-flex;align-items:center;gap:6px;pointer-events:auto;'
      + 'font:12px -apple-system,system-ui,sans-serif;font-weight:600;color:#1d1d1f;background:#fff;'
      + 'padding:4px 5px 4px 7px;border-radius:999px;box-shadow:0 6px 22px rgba(0,0,0,0.4);white-space:nowrap;}'
      + '.badge{display:inline-flex;align-items:center;justify-content:center;width:16px;height:16px;'
      + 'border-radius:50%;background:#34c759;color:#fff;font-size:11px;line-height:1;}'
      + '.more{display:inline-flex;align-items:center;justify-content:center;min-width:24px;height:20px;'
      + 'margin-left:1px;border-radius:999px;background:rgba(0,0,0,0.06);color:#3478f6;font-weight:700;'
      + 'cursor:pointer;letter-spacing:1.5px;}'
      + '.more:hover{background:rgba(52,120,246,0.16);}'
      + '.backdrop{position:fixed;inset:0;pointer-events:auto;}'
      + '.menu{position:fixed;min-width:188px;pointer-events:auto;background:rgba(38,38,42,0.98);color:#fff;'
      + 'border:0.5px solid rgba(255,255,255,0.16);border-radius:10px;padding:5px;'
      + 'font:13px -apple-system,system-ui,sans-serif;box-shadow:0 12px 34px rgba(0,0,0,0.5);'
      + '-webkit-backdrop-filter:blur(20px);backdrop-filter:blur(20px);}'
      + '.head{padding:4px 9px 7px;font:11px ui-monospace,monospace;color:rgba(255,255,255,0.5);'
      + 'border-bottom:0.5px solid rgba(255,255,255,0.1);margin-bottom:4px;white-space:nowrap;'
      + 'overflow:hidden;text-overflow:ellipsis;max-width:260px;}'
      + '.item{display:flex;justify-content:space-between;gap:24px;align-items:center;'
      + 'padding:5px 9px;border-radius:6px;cursor:pointer;white-space:nowrap;}'
      + '.item:hover{background:#3478f6;color:#fff;}'
      + '.sep{height:0.5px;margin:4px 6px;background:rgba(255,255,255,0.12);}'
      + '.key{opacity:0.4;font:11px system-ui,sans-serif;}';
    rs.appendChild(st);

    var pill = document.createElement('div'); pill.className = 'pill';
    var badge = document.createElement('span'); badge.className = 'badge'; badge.textContent = '✓'; pill.appendChild(badge);
    var txt = document.createElement('span'); txt.textContent = 'Copied'; pill.appendChild(txt);
    var more = document.createElement('span'); more.className = 'more'; more.textContent = '···'; more.title = 'More copy options'; pill.appendChild(more);
    rs.appendChild(pill);
    document.documentElement.appendChild(rHost);

    var pw = pill.offsetWidth;
    var pleft = Math.min(Math.max(8, rect.x + rect.w / 2 - pw / 2), window.innerWidth - pw - 8);
    var ptop = Math.max(6, rect.y - 34);
    pill.style.left = pleft + 'px'; pill.style.top = ptop + 'px';

    var killT = setTimeout(cleanup, 4200);
    function cleanup() { clearTimeout(killT); window.removeEventListener('keydown', esc, true); rHost.remove(); }
    function esc(e) { if (e.key === 'Escape') { cleanup(); e.preventDefault(); e.stopPropagation(); } }
    window.addEventListener('keydown', esc, true);

    var ITEMS = [
      { t: 'Copy element', k: 'C', f: function () { emitPart(p, 'all'); } },
      { t: 'Copy HTML', k: '', f: function () { emitPart(p, 'html'); } },
      { t: 'Copy styles', k: '', f: function () { emitPart(p, 'styles'); } },
      { t: 'Copy text', k: '', f: function () { emitPart(p, 'text'); } },
      { sep: true },
      { t: 'Copy screenshot', k: 'S', f: function () { copyImage(rect); } },
      { t: 'Send to agent', k: 'A', f: function () { emitAgent(p); } }
    ];
    var expanded = false;
    more.addEventListener('click', function (e) {
      e.preventDefault(); e.stopPropagation(); clearTimeout(killT);
      if (!expanded) openMore();
    }, true);
    function openMore() {
      expanded = true;
      more.style.background = 'rgba(52,120,246,0.16)'; // keep the ⋯ lit while open
      // Backdrop catches outside clicks to dismiss; the chip itself stays put.
      var bd = document.createElement('div'); bd.className = 'backdrop';
      bd.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); cleanup(); }, true);
      rs.appendChild(bd);
      var menu = document.createElement('div'); menu.className = 'menu';
      var head = document.createElement('div'); head.className = 'head'; head.textContent = p.selector || ''; menu.appendChild(head);
      ITEMS.forEach(function (it) {
        if (it.sep) { var s = document.createElement('div'); s.className = 'sep'; menu.appendChild(s); return; }
        var row = document.createElement('div'); row.className = 'item';
        var a = document.createElement('span'); a.textContent = it.t; row.appendChild(a);
        if (it.k) { var k = document.createElement('span'); k.className = 'key'; k.textContent = it.k; row.appendChild(k); }
        row.addEventListener('click', function (e) { e.preventDefault(); e.stopPropagation(); it.f(); cleanup(); }, true);
        menu.appendChild(row);
      });
      rs.appendChild(menu);
      // Expand directly under the chip (which stays visible), aligned to its
      // left edge and clamped on-screen.
      var mw = menu.offsetWidth, mh = menu.offsetHeight, ph = pill.offsetHeight;
      var ml = Math.min(Math.max(6, pleft), window.innerWidth - mw - 6);
      var mt = ptop + ph + 6;
      if (mt + mh > window.innerHeight - 6) mt = Math.max(6, ptop - mh - 6);
      menu.style.left = ml + 'px'; menu.style.top = mt + 'px';
    }
  }

  // Block the page from seeing the picker's mousedown (no drag-start, focus
  // steal, or mousedown-driven control); the click still fires for us.
  function down(e) { e.preventDefault(); e.stopPropagation(); }
  function click(e) {
    if (!cur) return;
    var p = payload(cur);
    emitPart(p, 'all');    // default action: copy the element's context as text
    stop();                // tear down the picking overlay
    showResult(p.rect, p); // chip + ⋯ for the other facets (incl. screenshot)
    e.preventDefault(); e.stopPropagation();
  }
  function key(e) {
    if (e.key === 'Escape') {
      window.ipc.postMessage(JSON.stringify({ kind: 'pick_cancel' }));
      stop(); e.preventDefault(); e.stopPropagation(); return;
    }
    // Leave OS chords (Cmd/Ctrl+C, Cmd/Ctrl+S, …) to the page / browser — the
    // picker only claims the bare C / A / S keys.
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (!cur) return;
    var p = payload(cur);
    if (e.key === 'c' || e.key === 'C') { emitPart(p, 'all'); stop(); showResult(p.rect, p); e.preventDefault(); e.stopPropagation(); }
    else if (e.key === 'a' || e.key === 'A') { emitAgent(p); stop(); showResult(p.rect, p); e.preventDefault(); e.stopPropagation(); }
    else if (e.key === 's' || e.key === 'S') { copyImage(p.rect); stop(); showResult(p.rect, p); e.preventDefault(); e.stopPropagation(); }
  }
  function stop() {
    host.remove();
    window.removeEventListener('mousemove', move, true);
    window.removeEventListener('mousedown', down, true);
    window.removeEventListener('click', click, true);
    window.removeEventListener('keydown', key, true);
    window.__oxPicker = null;
  }
  window.addEventListener('mousemove', move, true);
  window.addEventListener('mousedown', down, true);
  window.addEventListener('click', click, true);
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

/// Which facet of a picked element the copy-options menu requested. Defaults
/// to the full agent-oriented markdown when absent (keyboard `C`, older IPC).
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PickPart {
    /// Full browser-context block (identity + styles + HTML + DOM paths) —
    /// agent context.
    #[default]
    All,
    /// Raw `outerHTML` only.
    Html,
    /// The probed computed styles as a CSS declaration block.
    Styles,
    /// The element's text content only.
    Text,
}

#[derive(Debug, Deserialize)]
pub struct PickPayload {
    /// Page URL the element was grabbed from (query/fragment stripped on format).
    #[serde(default)]
    pub url: String,
    /// Lowercase tag name (`textarea`, `a`, `img`, …).
    #[serde(default)]
    pub tag: String,
    /// ARIA role — explicit `role` attribute or the implicit role for the tag.
    #[serde(default)]
    pub role: String,
    /// ARIA accessible name (aria-label / labelled text / title / alt / …).
    #[serde(default)]
    pub name: String,
    /// Tag-qualified selector for the element itself (`textarea#APjFqb`).
    #[serde(default)]
    pub selector: String,
    #[serde(default)]
    pub text: String,
    /// Short strings that contextualize the element (labels, sibling text).
    #[serde(default)]
    pub nearby: Vec<String>,
    #[serde(default)]
    pub styles: BTreeMap<String, String>,
    #[serde(default)]
    pub html: String,
    /// Ancestor chain (element excluded), each `tag` or `tag[role=…]`.
    #[serde(default)]
    pub ancestor_path: String,
    /// Full selector path from the nearest body/html down to the element.
    #[serde(default)]
    pub full_path: String,
    /// Facet to copy (menu choice); `All` for the keyboard `C` accelerator.
    #[serde(default)]
    pub part: PickPart,
    pub rect: PickRect,
}

/// Page color-scheme chosen from the in-page theme menu. Maps to the host's
/// `PageAppearance` (see `super::mod`); kept here so the IPC layer can
/// deserialize it directly.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceValue {
    System,
    Light,
    Dark,
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
    /// Picked element routed to the active agent terminal instead of the
    /// clipboard (`A` in the picker).
    PickToAgent(PickPayload),
    PickShot {
        rect: PickRect,
    },
    /// Picker dismissed with `Esc` — no payload, just a signal to hand the
    /// keyboard back to the host.
    PickCancel,
    /// Page color-scheme picked from the in-page theme menu.
    SetAppearance {
        value: AppearanceValue,
    },
    /// A profile chosen from the in-page profiles menu. `id` is a profile UUID
    /// string, or `"default"` for the shared store.
    SwitchProfile {
        id: String,
    },
    /// "New Profile…" chosen from the profiles menu.
    NewProfile,
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

/// Strip the query string and fragment from a URL so the grabbed-from header
/// can't leak search terms / tokens that ride in those parts. Best-effort
/// string split (the page-supplied URL is treated as inert text either way).
fn sanitize_url(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).trim().to_string()
}

/// Format a picked element as a pasteable text block: where it came from, its
/// identity (tag, role, accessible name, selector, dimensions), text content,
/// nearby context, computed styles, a clamped HTML excerpt, and its ancestor +
/// full DOM paths. Plain text (not markdown) so it pastes cleanly into a search
/// box, an editor, or an agent prompt alike.
pub fn format_pick(p: &PickPayload) -> String {
    let mut out = String::new();
    if !p.url.is_empty() {
        out.push_str(&format!("Attached browser context from {}\n\n", sanitize_url(&p.url)));
    }
    out.push_str("Selected element:\n");
    out.push_str(if p.tag.is_empty() { "element" } else { &p.tag });
    out.push('\n');
    if !p.name.is_empty() {
        out.push_str(&format!("Accessible name: \"{}\"\n", one_line(&p.name)));
    }
    if !p.role.is_empty() {
        out.push_str(&format!("Role: {}\n", one_line(&p.role)));
    }
    if !p.selector.is_empty() {
        out.push_str(&format!("Selector: {}\n", one_line(&p.selector)));
    }
    out.push_str(&format!("Dimensions: {:.0}x{:.0}\n", p.rect.w, p.rect.h));
    if !p.text.is_empty() {
        out.push_str(&format!("\nText content:\n{}\n", one_line(&p.text)));
    }
    if !p.nearby.is_empty() {
        out.push_str("\nNearby context:\n");
        for n in &p.nearby {
            out.push_str(&format!("- {}\n", one_line(n)));
        }
    }
    let styles: Vec<_> = p.styles.iter().filter(|(_, v)| !v.trim().is_empty()).collect();
    if !styles.is_empty() {
        out.push_str("\nComputed styles:\n");
        for (k, v) in styles {
            out.push_str(&format!("  {}: {}\n", one_line(k), one_line(v)));
        }
    }
    if !p.html.is_empty() {
        out.push_str("\nHTML:\n");
        out.push_str(p.html.trim());
        out.push('\n');
    }
    if !p.ancestor_path.is_empty() {
        out.push_str(&format!("\nAncestor path: {}\n", one_line(&p.ancestor_path)));
    }
    if !p.full_path.is_empty() {
        out.push_str(&format!("Full DOM path: {}\n", one_line(&p.full_path)));
    }
    out
}

/// Format a picked element for the clipboard per the menu's chosen facet. The
/// full `All` facet reuses [`format_pick`] (agent-oriented markdown); the
/// narrower facets copy the bare value so it pastes directly where it's wanted
/// (raw HTML, a CSS declaration block, or plain text).
pub fn format_pick_part(p: &PickPayload) -> String {
    match p.part {
        PickPart::All => format_pick(p),
        PickPart::Html => p.html.clone(),
        PickPart::Styles => p
            .styles
            .iter()
            .map(|(k, v)| format!("{}: {};", k.trim(), v.trim()))
            .collect::<Vec<_>>()
            .join("\n"),
        PickPart::Text => p.text.clone(),
    }
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
    fn parses_pick_to_agent() {
        // The picker's `A` key tags the same payload `pick_to_agent` so it
        // routes to the agent terminal instead of the clipboard.
        let body = r##"{"kind":"pick_to_agent","selector":"#go","rect":{"x":1,"y":2,"w":3,"h":4}}"##;
        match IpcMessage::parse(body).expect("parse") {
            IpcMessage::PickToAgent(p) => assert_eq!(p.selector, "#go"),
            _ => panic!("wrong variant"),
        }
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

    fn sample_pick(part: PickPart) -> PickPayload {
        let mut styles = BTreeMap::new();
        styles.insert("color".to_string(), "rgb(0, 0, 0)".to_string());
        styles.insert("display".to_string(), "block".to_string());
        PickPayload {
            url: "https://x.dev/search?q=secret#frag".into(),
            tag: "a".into(),
            role: "link".into(),
            name: "Go home".into(),
            selector: "a#go".into(),
            text: "x".into(),
            nearby: vec!["primary navigation".into()],
            styles,
            html: "<a id=\"go\">x</a>".into(),
            ancestor_path: "nav > body".into(),
            full_path: "body > nav > a#go".into(),
            part,
            rect: PickRect { x: 1.0, y: 2.0, w: 30.0, h: 40.0 },
        }
    }

    #[test]
    fn pick_formats_identity_styles_and_paths() {
        let md = format_pick(&sample_pick(PickPart::All));
        // Header strips the query/fragment so search terms / tokens don't leak.
        assert!(md.contains("Attached browser context from https://x.dev/search"));
        assert!(!md.contains("secret"));
        assert!(md.contains("Selected element:\na\n"));
        assert!(md.contains("Accessible name: \"Go home\""));
        assert!(md.contains("Role: link"));
        assert!(md.contains("Selector: a#go"));
        assert!(md.contains("Dimensions: 30x40"));
        assert!(md.contains("Text content:\nx"));
        assert!(md.contains("Nearby context:\n- primary navigation"));
        assert!(md.contains("  color: rgb(0, 0, 0)"));
        assert!(md.contains("HTML:\n<a id=\"go\">x</a>"));
        assert!(md.contains("Ancestor path: nav > body"));
        assert!(md.contains("Full DOM path: body > nav > a#go"));
    }

    #[test]
    fn pick_part_copies_bare_facets() {
        // `All` is the full browser-context text; the narrower facets copy the
        // bare value so it pastes directly where the user wants it.
        assert!(format_pick_part(&sample_pick(PickPart::All)).contains("Selected element:"));
        assert_eq!(format_pick_part(&sample_pick(PickPart::Html)), "<a id=\"go\">x</a>");
        assert_eq!(format_pick_part(&sample_pick(PickPart::Text)), "x");
        // Styles render as a CSS declaration block (BTreeMap → sorted keys).
        assert_eq!(
            format_pick_part(&sample_pick(PickPart::Styles)),
            "color: rgb(0, 0, 0);\ndisplay: block;"
        );
    }

    #[test]
    fn confirm_toast_embeds_json_encoded_label() {
        // The label is substituted as a JSON string so it can't break out of
        // the JS literal, and the placeholder is fully consumed.
        let js = confirm_toast_js("Screenshot copied");
        assert!(js.contains(r#"x.textContent = "Screenshot copied";"#));
        assert!(!js.contains("__OX_LABEL__"));
        // A label with a quote stays safely escaped inside the literal.
        let tricky = confirm_toast_js("a\"b");
        assert!(tricky.contains(r#""a\"b""#));
    }

    #[test]
    fn appearance_menu_marks_active_slug() {
        // The active slug is JSON-encoded into the menu so its row shows the ✓,
        // and the placeholder is fully consumed.
        let js = appearance_menu_js("dark");
        assert!(js.contains(r#"var CUR = "dark";"#));
        assert!(!js.contains("__OX_CURRENT__"));
    }

    #[test]
    fn parses_set_appearance() {
        // The theme menu posts the chosen slug; it deserializes to the value
        // the host maps onto its `PageAppearance`.
        let body = r#"{"kind":"set_appearance","value":"light"}"#;
        match IpcMessage::parse(body).expect("parse") {
            IpcMessage::SetAppearance { value } => assert_eq!(value, AppearanceValue::Light),
            _ => panic!("wrong variant"),
        }
        assert!(IpcMessage::parse(r#"{"kind":"set_appearance","value":"bogus"}"#).is_none());
    }

    #[test]
    fn profile_menu_embeds_items_and_parses_actions() {
        // The host-built profile list is spliced in verbatim (valid JSON → valid
        // JS array literal); the placeholder is fully consumed.
        let js = profile_menu_js(r#"[{"id":"default","name":"Default","active":true}]"#);
        assert!(js.contains(r#"var ITEMS = [{"id":"default","name":"Default","active":true}];"#));
        assert!(!js.contains("__OX_PROFILES__"));
        // Both menu actions round-trip through the IPC layer.
        match IpcMessage::parse(r#"{"kind":"switch_profile","id":"default"}"#).expect("parse") {
            IpcMessage::SwitchProfile { id } => assert_eq!(id, "default"),
            _ => panic!("wrong variant"),
        }
        assert!(matches!(
            IpcMessage::parse(r#"{"kind":"new_profile"}"#),
            Some(IpcMessage::NewProfile)
        ));
    }

    #[test]
    fn pick_part_defaults_to_all_when_absent() {
        // Older IPC / the keyboard `C` accelerator omit `part`.
        let body = r##"{"kind":"pick","selector":"#go","rect":{"x":1,"y":2,"w":3,"h":4}}"##;
        match IpcMessage::parse(body).expect("parse") {
            IpcMessage::Pick(p) => assert_eq!(p.part, PickPart::All),
            _ => panic!("wrong variant"),
        }
        // A menu choice carries the facet through.
        let html = r##"{"kind":"pick","part":"html","selector":"#go","rect":{"x":1,"y":2,"w":3,"h":4}}"##;
        match IpcMessage::parse(html).expect("parse") {
            IpcMessage::Pick(p) => assert_eq!(p.part, PickPart::Html),
            _ => panic!("wrong variant"),
        }
    }
}
