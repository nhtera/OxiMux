import { XTERM_CSS, XTERM_JS } from './xterm-bundle';

/** How the phone reconciles its viewport with the host's grid. */
export type FitMode = 'mirror' | 'reflow';

/** Smallest / largest font the page will pick or accept. */
export const MIN_FONT = 7;
export const MAX_FONT = 18;

/**
 * The HTML the terminal WebView runs: a real xterm.js emulator plus a small
 * bridge to React Native.
 *
 * Emulation happens here rather than in Rust or TypeScript because a terminal is
 * a genuinely hard thing to emulate — wide characters, scroll regions, alternate
 * buffers, mouse reporting — and the host already speaks the raw byte stream a
 * real emulator expects. The core forwards bytes; this page draws them.
 *
 * **Bytes cross as base64.** The WebView bridge carries strings only, and
 * terminal output is arbitrary binary (UTF-8 mid-sequence, C1 controls, raw
 * bytes from a program writing its own encoding). Sending it as a JS string
 * would put it through a lossy round trip; base64 survives intact and xterm
 * accepts the decoded `Uint8Array` directly.
 *
 * **The phone does not vote on the PTY's size.** The relay daemon runs each PTY
 * at the element-wise `min` of every attachment's requested size, and registers
 * a new attachment at the current effective size so that attaching alone is
 * neutral. A phone that fits the grid to its own viewport therefore shrinks the
 * desktop's terminal to phone width for as long as it is watching — the
 * attachment can only ever make the PTY smaller. So `mirror` (the default)
 * keeps the grid at exactly the host's `cols`/`rows` and scales the *font* to
 * fit instead, which also means absolute-position bytes land in the cells the
 * host meant. `reflow` is the opt-in that does resize the PTY, for when the
 * phone is the only thing watching.
 */
export function terminalPage(background: string, foreground: string, mode: FitMode): string {
  return `<!doctype html>
<html>
<head>
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<style>${XTERM_CSS}</style>
<style>
  html, body { margin: 0; padding: 0; height: 100%; background: ${background}; }
  /* In mirror mode the grid can be wider than the phone, so the page pans
     horizontally rather than clipping the right-hand columns. */
  body { overflow-x: auto; overflow-y: hidden; -webkit-overflow-scrolling: touch; }
  /* Width is set from JS to the grid's measured pixel width. It cannot be left
     to the content: xterm absolutely-positions its screen inside this element,
     so an intrinsic width collapses and the overflowing columns get clipped
     rather than becoming something the page can pan to. */
  #term { height: 100%; min-width: 100%; }
  /* The cursor-hosting textarea is what the soft keyboard attaches to; xterm
     positions it off-view, and iOS will not raise the keyboard for an element
     it considers hidden, so it is kept technically visible but transparent. */
  .xterm-helper-textarea { opacity: 0; }
</style>
</head>
<body>
<div id="term"></div>
<script>${XTERM_JS}</script>
<script>
  (function () {
    var MIN_FONT = ${MIN_FONT};
    var MAX_FONT = ${MAX_FONT};
    var mode = ${JSON.stringify(mode)};
    var fontSize = 12;
    // Auto-fit owns the font size until the user zooms by hand; a reset (a fresh
    // snapshot, and so possibly a new grid) hands control back to auto-fit.
    var autoFit = true;
    // The host's own grid, as of the last snapshot. Remembered because leaving
    // reflow has to hand this size back (see releaseVote) and by then the
    // emulator has already been resized down to the phone's.
    var hostCols = 0;
    var hostRows = 0;
    // Sticky Ctrl. It has to live here, not in React Native: the soft keyboard
    // types into xterm's own textarea, so nothing on the RN side ever sees the
    // keystroke it is supposed to modify.
    var ctrlPending = false;

    var post = function (msg) {
      if (window.ReactNativeWebView) window.ReactNativeWebView.postMessage(JSON.stringify(msg));
    };
    var term = new Terminal({
      convertEol: false,
      cursorBlink: true,
      fontSize: fontSize,
      fontFamily: 'Menlo, Courier, monospace',
      theme: { background: ${JSON.stringify(background)}, foreground: ${JSON.stringify(foreground)} },
      scrollback: 5000,
    });
    term.open(document.getElementById('term'));

    // Keystrokes out. xterm hands us exactly the bytes a real terminal would
    // send (arrow keys as CSI sequences, Ctrl-C as 0x03), so nothing here needs
    // to know what a key "means" — except when sticky Ctrl is armed, which is a
    // modifier the soft keyboard cannot express on its own.
    term.onData(function (data) {
      if (ctrlPending) {
        ctrlPending = false;
        post({ type: 'ctrl', on: false });
        if (data.length === 1) {
          var code = data.toUpperCase().charCodeAt(0);
          // @ through _ (and lowercase letters, upper-cased above) are the range
          // that has a control codepoint; anything else passes through unchanged
          // rather than being mangled into an unrelated byte.
          if (code >= 64 && code <= 95) data = String.fromCharCode(code & 0x1f);
        }
      }
      post({ type: 'input', data: data });
    });

    var b64ToBytes = function (b64) {
      var bin = atob(b64);
      var out = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
      return out;
    };

    // Measured from a rendered cell rather than assumed from the font size: the
    // actual advance width depends on the font the WebView resolved, and
    // guessing it puts every wrap in the wrong column.
    var cellSize = function () {
      var dims = term._core && term._core._renderService && term._core._renderService.dimensions;
      var cell = dims && dims.css && dims.css.cell;
      if (!cell || !cell.width || !cell.height) return null;
      return cell;
    };

    var host = document.getElementById('term');

    var report = function () {
      var cell = cellSize();
      var width = cell ? Math.ceil(term.cols * cell.width) : 0;
      // Give the element the grid's real width so the page has something to pan
      // over; the min-width in the stylesheet keeps a narrow grid full-bleed.
      if (width) host.style.width = width + 'px';
      post({
        type: 'geometry',
        cols: term.cols,
        rows: term.rows,
        fontSize: fontSize,
        // Whether the grid is wider than the phone, so the app can say so rather
        // than leaving the user to discover the missing columns by swiping.
        overflow: width > window.innerWidth + 1,
      });
    };

    var setFont = function (next) {
      next = Math.max(MIN_FONT, Math.min(MAX_FONT, next));
      if (next === fontSize) return false;
      fontSize = next;
      term.options.fontSize = next;
      return true;
    };

    // Mirror: keep the host's grid, scale the font so it fits the viewport.
    // Cell width is very nearly linear in font size, so one pass lands within a
    // pixel and the second (see refit) settles it.
    var fitFont = function () {
      var cell = cellSize();
      if (!cell) return;
      var affordable = window.innerWidth / term.cols;
      setFont(Math.floor(fontSize * (affordable / cell.width)));
    };

    // Reflow: size the grid to the phone and tell the app, which resizes the
    // real PTY — and, by the daemon's min rule, everyone else watching it.
    var fitGrid = function () {
      var cell = cellSize();
      if (!cell) return;
      var cols = Math.max(2, Math.floor(window.innerWidth / cell.width));
      var rows = Math.max(2, Math.floor(window.innerHeight / cell.height));
      if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
      post({ type: 'resize', cols: term.cols, rows: term.rows });
    };

    // Hand the host's size back after reflow shrank it.
    //
    // The daemon runs the PTY at the min across attachments and keeps each
    // attachment's LAST requested size, so simply going quiet leaves this phone
    // still voting for its own narrow grid — the desktop would stay shrunk for
    // the rest of the attachment's life, and detaching does not undo it either
    // (the vote is gone, but a min over the remaining attachments only recovers
    // if they were never dragged down). Re-asserting the host's own dimensions
    // is what actually releases it. Mirror therefore posts a resize exactly
    // here: never one that shrinks, only one that restores.
    var releaseVote = function () {
      if (!hostCols || !hostRows) return;
      if (term.cols === hostCols && term.rows === hostRows) return;
      term.resize(hostCols, hostRows);
      post({ type: 'resize', cols: hostCols, rows: hostRows });
    };

    var refit = function () {
      if (mode === 'reflow') fitGrid();
      else if (autoFit) fitFont();
      report();
    };

    // Two passes: changing the font changes the cell metrics the next
    // measurement needs, and a single pass leaves it a pixel off.
    var refitSoon = function () {
      requestAnimationFrame(function () {
        refit();
        requestAnimationFrame(refit);
      });
    };

    window.addEventListener('resize', refitSoon);

    // Messages in: replay/output bytes, a reset before a fresh snapshot, or one
    // of the controls the app's key bar drives.
    var apply = function (msg) {
      if (msg.type === 'write') {
        term.write(b64ToBytes(msg.data));
      } else if (msg.type === 'reset') {
        // A resync replaces the screen wholesale. Clearing first matters:
        // writing a replay snapshot on top of the old contents would leave the
        // pre-gap screen scrolled above it, which reads as real history.
        term.reset();
        // Build the emulator at exactly the host's dimensions before replaying,
        // so absolute-position bytes land in the cells the host addressed. In
        // reflow mode the following refit resizes it back down and the live
        // process repaints; in mirror mode this size is the one we keep.
        if (msg.cols && msg.rows) {
          hostCols = msg.cols;
          hostRows = msg.rows;
          term.resize(msg.cols, msg.rows);
        }
        autoFit = true;
        refitSoon();
      } else if (msg.type === 'focus') {
        term.focus();
      } else if (msg.type === 'mode') {
        if (mode === msg.mode) return;
        mode = msg.mode;
        autoFit = true;
        if (mode === 'mirror') releaseVote();
        refitSoon();
      } else if (msg.type === 'zoom') {
        // A manual zoom takes the font away from auto-fit until the next reset,
        // otherwise the following refit would immediately undo it.
        if (setFont(fontSize + msg.delta)) autoFit = false;
        refitSoon();
      } else if (msg.type === 'ctrl') {
        ctrlPending = !!msg.on;
        term.focus();
      } else if (msg.type === 'scroll') {
        term.scrollLines(msg.lines || 0);
      }
    };
    // Both targets are registered because the platforms disagree about which one
    // react-native-webview delivers to — but when a platform fires BOTH, every
    // message is handled twice, and a terminal handles its messages by writing
    // bytes to the screen. That is a flat 2x duplication of everything the host
    // sends, which reads exactly like a typing bug (each character echoing back
    // doubled) and is invisible to any test that drives one listener.
    //
    // The sequence number is the fix rather than picking a single target: it
    // costs nothing, needs no per-platform knowledge, and is robust if a future
    // version changes which target it uses.
    var lastSeq = -1;
    var receive = function (raw) {
      var msg;
      try {
        msg = JSON.parse(raw);
      } catch (e) {
        return;
      }
      if (typeof msg.seq === 'number') {
        if (msg.seq <= lastSeq) return;
        lastSeq = msg.seq;
      }
      apply(msg);
    };
    document.addEventListener('message', function (e) { receive(e.data); });
    window.addEventListener('message', function (e) { receive(e.data); });

    // Report the first fit once layout settles, so the app knows the geometry
    // before any snapshot arrives.
    refitSoon();
    post({ type: 'ready' });
  })();
</script>
</body>
</html>`;
}
