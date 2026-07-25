import { XTERM_CSS, XTERM_JS } from './xterm-bundle';

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
 */
export function terminalPage(background: string, foreground: string): string {
  return `<!doctype html>
<html>
<head>
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<style>${XTERM_CSS}</style>
<style>
  html, body { margin: 0; padding: 0; height: 100%; background: ${background}; }
  #term { height: 100%; width: 100%; }
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
    var post = function (msg) {
      if (window.ReactNativeWebView) window.ReactNativeWebView.postMessage(JSON.stringify(msg));
    };
    var term = new Terminal({
      convertEol: false,
      cursorBlink: true,
      fontSize: 12,
      fontFamily: 'Menlo, Courier, monospace',
      theme: { background: ${JSON.stringify(background)}, foreground: ${JSON.stringify(foreground)} },
      scrollback: 5000,
    });
    term.open(document.getElementById('term'));

    // Keystrokes out. xterm hands us exactly the bytes a real terminal would
    // send (arrow keys as CSI sequences, Ctrl-C as 0x03), so nothing here needs
    // to know what a key "means".
    term.onData(function (data) { post({ type: 'input', data: data }); });

    var b64ToBytes = function (b64) {
      var bin = atob(b64);
      var out = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
      return out;
    };

    // Fit the grid to the viewport and tell the app, which resizes the real PTY.
    // Measured from a rendered cell rather than assumed from the font size:
    // the actual advance width depends on the font the WebView resolved, and
    // guessing it puts every wrap in the wrong column.
    var measure = function () {
      var dims = term._core && term._core._renderService && term._core._renderService.dimensions;
      var cell = dims && dims.css && dims.css.cell;
      if (!cell || !cell.width || !cell.height) return null;
      var cols = Math.max(2, Math.floor(window.innerWidth / cell.width));
      var rows = Math.max(2, Math.floor(window.innerHeight / cell.height));
      return { cols: cols, rows: rows };
    };
    var applyFit = function () {
      var size = measure();
      if (!size) return;
      if (size.cols !== term.cols || size.rows !== term.rows) {
        term.resize(size.cols, size.rows);
      }
      post({ type: 'resize', cols: term.cols, rows: term.rows });
    };

    window.addEventListener('resize', applyFit);

    // Messages in: replay/output bytes, or a reset before a fresh snapshot.
    var handle = function (raw) {
      var msg = JSON.parse(raw);
      if (msg.type === 'write') {
        term.write(b64ToBytes(msg.data));
      } else if (msg.type === 'reset') {
        // A resync replaces the screen wholesale. Clearing first matters:
        // writing a replay snapshot on top of the old contents would leave the
        // pre-gap screen scrolled above it, which reads as real history.
        term.reset();
        if (msg.cols && msg.rows) term.resize(msg.cols, msg.rows);
      } else if (msg.type === 'focus') {
        term.focus();
      } else if (msg.type === 'fit') {
        applyFit();
      }
    };
    document.addEventListener('message', function (e) { handle(e.data); });
    window.addEventListener('message', function (e) { handle(e.data); });

    // Report the first fit once layout settles, so the app can size the PTY to
    // the phone rather than leaving it at the desktop's width.
    setTimeout(applyFit, 0);
    post({ type: 'ready' });
  })();
</script>
</body>
</html>`;
}
