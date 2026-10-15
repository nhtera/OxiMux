/**
 * The page a watched device is drawn in: WebCodecs' `VideoDecoder` onto a
 * canvas, fed by React Native one frame at a time.
 *
 * **It loads from `SIM_VIEW_BASE`, an https origin, not `about:blank`.**
 * Android's WebView exposes `VideoDecoder` only to a secure context, and an
 * inline page with no base URL is not one (checked on API 37 / WebView 155:
 * `about:blank` has no `VideoDecoder`, any https base does; iOS has it
 * either way). `.invalid` never resolves, so the origin reaches nothing; the
 * view still refuses every navigation.
 *
 * Frames cross as base64 (the bridge carries strings): kind 0 is the
 * decoder's config (an avcC record, WebCodecs' `description`), 1 a key frame,
 * 2 a delta. A delta before the first key frame after a config, or after the
 * decoder fell behind, is dropped: decoded against the wrong reference it
 * would show corruption until the next key frame. The host sends one only
 * when its side of the stream broke, so a page that falls behind (or whose
 * decoder fails) says so (`stuck`), and the view starts over.
 */
export const SIM_VIEW_BASE = 'https://sim-view.invalid/';

/** Deltas queued in the decoder past which the page waits for a key frame. */
export const MAX_DECODE_QUEUE = 4;

export function simPage(background: string): string {
  return `<!doctype html>
<html>
<head>
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<style>
  html, body { margin: 0; padding: 0; height: 100%; background: ${background}; overflow: hidden; }
  canvas { display: block; width: 100%; height: 100%; object-fit: contain; }
</style>
</head>
<body>
<canvas id="screen" width="2" height="2"></canvas>
<script>
(function () {
  var canvas = document.getElementById('screen');
  var ctx = canvas.getContext('2d');
  var decoder = null;
  var waitKey = true;
  var stuck = false;
  var ts = 0;
  var shown = 0;
  var lastSeq = -1;

  function post(msg) {
    if (window.ReactNativeWebView) window.ReactNativeWebView.postMessage(JSON.stringify(msg));
  }
  function bytes(b64) {
    var s = atob(b64), out = new Uint8Array(s.length);
    for (var i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  function hex(n) { return (n < 16 ? '0' : '') + n.toString(16).toUpperCase(); }
  function close() {
    if (decoder) { try { decoder.close(); } catch (e) {} }
    decoder = null;
    waitKey = true;
    stuck = false;
  }
  function draw(frame) {
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
      post({ type: 'size', width: frame.displayWidth, height: frame.displayHeight });
    }
    ctx.drawImage(frame, 0, 0);
    frame.close();
    if (++shown === 1) post({ type: 'first' });
  }
  function configure(avcc) {
    close();
    if (typeof VideoDecoder === 'undefined') { post({ type: 'unsupported' }); return; }
    decoder = new VideoDecoder({
      output: draw,
      error: function (e) { post({ type: 'error', message: String(e) }); close(); },
    });
    try {
      decoder.configure({
        codec: 'avc1.' + hex(avcc[1]) + hex(avcc[2]) + hex(avcc[3]),
        description: avcc,
        optimizeForLatency: true,
      });
    } catch (e) {
      post({ type: 'error', message: String(e) });
      close();
    }
  }
  function decode(kind, data) {
    if (!decoder || decoder.state !== 'configured') return;
    // A delta while waiting for a key frame: frames went missing on this
    // side, and the host does not know to send one.
    if (kind === 2 && (waitKey || decoder.decodeQueueSize > ${MAX_DECODE_QUEUE})) {
      if (!stuck) post({ type: 'stuck' });
      stuck = true;
      waitKey = true;
      return;
    }
    if (kind === 1) { waitKey = false; stuck = false; }
    try {
      decoder.decode(new EncodedVideoChunk({ type: kind === 1 ? 'key' : 'delta', timestamp: (ts++) * 66666, data: data }));
    } catch (e) {
      post({ type: 'error', message: String(e) });
      close();
    }
  }
  function receive(raw) {
    var msg;
    try { msg = JSON.parse(raw); } catch (e) { return; }
    // Some platforms deliver to both document and window: once only.
    if (typeof msg.seq === 'number') {
      if (msg.seq <= lastSeq) return;
      lastSeq = msg.seq;
    }
    if (msg.k === 0) configure(bytes(msg.b));
    else if (msg.k === 1 || msg.k === 2) decode(msg.k, bytes(msg.b));
  }
  document.addEventListener('message', function (e) { receive(e.data); });
  window.addEventListener('message', function (e) { receive(e.data); });
  post({ type: 'ready', webcodecs: typeof VideoDecoder !== 'undefined', secure: !!window.isSecureContext });
})();
</script>
</body>
</html>`;
}
